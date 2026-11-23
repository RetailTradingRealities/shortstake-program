// Runtime-agnostic core of the custom price oracle: combines per-source USD quotes into
// one price, applies the manipulation guards, and builds the exact bytes the oracle
// signer signs. No Deno, Supabase or network APIs here so the logic is unit-testable
// from Node (scripts/test-price-oracle-core.mts). index.ts wires this to the data-feed
// APIs, the database and the signer.

export type SourceName = "birdeye" | "dexscreener" | "coingecko";

// Mirrors programs/short-stake001/src/signed_price.rs PriceAction (and
// app/src/oracle/signedPrice.ts). The program rejects a price signed for another action.
export const PriceAction = {
  DepositStake: 1,
  OpenBorrowPosition: 2,
  ClosePosition: 3,
  LiquidatePosition: 4,
} as const;
export type PriceAction = (typeof PriceAction)[keyof typeof PriceAction];

// How many independent sources must survive filtering before a price is signed.
// Opening exposure needs agreement; exiting (close / liquidate) must keep working
// through a partial data-feed outage, so one source is enough there.
export const MIN_SOURCES: Record<PriceAction, number> = {
  [PriceAction.DepositStake]: 2,
  [PriceAction.OpenBorrowPosition]: 2,
  [PriceAction.ClosePosition]: 1,
  [PriceAction.LiquidatePosition]: 1,
};

export const GUARDS = {
  /** A source further than this from the median of all sources is an outlier. */
  outlierBps: 500,
  /** Ignore a source whose pool liquidity is below this (thin pools are cheap to move). */
  minLiquidityUsd: 25_000,
  /** Ignore a source whose own last-update timestamp is older than this. */
  maxSourceAgeSeconds: 120,
  /** A move larger than this from the last accepted price is held back... */
  maxJumpBps: 1_500,
  /** ...until the new level has persisted this long (stops one-block spikes). */
  jumpConfirmSeconds: 60,
  /** While a jump is pending, later readings must stay within this of it to count. */
  pendingToleranceBps: 500,
  /**
   * The ANCHOR is the latest accepted price at least this old (Slot Zero M-01). The jump guard
   * measures against it, not against the last accepted price, so 15% steps a few seconds apart
   * can't ratchet the price up. Liquidations sign the lower of the current price and the anchor,
   * and opens the higher, so a pool push must hold for over a minute to move either one.
   */
  anchorSeconds: 60,
  /** How much accepted-price history to keep (plus the newest entry older than this). */
  historySeconds: 600,
} as const;

/** How long the program accepts any signed price (signed_price.rs MAX_SIGNED_PRICE_AGE_SECONDS). */
export const PROGRAM_MAX_PRICE_AGE_SECONDS = 60;
/**
 * How long a LIQUIDATION price stays usable on-chain. A liquidate signature names no position, so
 * for its whole validity anyone can use it on any position in the mint: in a falling market a
 * liquidator could keep the highest one and use it after the drop, for both the trigger and the
 * payout (reported by KupiaSec, 2026-11-23). Until the program checks a shorter age itself, the
 * oracle backdates liquidation publish times so the program's 60 s limit leaves only this much.
 */
export const LIQUIDATION_PRICE_VALIDITY_SECONDS = 15;

/** The publish_time to sign: now, or backdated for liquidations (see above). */
export function publishTimeFor(action: PriceAction, nowSeconds: number): number {
  return action === PriceAction.LiquidatePosition
    ? nowSeconds - (PROGRAM_MAX_PRICE_AGE_SECONDS - LIQUIDATION_PRICE_VALIDITY_SECONDS)
    : nowSeconds;
}

export interface SourceQuote {
  source: SourceName;
  priceUsd: number;
  /** Liquidity of the pool the price came from; null when the API doesn't report it. */
  liquidityUsd: number | null;
  /** Unix seconds the source last updated the price; null when the API doesn't say. */
  observedAt: number | null;
}

export type DroppedReason = "invalid" | "stale" | "illiquid" | "outlier" | "disagreement";

export type AggregateResult =
  | { ok: true; priceUsd: number; used: SourceName[]; dropped: Array<{ source: SourceName; reason: DroppedReason }> }
  | { ok: false; reason: string; dropped: Array<{ source: SourceName; reason: DroppedReason }> };

const relDiffBps = (a: number, b: number) => (Math.abs(a - b) / b) * 10_000;

function median(values: number[]): number {
  const s = [...values].sort((a, b) => a - b);
  const mid = Math.floor(s.length / 2);
  return s.length % 2 ? s[mid] : (s[mid - 1] + s[mid]) / 2;
}

/**
 * Drops invalid / stale / illiquid quotes and outliers, then returns the median of what
 * is left (with two survivors that is their average). Two sources that disagree by more
 * than the outlier band are both dropped: with only two there is no way to tell which
 * one is wrong.
 */
export function aggregate(quotes: SourceQuote[], action: PriceAction, nowSeconds: number): AggregateResult {
  // An unknown action has no minimum: refuse rather than "need undefined" (Slot Zero L-05).
  if (!Object.hasOwn(MIN_SOURCES, action)) return { ok: false, reason: "unknown action", dropped: [] };
  const dropped: Array<{ source: SourceName; reason: DroppedReason }> = [];
  let valid = quotes.filter((q) => {
    let reason: DroppedReason | null = null;
    if (!Number.isFinite(q.priceUsd) || q.priceUsd <= 0) reason = "invalid";
    else if (q.observedAt !== null && nowSeconds - q.observedAt > GUARDS.maxSourceAgeSeconds) reason = "stale";
    else if (q.liquidityUsd !== null && q.liquidityUsd < GUARDS.minLiquidityUsd) reason = "illiquid";
    if (reason) dropped.push({ source: q.source, reason });
    return reason === null;
  });

  if (valid.length === 2) {
    const [a, b] = valid;
    const [lo, hi] = [Math.min(a.priceUsd, b.priceUsd), Math.max(a.priceUsd, b.priceUsd)];
    if (relDiffBps(hi, lo) > GUARDS.outlierBps) {
      dropped.push({ source: a.source, reason: "disagreement" }, { source: b.source, reason: "disagreement" });
      valid = [];
    }
  } else if (valid.length > 2) {
    const m = median(valid.map((q) => q.priceUsd));
    valid = valid.filter((q) => {
      const outlier = relDiffBps(q.priceUsd, m) > GUARDS.outlierBps;
      if (outlier) dropped.push({ source: q.source, reason: "outlier" });
      return !outlier;
    });
  }

  const needed = MIN_SOURCES[action];
  if (valid.length < needed || valid.length === 0) {
    return { ok: false, reason: `needs ${needed} agreeing source(s), have ${valid.length}`, dropped };
  }
  return { ok: true, priceUsd: median(valid.map((q) => q.priceUsd)), used: valid.map((q) => q.source), dropped };
}

/** An accepted price and when (unix seconds). */
export interface AcceptedPrice {
  p: number;
  at: number;
  /** Accepted after the jump guard's hold: it already persisted jumpConfirmSeconds. */
  held?: boolean;
}

/** Per-mint memory the jump guard needs between requests (kept in Postgres by index.ts). */
export interface MintPriceState {
  lastPriceUsd: number | null;
  lastAt: number | null;
  pendingPriceUsd: number | null;
  pendingSince: number | null;
  /** Accepted prices, oldest first (see GUARDS.historySeconds). Missing on rows from before M-01. */
  history?: AcceptedPrice[];
}

/** The state's accepted prices, falling back to the single last price on pre-M-01 rows. */
function historyOf(state: MintPriceState): AcceptedPrice[] {
  if (state.history && state.history.length > 0) return state.history;
  return state.lastPriceUsd !== null && state.lastAt !== null ? [{ p: state.lastPriceUsd, at: state.lastAt }] : [];
}

/**
 * The latest accepted price that is at least GUARDS.anchorSeconds old, or that was accepted
 * after the jump guard's hold (it already persisted that long). Null if there is none yet.
 */
export function anchorPrice(state: MintPriceState, nowSeconds: number): number | null {
  const old = historyOf(state).filter((h) => h.at <= nowSeconds - GUARDS.anchorSeconds || h.held === true);
  return old.length > 0 ? old[old.length - 1].p : null;
}

/**
 * What the jump guard compares against: the anchor; while there is none (a mint first priced
 * under a minute ago), the OLDEST accepted price, so the first minute can't be ratcheted either.
 */
function jumpReference(state: MintPriceState, nowSeconds: number): number | null {
  return anchorPrice(state, nowSeconds) ?? historyOf(state)[0]?.p ?? null;
}

/** History plus a newly accepted price, pruned to historySeconds (keeping one older entry). */
function withAccepted(state: MintPriceState, priceUsd: number, nowSeconds: number, held = false): AcceptedPrice[] {
  const all = [...historyOf(state), { p: priceUsd, at: nowSeconds, ...(held ? { held: true } : {}) }];
  const cutoff = nowSeconds - GUARDS.historySeconds;
  const lastOld = all.filter((h) => h.at < cutoff).pop();
  return [...(lastOld ? [lastOld] : []), ...all.filter((h) => h.at >= cutoff)];
}

/**
 * The price to sign once the jump guard has accepted `priceUsd` (Slot Zero M-01). Liquidations
 * take the lower of it and the anchor, so a pump has to outlast the anchor window; opens take
 * the higher, so a dump can't let a borrower post less collateral. Deposits and closes sign the
 * current price.
 */
export function signingPrice(action: PriceAction, priceUsd: number, anchor: number | null): number {
  if (anchor === null) return priceUsd;
  if (action === PriceAction.LiquidatePosition) return Math.min(priceUsd, anchor);
  if (action === PriceAction.OpenBorrowPosition) return Math.max(priceUsd, anchor);
  return priceUsd;
}

/**
 * Exit fallback when NO source survives filtering (Slot Zero L-01): every pool under the
 * liquidity floor, or two sources disagreeing. Without it a position on an illiquid token could
 * neither be closed nor liquidated. Built from the last accepted price and any raw quotes (even
 * dropped ones):
 * - close: the HIGHER - the close price only sets interest, so this costs the borrower little;
 * - liquidate: the LOWER - a thin pool can't be pushed up to trigger a liquidation through it.
 * Null for other actions, or when there is nothing at all to go on.
 */
export function exitFallbackPrice(action: PriceAction, state: MintPriceState, rawQuotes: SourceQuote[]): number | null {
  if (action !== PriceAction.ClosePosition && action !== PriceAction.LiquidatePosition) return null;
  const candidates = rawQuotes.map((q) => q.priceUsd).filter((p) => Number.isFinite(p) && p > 0);
  if (state.lastPriceUsd !== null && Number.isFinite(state.lastPriceUsd) && state.lastPriceUsd > 0) {
    candidates.push(state.lastPriceUsd);
  }
  if (candidates.length === 0) return null;
  return action === PriceAction.ClosePosition ? Math.max(...candidates) : Math.min(...candidates);
}

export type JumpDecision =
  | { accept: true; next: MintPriceState }
  | { accept: false; next: MintPriceState; retryAfterSeconds: number };

/**
 * Accepts a price within maxJumpBps of the reference (the anchor, see GUARDS.anchorSeconds) at once. A bigger move
 * is only accepted once readings have stayed near the new level for jumpConfirmSeconds,
 * so a pool pumped for a few blocks can't be used to liquidate shorts (or open against
 * a crashed price). A genuine, sustained move still gets through after the delay.
 */
export function applyJumpGuard(state: MintPriceState, priceUsd: number, nowSeconds: number): JumpDecision {
  // No reading (e.g. a NaN median) must never touch the state (Slot Zero L-05).
  if (!Number.isFinite(priceUsd) || priceUsd <= 0) {
    return { accept: false, next: state, retryAfterSeconds: GUARDS.jumpConfirmSeconds };
  }
  const accepted: MintPriceState = {
    lastPriceUsd: priceUsd,
    lastAt: nowSeconds,
    pendingPriceUsd: null,
    pendingSince: null,
    history: withAccepted(state, priceUsd, nowSeconds),
  };
  // Measured against a time-anchored reference, not the last accepted price (Slot Zero M-01).
  const reference = jumpReference(state, nowSeconds);
  if (reference === null || relDiffBps(priceUsd, reference) <= GUARDS.maxJumpBps) {
    return { accept: true, next: accepted };
  }

  const continuesPending =
    state.pendingPriceUsd !== null &&
    state.pendingSince !== null &&
    relDiffBps(priceUsd, state.pendingPriceUsd) <= GUARDS.pendingToleranceBps;
  if (!continuesPending) {
    return {
      accept: false,
      next: { ...state, pendingPriceUsd: priceUsd, pendingSince: nowSeconds },
      retryAfterSeconds: GUARDS.jumpConfirmSeconds,
    };
  }

  const heldFor = nowSeconds - state.pendingSince!;
  if (heldFor >= GUARDS.jumpConfirmSeconds) {
    return { accept: true, next: { ...accepted, history: withAccepted(state, priceUsd, nowSeconds, true) } };
  }
  return { accept: false, next: state, retryAfterSeconds: GUARDS.jumpConfirmSeconds - heldFor };
}

// The program's price unit: integer 10^-12 USD per whole token (constants.rs PRICE_DECIMALS).
export const PRICE_DECIMALS = 12;
const U64_MAX = (1n << 64n) - 1n;

/** USD float -> on-chain price units, via the decimal string so no float rounding creeps in. */
export function usdToPriceUnits(priceUsd: number): bigint {
  const [mantissa, expStr] = priceUsd.toExponential(14).split("e");
  const digits = BigInt(mantissa.replace(".", "").replace("-", ""));
  const shift = Number(expStr) - 14 + PRICE_DECIMALS;
  const units = shift >= 0 ? digits * 10n ** BigInt(shift) : digits / 10n ** BigInt(-shift);
  if (units <= 0n || units > U64_MAX) throw new Error(`price ${priceUsd} USD is outside the on-chain range`);
  return units;
}

/**
 * program_id || token_mint || price (u64 LE) || publish_time (i64 LE) || action (u8).
 * Same 81 bytes as signed_price.rs and app/src/oracle/signedPrice.ts - the Node test
 * asserts this matches the app's encoder byte for byte.
 */
export function signedPriceMessage(
  programId: Uint8Array,
  tokenMint: Uint8Array,
  priceUnits: bigint,
  publishTime: bigint,
  action: PriceAction,
): Uint8Array {
  const message = new Uint8Array(81);
  const view = new DataView(message.buffer);
  message.set(programId, 0);
  message.set(tokenMint, 32);
  view.setBigUint64(64, priceUnits, true);
  view.setBigInt64(72, publishTime, true);
  message[80] = action;
  return message;
}
