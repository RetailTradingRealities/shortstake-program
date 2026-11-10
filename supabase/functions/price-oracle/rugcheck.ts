// Rugcheck token safety gate for opening exposure (deposit / open). index.ts runs it before
// signing a deposit or open price for a custom-oracle token, so a token that fails it can't
// get a market or new borrows: without a signed price the program rejects the instruction.
// Pyth-listed tokens never reach the oracle and are therefore never checked, by design.
// Exits (close / liquidate) are never gated - they must keep working if a token's report
// worsens or Rugcheck is down.
//
// `evaluateRugcheck` is pure (unit-tested from Node with recorded responses);
// `checkTokenSafety` adds the API call.

import type { FetchFn } from "./sources.ts";

export const SPL_TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";

export const RUGCHECK_RULES = {
  /** score_normalised runs 0..100, higher = riskier (WIF/Fartcoin score 1, PNUT 38). */
  maxScoreNormalised: 50,
  /** Share of pool liquidity (USD) that is locked or burned, across all of the report's markets. */
  minLpLockedPct: 20,
  /**
   * Total pool liquidity (USD, report.totalMarketLiquidity) at or above which a token passes
   * without locked LP. Locked LP guards against a young token's creator pulling its pool; big
   * established tokens (BONK 16%, JUP 1%, TRUMP 0.02% locked) trade in unlocked pools but are
   * deep enough to liquidate into.
   */
  minLiquidityUsdUnlocked: 1_000_000,
} as const;

/**
 * refresh=true makes Rugcheck rescan: 4-30s for large tokens, and over 30s (JUP, WIF, TRUMP)
 * when 15 ran at once. Timeouts are not retried, so a check takes at most ~50s.
 */
const TIMEOUT_MS = 45_000;
/**
 * Waits before each retry after HTTP 429. Rescans are rate-limited: 14 at once got 4 x 429,
 * which then succeeded one at a time. A Retry-After of at most MAX_RETRY_AFTER_MS replaces the
 * wait; a longer one is not honoured (the caller would rather fail closed with 503 than hang).
 */
const RETRY_DELAYS_MS = [1_000, 3_000];
const MAX_RETRY_AFTER_MS = 5_000;

export type SleepFn = (ms: number) => Promise<void>;
const sleep: SleepFn = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export interface TokenSafety {
  eligible: boolean;
  /** Why the token is rejected - empty when eligible. */
  reasons: string[];
  /** Shown to the user but never rejecting: token program and authorities. */
  warnings: string[];
  scoreNormalised: number | null;
  lpLockedPct: number | null;
  /** report.totalMarketLiquidity, USD. */
  totalLiquidityUsd: number | null;
  tokenProgram: string | null;
}

const isNum = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

// Loosely typed: only the fields read here, and every one is checked before use.
interface RugcheckReport {
  token?: { mintAuthority?: string | null; freezeAuthority?: string | null } | null;
  tokenProgram?: unknown;
  score_normalised?: unknown;
  rugged?: unknown;
  totalMarketLiquidity?: unknown;
  risks?: Array<{ name?: string; level?: string }> | unknown;
  markets?: Array<{ lp?: { lpLockedUSD?: unknown; quoteUSD?: unknown; baseUSD?: unknown } | null }> | unknown;
}

/**
 * Locked share of all pool liquidity, in percent: sum(lpLockedUSD) / sum(quoteUSD + baseUSD).
 * This is how Rugcheck computes /report/summary's lpLockedPct (it matched to ~1e-13 on POPCAT,
 * GOAT, WIF, BONK, JUP, PNUT, MEW, MOODENG and CHILLGUY), but taken from the rescanned report
 * so it is fresh - the summary is cached, and a refreshed summary wrongly reports 0.
 * Null (fails closed) when there are no markets or no priced liquidity.
 */
export function lpLockedPctFromMarkets(markets: unknown): number | null {
  if (!Array.isArray(markets)) return null;
  let locked = 0;
  let total = 0;
  for (const market of markets) {
    const lp = market?.lp;
    if (!lp) continue;
    if (isNum(lp.quoteUSD)) total += lp.quoteUSD;
    if (isNum(lp.baseUSD)) total += lp.baseUSD;
    if (isNum(lp.lpLockedUSD)) locked += lp.lpLockedUSD;
  }
  return total > 0 ? (locked / total) * 100 : null;
}

/** Applies the rules to a /report response. Missing data fails closed. */
export function evaluateRugcheck(report: RugcheckReport): TokenSafety {
  const reasons: string[] = [];
  const warnings: string[] = [];
  const token = report.token;

  if (!token) reasons.push("Rugcheck report has no token data");

  if (report.rugged !== false) {
    reasons.push(report.rugged === true ? "Rugcheck marks this token as rugged" : "Rugged status unavailable");
  }

  const score = isNum(report.score_normalised) ? report.score_normalised : null;
  if (score === null) reasons.push("Missing or invalid risk score");
  else if (score > RUGCHECK_RULES.maxScoreNormalised) {
    reasons.push(`Risk score ${score} is above ${RUGCHECK_RULES.maxScoreNormalised}`);
  }

  // Liquidity rule: enough locked LP, or deep enough pools that locking doesn't matter.
  const lpLockedPct = lpLockedPctFromMarkets(report.markets);
  const totalLiquidityUsd = isNum(report.totalMarketLiquidity) ? report.totalMarketLiquidity : null;
  const deep = totalLiquidityUsd !== null && totalLiquidityUsd >= RUGCHECK_RULES.minLiquidityUsdUnlocked;
  const locked = lpLockedPct !== null && lpLockedPct >= RUGCHECK_RULES.minLpLockedPct;
  if (!deep && !locked) {
    const lockedText = lpLockedPct === null
      ? "Locked liquidity unavailable"
      : `Only ${lpLockedPct.toFixed(1)}% of liquidity is locked (minimum ${RUGCHECK_RULES.minLpLockedPct}%)`;
    const depthText = totalLiquidityUsd === null
      ? "total liquidity unavailable"
      : `total liquidity $${Math.round(totalLiquidityUsd).toLocaleString("en-US")} is below $${RUGCHECK_RULES.minLiquidityUsdUnlocked.toLocaleString("en-US")}`;
    reasons.push(`${lockedText}, and ${depthText}`);
  }

  if (!Array.isArray(report.risks)) reasons.push("Risk findings unavailable");
  else {
    for (const risk of report.risks) {
      if (risk?.level === "danger") reasons.push(`Danger: ${risk.name ?? "unspecified risk"}`);
    }
  }

  // Warnings only. The program itself rejects non-SPL-Token mints at deposit time.
  const tokenProgram = typeof report.tokenProgram === "string" ? report.tokenProgram : null;
  if (tokenProgram !== SPL_TOKEN_PROGRAM) {
    warnings.push(tokenProgram ? `Unsupported token program ${tokenProgram}` : "Token program unknown");
  }
  if (token) {
    if (!("freezeAuthority" in token)) warnings.push("Freeze authority status unavailable");
    else if (token.freezeAuthority !== null) warnings.push("Freeze authority is enabled");
    if (!("mintAuthority" in token)) warnings.push("Mint authority status unavailable");
    else if (token.mintAuthority !== null) warnings.push("Mint authority is enabled");
  }

  return { eligible: reasons.length === 0, reasons, warnings, scoreNormalised: score, lpLockedPct, totalLiquidityUsd, tokenProgram };
}

/**
 * Fetches a freshly rescanned report (refresh=true, needs a paid key) and evaluates it.
 * Retries HTTP 429 (see RETRY_DELAYS_MS); throws on any other failure or once retries run out.
 * /report/summary is not used: refreshed it reports lpLockedPct = 0 (POPCAT 93% -> 0), and
 * plain it is cached, so lpLockedPct is derived from the report itself.
 */
export async function checkTokenSafety(
  tokenMint: string,
  apiKey: string,
  fetchFn: FetchFn = fetch,
  sleepFn: SleepFn = sleep,
): Promise<TokenSafety> {
  const url = new URL(`https://api.rugcheck.xyz/v1/tokens/${encodeURIComponent(tokenMint)}/report`);
  url.searchParams.set("refresh", "true");
  let response: Response;
  for (let attempt = 0; ; attempt++) {
    response = await fetchFn(url, {
      headers: { accept: "application/json", "X-API-KEY": apiKey },
      signal: AbortSignal.timeout(TIMEOUT_MS),
    });
    if (response.status !== 429 || attempt >= RETRY_DELAYS_MS.length) break;
    await response.body?.cancel();
    const retryAfterMs = Number(response.headers.get("retry-after")) * 1000;
    const honoured = Number.isFinite(retryAfterMs) && retryAfterMs > 0 && retryAfterMs <= MAX_RETRY_AFTER_MS;
    await sleepFn(honoured ? retryAfterMs : RETRY_DELAYS_MS[attempt]);
  }
  if (!response.ok) throw new Error(`rugcheck report: HTTP ${response.status}`);
  return evaluateRugcheck(await response.json() as RugcheckReport);
}
