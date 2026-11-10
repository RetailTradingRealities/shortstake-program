// Data-feed adapters: one function per provider, each turning that provider's USD price
// response into a SourceQuote for core.ts. They throw on any failure (HTTP error, missing
// or malformed field, unsupported token) - index.ts runs them with Promise.allSettled and
// a failed source simply doesn't count towards the minimum. `fetchFn` is injectable so
// scripts/test-price-oracle-core.mts can feed recorded responses.

import type { SourceQuote } from "./core.ts";

export type FetchFn = (input: string | URL, init?: RequestInit) => Promise<Response>;

/** Per-request budget; a slow provider must not hold up the whole signed price. */
const TIMEOUT_MS = 4_000;

async function getJson(fetchFn: FetchFn, url: URL, headers: Record<string, string>): Promise<unknown> {
  const response = await fetchFn(url, { headers, signal: AbortSignal.timeout(TIMEOUT_MS) });
  if (!response.ok) throw new Error(`HTTP ${response.status} from ${url.host}`);
  return response.json();
}

const isNum = (v: unknown): v is number => typeof v === "number" && Number.isFinite(v);

// Birdeye GET /defi/price - price, its own update time, and liquidity in one call.
// https://public-api.birdeye.so/defi/price?address=<mint>  headers: X-API-KEY, x-chain
export async function fetchBirdeye(tokenMint: string, apiKey: string, fetchFn: FetchFn = fetch): Promise<SourceQuote> {
  const url = new URL("https://public-api.birdeye.so/defi/price");
  url.searchParams.set("address", tokenMint);
  url.searchParams.set("include_liquidity", "true");
  const body = (await getJson(fetchFn, url, { "X-API-KEY": apiKey, "x-chain": "solana" })) as {
    success?: boolean;
    data?: { value?: unknown; updateUnixTime?: unknown; liquidity?: unknown; isScaledUiToken?: unknown } | null;
  };
  const data = body.success ? body.data : null;
  if (!data || !isNum(data.value)) throw new Error("birdeye: no price for this mint");
  // Scaled-UI-amount (Token-2022) prices are per *displayed* token, not per whole base-unit
  // token the program prices against - the multiplier can change, so don't guess.
  if (data.isScaledUiToken === true) throw new Error("birdeye: scaled-UI-amount token not supported");
  return {
    source: "birdeye",
    priceUsd: data.value,
    liquidityUsd: isNum(data.liquidity) ? data.liquidity : null,
    observedAt: isNum(data.updateUnixTime) ? data.updateUnixTime : null,
  };
}

// CoinGecko is ONE source, from one of two endpoints - never both counted:
//  1. GET /simple/token_price/solana - CoinGecko's aggregated price (CEX + DEX), 13-22s old in
//     testing, but only for tokens CoinGecko lists. No liquidity figure, so this quote skips the
//     liquidity floor (the outlier check against the other feeds still applies).
//  2. For unlisted tokens (most new memecoins), the on-chain endpoint below - often minutes old.
// A mint found unlisted is remembered for an hour (per function instance), so those requests
// go straight to the on-chain endpoint: one CoinGecko call per price either way.
const COINGECKO_UNLISTED_TTL_MS = 3_600_000;
const coingeckoUnlistedUntil = new Map<string, number>();

/** Test hook: forget which mints were found unlisted. */
export function resetCoinGeckoListingCache(): void {
  coingeckoUnlistedUntil.clear();
}

export async function fetchCoinGecko(tokenMint: string, apiKey: string, fetchFn: FetchFn = fetch): Promise<SourceQuote> {
  if ((coingeckoUnlistedUntil.get(tokenMint) ?? 0) < Date.now()) {
    let listed: SourceQuote | null = null;
    try {
      listed = await fetchCoinGeckoListed(tokenMint, apiKey, fetchFn);
      if (!listed) coingeckoUnlistedUntil.set(tokenMint, Date.now() + COINGECKO_UNLISTED_TTL_MS);
    } catch (e) {
      // Endpoint error (rate limit, outage): try the on-chain price, but don't mark unlisted.
      console.warn(`coingecko simple/token_price failed for ${tokenMint}, using on-chain price: ${(e as Error).message}`);
    }
    if (listed) return listed;
  }
  return fetchCoinGeckoOnchain(tokenMint, apiKey, fetchFn);
}

/** The aggregated price, or null when CoinGecko doesn't list this mint. Throws on HTTP errors. */
async function fetchCoinGeckoListed(tokenMint: string, apiKey: string, fetchFn: FetchFn): Promise<SourceQuote | null> {
  const url = new URL("https://pro-api.coingecko.com/api/v3/simple/token_price/solana");
  url.searchParams.set("contract_addresses", tokenMint);
  url.searchParams.set("vs_currencies", "usd");
  url.searchParams.set("include_last_updated_at", "true");
  url.searchParams.set("precision", "full");
  const body = (await getJson(fetchFn, url, { "x-cg-pro-api-key": apiKey })) as Record<
    string,
    { usd?: unknown; last_updated_at?: unknown } | undefined
  >;
  const key = Object.keys(body ?? {}).find((k) => k.toLowerCase() === tokenMint.toLowerCase());
  const entry = key === undefined ? undefined : body[key];
  if (!entry || !isNum(entry.usd) || entry.usd <= 0) return null;
  return {
    source: "coingecko",
    priceUsd: entry.usd,
    liquidityUsd: null,
    observedAt: isNum(entry.last_updated_at) ? entry.last_updated_at : null,
  };
}

// CoinGecko on-chain (GeckoTerminal) GET /onchain/simple/networks/solana/token_price/<mint>.
// All values are strings keyed by token address. total_reserve_in_usd (the token's liquidity
// across all its pools) is only returned when asked for, and last_trade_timestamp only with
// include_inactive_source=true. That flag also lets an inactive pool's last swap price
// through - which the age check in core.ts then drops, since its timestamp is old.
async function fetchCoinGeckoOnchain(tokenMint: string, apiKey: string, fetchFn: FetchFn): Promise<SourceQuote> {
  const url = new URL(`https://pro-api.coingecko.com/api/v3/onchain/simple/networks/solana/token_price/${tokenMint}`);
  url.searchParams.set("include_total_reserve_in_usd", "true");
  url.searchParams.set("include_inactive_source", "true");
  const body = (await getJson(fetchFn, url, { "x-cg-pro-api-key": apiKey })) as {
    data?: { attributes?: Record<string, Record<string, unknown> | undefined> };
  };
  const attributes = body.data?.attributes ?? {};
  // Keyed by address; look up case-insensitively in case the API normalizes the key.
  const field = (name: string): unknown => {
    const map = attributes[name] ?? {};
    const key = Object.keys(map).find((k) => k.toLowerCase() === tokenMint.toLowerCase());
    return key === undefined ? undefined : map[key];
  };
  const num = (v: unknown): number | null => {
    const n = typeof v === "string" ? Number(v) : v;
    return isNum(n) ? n : null;
  };
  const priceUsd = num(field("token_prices"));
  if (priceUsd === null) throw new Error("coingecko: no price for this mint");
  return {
    source: "coingecko",
    priceUsd,
    liquidityUsd: num(field("total_reserve_in_usd")),
    // Time of the last swap: a token nobody has traded recently has no fresh price.
    observedAt: num(field("last_trade_timestamp")),
  };
}

// DexScreener GET /token-pairs/v1/solana/<mint> (free, no key) - every pool the token trades
// in. priceUsd is the *base* token's price, so only pools with our mint as base count, and
// the deepest one is used: thin pools drift and are the cheapest to push around. The
// tokens/v1 endpoint is not used because it returns a single, not necessarily deepest, pool.
// No timestamp is reported, so core.ts can't age-check this source.
export async function fetchDexScreener(tokenMint: string, fetchFn: FetchFn = fetch): Promise<SourceQuote> {
  const url = new URL(`https://api.dexscreener.com/token-pairs/v1/solana/${tokenMint}`);
  const pairs = await getJson(fetchFn, url, {});
  if (!Array.isArray(pairs)) throw new Error("dexscreener: unexpected response");
  let best: { priceUsd: number; liquidityUsd: number } | null = null;
  for (const p of pairs as Array<{ baseToken?: { address?: string }; priceUsd?: unknown; liquidity?: { usd?: unknown } }>) {
    if (p.baseToken?.address !== tokenMint) continue;
    const priceUsd = typeof p.priceUsd === "string" ? Number(p.priceUsd) : NaN;
    const liquidityUsd = p.liquidity?.usd;
    if (!isNum(priceUsd) || priceUsd <= 0 || !isNum(liquidityUsd)) continue;
    if (!best || liquidityUsd > best.liquidityUsd) best = { priceUsd, liquidityUsd };
  }
  if (!best) throw new Error("dexscreener: no pool with this mint as base token");
  return { source: "dexscreener", priceUsd: best.priceUsd, liquidityUsd: best.liquidityUsd, observedAt: null };
}
