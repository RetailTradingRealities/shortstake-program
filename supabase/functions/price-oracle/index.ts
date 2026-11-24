// Custom price oracle: fetches a token's USD price from Birdeye, DexScreener and CoinGecko,
// combines them under the per-action rules in core.ts, and returns it signed by the oracle
// signer for the program's Ed25519 check (programs/short-stake001/src/signed_price.rs).
//
//   POST { "mint": "<base58>", "action": "deposit" | "open" | "close" | "liquidate" }
//   (or GET ?mint=...&action=...)
//
// 200 -> SignedPrice JSON (price in 10^-12 USD units, signature base64) - place it with
//        getSignedPriceInstruction directly before the priced instruction, within 60s.
// 422 -> the token failed the Rugcheck safety gate (deposit/open only; body lists reasons),
//        or not enough agreeing sources for this action (body says which were dropped/failed).
// 503 -> Rugcheck could not be reached (deposit/open are refused, never let through), or a
//        large price move is waiting to be confirmed; retry after Retry-After seconds.
//
// Secrets (see README.md): ORACLE_SIGNER_KEYPAIR, BIRDEYE_API_KEY, COINGECKO_API_KEY,
// RUGCHECK_API_KEY. Devnet only: DEVNET_PRICE_ALIASES (see devnetAliases below).

import { address, createKeyPairSignerFromBytes, getAddressEncoder, signBytes, type KeyPairSigner } from "@solana/kit";
import { createClient } from "@supabase/supabase-js";
import {
  PriceAction,
  aggregate,
  anchorPrice,
  applyJumpGuard,
  exitFallbackPrice,
  liquidationBackdateSeconds,
  publishTimeFor,
  signedPriceMessage,
  signingPrice,
  usdToPriceUnits,
  type MintPriceState,
  type SourceQuote,
} from "./core.ts";
import { checkTokenSafety, type TokenSafety } from "./rugcheck.ts";
import { fetchBirdeye, fetchCoinGecko, fetchDexScreener } from "./sources.ts";

// Same Edge-runtime WebCrypto workaround as helius-indexer: the esm.sh kit build checks
// isSecureContext, which this (server-side, TLS-terminated) runtime doesn't define.
(globalThis as { isSecureContext?: boolean }).isSecureContext = true;

const PROGRAM_ID = "sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3";
const CACHE_SECONDS = 5;
const TOKEN_CHECK_CACHE_SECONDS = 3_600;
/** One more round of data-feed requests before refusing for too few agreeing sources. */
const SOURCE_RETRY_DELAY_MS = 1_000;
/** Opening exposure is gated by Rugcheck; exits never are. */
const GATED_ACTIONS: ReadonlySet<PriceAction> = new Set([PriceAction.DepositStake, PriceAction.OpenBorrowPosition]);
const ACTIONS: Record<string, PriceAction> = {
  deposit: PriceAction.DepositStake,
  open: PriceAction.OpenBorrowPosition,
  close: PriceAction.ClosePosition,
  liquidate: PriceAction.LiquidatePosition,
};

const supabase = createClient(Deno.env.get("SUPABASE_URL")!, Deno.env.get("SUPABASE_SERVICE_ROLE_KEY")!, {
  auth: { persistSession: false },
});

let signer: Promise<KeyPairSigner> | null = null;
function oracleSigner(): Promise<KeyPairSigner> {
  const raw = Deno.env.get("ORACLE_SIGNER_KEYPAIR");
  if (!raw) throw new Error("ORACLE_SIGNER_KEYPAIR secret is not set");
  signer ??= createKeyPairSignerFromBytes(new Uint8Array(JSON.parse(raw)));
  return signer;
}

// Prices are public information; any origin may ask for one.
const CORS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Headers": "authorization, apikey, content-type, x-client-info",
  "Access-Control-Allow-Methods": "GET, POST, OPTIONS",
};
const json = (status: number, body: unknown, headers: Record<string, string> = {}) =>
  new Response(JSON.stringify(body), { status, headers: { ...CORS, "Content-Type": "application/json", ...headers } });

/**
 * DEVNET TESTING ONLY. "devnetMint=realMint,..." - a devnet test token has no market price, so
 * the oracle prices (and Rugcheck-gates) the real mainnet token it stands in for, and signs that
 * price for the devnet mint. Everything keyed by mint (cache, jump guard, the signed message)
 * still uses the requested mint. MUST be unset on the mainnet deployment.
 */
const devnetAliases = new Map(
  (Deno.env.get("DEVNET_PRICE_ALIASES") ?? "")
    .split(",")
    .map((pair) => pair.split("=").map((part) => part.trim()))
    .filter((pair): pair is [string, string] => pair.length === 2 && pair[0] !== "" && pair[1] !== ""),
);

const ACTION_NAMES = Object.fromEntries(Object.entries(ACTIONS).map(([name, a]) => [a, name])) as Record<PriceAction, string>;

/**
 * One JSON line per event, so the Supabase function logs can be filtered and counted
 * (search "price_oracle_event"). Refusals carry the reason and every dropped / failed feed.
 */
function logEvent(event: string, mint: string, action: PriceAction, detail: Record<string, unknown>) {
  console.warn(JSON.stringify({ price_oracle_event: event, mint, action: ACTION_NAMES[action], ...detail }));
}

function fetchQuotes(mint: string): Array<{ source: string; run: () => Promise<SourceQuote> }> {
  const key = (name: string) => {
    const v = Deno.env.get(name);
    if (!v) throw new Error(`${name} secret is not set`);
    return v;
  };
  return [
    { source: "birdeye", run: () => fetchBirdeye(mint, key("BIRDEYE_API_KEY")) },
    { source: "dexscreener", run: () => fetchDexScreener(mint) },
    { source: "coingecko", run: () => fetchCoinGecko(mint, key("COINGECKO_API_KEY")) },
  ];
}

const toBase64 = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes));

/** Ask every source at once; a failed one simply doesn't count. */
async function collectQuotes(mint: string): Promise<{ quotes: SourceQuote[]; failed: Array<{ source: string; error: string }> }> {
  const sources = fetchQuotes(mint);
  // Promise.resolve().then(...): a source that throws synchronously (e.g. its API key secret
  // is missing) counts as ONE failed source instead of failing every request (Slot Zero I-08).
  const settled = await Promise.allSettled(sources.map((s) => Promise.resolve().then(s.run)));
  const quotes: SourceQuote[] = [];
  const failed: Array<{ source: string; error: string }> = [];
  settled.forEach((r, i) => {
    if (r.status === "fulfilled") quotes.push(r.value);
    else failed.push({ source: sources[i].source, error: String((r.reason as Error)?.message ?? r.reason) });
  });
  return { quotes, failed };
}

/** Cached Rugcheck verdict if checked within the hour, otherwise a fresh check (stored). */
async function tokenSafety(mint: string): Promise<TokenSafety> {
  const cached = await supabase
    .from("oracle_token_checks")
    .select("result, checked_at")
    .eq("token_mint", mint)
    .maybeSingle();
  if (cached.data && Date.now() - Date.parse(cached.data.checked_at) < TOKEN_CHECK_CACHE_SECONDS * 1000) {
    return cached.data.result as TokenSafety;
  }
  const apiKey = Deno.env.get("RUGCHECK_API_KEY");
  if (!apiKey) throw new Error("RUGCHECK_API_KEY secret is not set");
  const result = await checkTokenSafety(mint, apiKey);
  const stored = await supabase
    .from("oracle_token_checks")
    .upsert({ token_mint: mint, eligible: result.eligible, result, checked_at: new Date().toISOString() });
  if (stored.error) console.warn(`price-oracle token check cache write failed: ${stored.error.message}`);
  return result;
}

Deno.serve(async (req) => {
  if (req.method === "OPTIONS") return new Response(null, { status: 204, headers: CORS });
  if (req.method !== "GET" && req.method !== "POST") return json(405, { error: "method not allowed" });

  let params: { mint?: unknown; action?: unknown };
  if (req.method === "POST") {
    try {
      params = await req.json();
    } catch {
      return json(400, { error: "body must be JSON: { mint, action }" });
    }
  } else {
    const q = new URL(req.url).searchParams;
    params = { mint: q.get("mint"), action: q.get("action") };
  }

  // Own keys only: a plain object lookup would also accept "constructor" etc. (Slot Zero L-05).
  const action = typeof params.action === "string" && Object.hasOwn(ACTIONS, params.action) ? ACTIONS[params.action] : undefined;
  if (action === undefined) return json(400, { error: `action must be one of ${Object.keys(ACTIONS).join(", ")}` });
  let mint: string;
  try {
    mint = address(String(params.mint));
  } catch {
    return json(400, { error: "mint must be a base58 token mint address" });
  }

  const priceMint = devnetAliases.get(mint) ?? mint;

  // 1. Same mint + action signed moments ago: hand out the same price.
  const cached = await supabase
    .from("oracle_signed_prices")
    .select("response, signed_at")
    .eq("token_mint", mint)
    .eq("action", action)
    .maybeSingle();
  if (cached.data && Date.now() - Date.parse(cached.data.signed_at) < CACHE_SECONDS * 1000) {
    return json(200, cached.data.response);
  }

  // 2. Deposit / open only: the token must pass the Rugcheck gate. Fail closed.
  let warnings: string[] = [];
  if (GATED_ACTIONS.has(action)) {
    let safety: TokenSafety;
    try {
      safety = await tokenSafety(priceMint);
    } catch (e) {
      logEvent("refused_token_check_unavailable", mint, action, { error: (e as Error).message });
      return json(503, { error: "unable to verify token safety, try again shortly", retryAfterSeconds: 30 }, { "Retry-After": "30" });
    }
    if (!safety.eligible) {
      logEvent("refused_token_unsafe", mint, action, { reasons: safety.reasons, warnings: safety.warnings });
      return json(422, { error: "token failed safety checks", reasons: safety.reasons, warnings: safety.warnings });
    }
    warnings = safety.warnings;
  }

  // 3. Ask every source. Too few agreeing is often momentary (a feed timing out, CoinGecko
  //    just over the age limit), so fetch once more before refusing.
  let { quotes, failed } = await collectQuotes(priceMint);
  let now = Math.floor(Date.now() / 1000);
  let agg = aggregate(quotes, action, now);
  if (!agg.ok) {
    logEvent("retrying_too_few_sources", mint, action, { reason: agg.reason, dropped: agg.dropped, failed });
    await new Promise((resolve) => setTimeout(resolve, SOURCE_RETRY_DELAY_MS));
    ({ quotes, failed } = await collectQuotes(priceMint));
    now = Math.floor(Date.now() / 1000);
    agg = aggregate(quotes, action, now);
    if (agg.ok) logEvent("recovered_on_retry", mint, action, { sources: agg.used });
  }
  // Per-mint jump guard memory (also what the exit fallback falls back on).
  const stateRow = await supabase.from("oracle_price_state").select("*").eq("token_mint", mint).maybeSingle();
  if (stateRow.error) return json(500, { error: "state read failed" });
  const state: MintPriceState = {
    lastPriceUsd: stateRow.data?.last_price_usd ?? null,
    lastAt: stateRow.data?.last_at ?? null,
    pendingPriceUsd: stateRow.data?.pending_price_usd ?? null,
    pendingSince: stateRow.data?.pending_since ?? null,
    history: Array.isArray(stateRow.data?.price_history) ? stateRow.data.price_history : undefined,
  };

  if (!agg.ok) {
    // Exits must not dead-end on an illiquid token (Slot Zero L-01): sign a fallback built from
    // the last accepted price and the raw quotes - the higher for close, the lower for liquidate.
    // It doesn't update the jump guard state.
    const fallback = exitFallbackPrice(action, state, quotes);
    if (fallback === null) {
      logEvent("refused_too_few_sources", mint, action, { reason: agg.reason, dropped: agg.dropped, failed });
      return json(422, { error: agg.reason, dropped: agg.dropped, failed });
    }
    logEvent("priced_exit_fallback", mint, action, { reason: agg.reason, priceUsd: fallback, lastPriceUsd: state.lastPriceUsd, dropped: agg.dropped, failed });
    return signAndRespond(mint, action, fallback, now, { sources: [], dropped: agg.dropped, warnings, fallback: true });
  }
  // Priced, but not every feed counted - the data for deciding whether feeds need fixing.
  if (agg.dropped.length || failed.length) {
    logEvent("priced_with_feeds_missing", mint, action, { sources: agg.used, dropped: agg.dropped, failed });
  }

  // 4. Jump guard against the time-anchored reference for this mint (core.ts, Slot Zero M-01).
  const decision = applyJumpGuard(state, agg.priceUsd, now);
  const saved = await supabase.from("oracle_price_state").upsert({
    token_mint: mint,
    last_price_usd: decision.next.lastPriceUsd,
    last_at: decision.next.lastAt,
    pending_price_usd: decision.next.pendingPriceUsd,
    pending_since: decision.next.pendingSince,
    price_history: decision.next.history ?? [],
    updated_at: new Date().toISOString(),
  });
  if (saved.error) return json(500, { error: "state write failed" });
  if (!decision.accept) {
    logEvent("refused_jump_pending", mint, action, {
      priceUsd: agg.priceUsd,
      lastPriceUsd: state.lastPriceUsd,
      retryAfterSeconds: decision.retryAfterSeconds,
    });
    return json(
      503,
      { error: "large price move awaiting confirmation", retryAfterSeconds: decision.retryAfterSeconds },
      { "Retry-After": String(decision.retryAfterSeconds) },
    );
  }

  // 5. Liquidations sign the lower of the price and the anchor (the latest price at least a
  //    minute old), opens the higher (core.ts signingPrice, Slot Zero M-01).
  const anchor = anchorPrice(decision.next, now);
  const signedUsd = signingPrice(action, agg.priceUsd, anchor);
  if (signedUsd !== agg.priceUsd) {
    logEvent("priced_at_anchor", mint, action, { priceUsd: agg.priceUsd, anchorUsd: anchor, signedUsd });
  }
  return signAndRespond(mint, action, signedUsd, now, { sources: agg.used, dropped: agg.dropped, warnings });
});

/** Sign exactly what signed_price.rs verifies, cache it for CACHE_SECONDS, and respond. */
async function signAndRespond(
  mint: string,
  action: PriceAction,
  priceUsd: number,
  now: number,
  extra: { sources: string[]; dropped: unknown; warnings: string[]; fallback?: boolean },
): Promise<Response> {
  let units: bigint;
  try {
    units = usdToPriceUnits(priceUsd);
  } catch (e) {
    logEvent("refused_price_out_of_range", mint, action, { priceUsd });
    return json(422, { error: (e as Error).message });
  }
  const oracle = await oracleSigner();
  const enc = getAddressEncoder();
  // Liquidation prices are backdated so the program accepts them for only
  // LIQUIDATION_PRICE_VALIDITY_SECONDS (core.ts); LIQUIDATION_PRICE_BACKDATE_SECONDS=0 once the
  // program checks 15 s itself. The cache below still keys on the real time.
  const backdate = liquidationBackdateSeconds(Deno.env.get("LIQUIDATION_PRICE_BACKDATE_SECONDS"));
  const publishTime = BigInt(publishTimeFor(action, now, backdate));
  const message = signedPriceMessage(
    enc.encode(address(PROGRAM_ID)) as Uint8Array,
    enc.encode(address(mint)) as Uint8Array,
    units,
    publishTime,
    action,
  );
  const signature = await signBytes(oracle.keyPair.privateKey, message);

  const response = {
    tokenMint: mint,
    priceUsd: units.toString(),
    publishTime: publishTime.toString(),
    action,
    signer: oracle.address,
    signature: toBase64(signature),
    sources: extra.sources,
    dropped: extra.dropped,
    /** Token-safety notes for the UI (token program, authorities) - never a rejection. */
    warnings: extra.warnings,
    /** True when no source survived and this is the exit fallback (close / liquidate only). */
    ...(extra.fallback ? { fallback: true } : {}),
  };
  const stored = await supabase
    .from("oracle_signed_prices")
    .upsert({ token_mint: mint, action, response, signed_at: new Date(now * 1000).toISOString() });
  if (stored.error) console.warn(`price-oracle cache write failed: ${stored.error.message}`);
  return json(200, response);
}
