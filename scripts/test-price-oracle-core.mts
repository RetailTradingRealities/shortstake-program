// Unit test for the price oracle's pure core (supabase/functions/price-oracle/core.ts):
// aggregation rules, manipulation guards, unit conversion, and that the signed message
// matches the app's encoder (and so the program) byte for byte.
//   npm run test:oracle
import assert from 'node:assert/strict';
import { address, generateKeyPairSigner, getAddressEncoder, signBytes, verifySignature } from '@solana/kit';
import {
  GUARDS,
  PriceAction,
  aggregate,
  anchorPrice,
  applyJumpGuard,
  exitFallbackPrice,
  LIQUIDATION_PRICE_VALIDITY_SECONDS,
  PROGRAM_MAX_PRICE_AGE_SECONDS,
  publishTimeFor,
  signingPrice,
  signedPriceMessage,
  usdToPriceUnits,
  type MintPriceState,
  type SourceQuote,
} from '../supabase/functions/price-oracle/core.ts';
import { fetchSignedPrice, getSignedPriceInstruction, getSignedPriceMessage } from '../app/src/oracle/signedPrice.ts';
import { fetchBirdeye, fetchCoinGecko, fetchDexScreener, resetCoinGeckoListingCache, type FetchFn } from '../supabase/functions/price-oracle/sources.ts';
import { RUGCHECK_RULES, SPL_TOKEN_PROGRAM, checkTokenSafety, evaluateRugcheck, lpLockedPctFromMarkets } from '../supabase/functions/price-oracle/rugcheck.ts';

const NOW = 1_760_000_000;
const q = (source: SourceQuote['source'], priceUsd: number, extra: Partial<SourceQuote> = {}): SourceQuote => ({
  source,
  priceUsd,
  liquidityUsd: 1_000_000,
  observedAt: NOW,
  ...extra,
});
const ok = (r: ReturnType<typeof aggregate>) => {
  assert.ok(r.ok, `expected a price, got: ${!r.ok && r.reason}`);
  return r;
};

// 3 sources: median.
{
  const r = ok(aggregate([q('birdeye', 1.0), q('dexscreener', 1.02), q('coingecko', 1.01)], PriceAction.OpenBorrowPosition, NOW));
  assert.equal(r.priceUsd, 1.01);
  assert.deepEqual(r.used.sort(), ['birdeye', 'coingecko', 'dexscreener']);
}

// 3 sources, one >5% off the median: dropped, average of the other two.
{
  const r = ok(aggregate([q('birdeye', 1.0), q('dexscreener', 1.02), q('coingecko', 1.2)], PriceAction.OpenBorrowPosition, NOW));
  assert.equal(r.priceUsd, 1.01);
  assert.deepEqual(r.dropped, [{ source: 'coingecko', reason: 'outlier' }]);
}

// 2 sources that agree: average.
{
  const r = ok(aggregate([q('birdeye', 1.0), q('dexscreener', 1.04)], PriceAction.DepositStake, NOW));
  assert.equal(r.priceUsd, 1.02);
}

// 2 sources that disagree by >5%: can't tell which is wrong -> rejected, whichever is lower.
for (const pair of [[1.0, 1.06], [1.06, 1.0]]) {
  const r = aggregate([q('birdeye', pair[0]), q('dexscreener', pair[1])], PriceAction.LiquidatePosition, NOW);
  assert.equal(r.ok, false);
}

// 1 source: rejected for deposit / open, accepted for close / liquidate.
{
  const one = [q('birdeye', 0.5)];
  assert.equal(aggregate(one, PriceAction.DepositStake, NOW).ok, false);
  assert.equal(aggregate(one, PriceAction.OpenBorrowPosition, NOW).ok, false);
  assert.equal(ok(aggregate(one, PriceAction.ClosePosition, NOW)).priceUsd, 0.5);
  assert.equal(ok(aggregate(one, PriceAction.LiquidatePosition, NOW)).priceUsd, 0.5);
}

// Stale, illiquid and invalid quotes don't count; unknown age / liquidity is allowed.
{
  const r = aggregate(
    [
      q('birdeye', 1.0, { observedAt: NOW - GUARDS.maxSourceAgeSeconds - 1 }),
      q('dexscreener', 1.0, { liquidityUsd: GUARDS.minLiquidityUsd - 1 }),
      q('coingecko', Number.NaN),
    ],
    PriceAction.LiquidatePosition,
    NOW,
  );
  assert.equal(r.ok, false);
  assert.deepEqual(r.dropped.map((d) => d.reason), ['stale', 'illiquid', 'invalid']);
  ok(aggregate([q('coingecko', 1.0, { observedAt: null, liquidityUsd: null })], PriceAction.ClosePosition, NOW));
}

// Jump guard: small moves pass, a big move only after it has persisted.
{
  const fresh: MintPriceState = { lastPriceUsd: null, lastAt: null, pendingPriceUsd: null, pendingSince: null };
  let d = applyJumpGuard(fresh, 1.0, NOW);
  assert.ok(d.accept, 'first price for a mint has nothing to compare against');
  d = applyJumpGuard(d.next, 1.1, NOW + 5);
  assert.ok(d.accept, '10% move is within the jump band');

  const spike = applyJumpGuard(d.next, 1.5, NOW + 10);
  assert.ok(!spike.accept && spike.retryAfterSeconds === GUARDS.jumpConfirmSeconds);
  const early = applyJumpGuard(spike.next, 1.52, NOW + 30);
  assert.ok(!early.accept, 'still inside the confirmation window');
  const confirmed = applyJumpGuard(early.next, 1.51, NOW + 10 + GUARDS.jumpConfirmSeconds);
  assert.ok(confirmed.accept, 'sustained move is accepted after the window');
  assert.equal(confirmed.next.lastPriceUsd, 1.51);

  const reverted = applyJumpGuard(spike.next, 1.1, NOW + 20);
  assert.ok(reverted.accept, 'price back near the last accepted level passes');
  assert.equal(reverted.next.pendingPriceUsd, null);

  const wobble = applyJumpGuard(spike.next, 2.0, NOW + 80);
  assert.ok(!wobble.accept, 'a different new level restarts the window');
  assert.equal(wobble.next.pendingSince, NOW + 80);
}

// Slot Zero M-01: the jump guard can't be ratcheted in 15% steps, and liquidations / opens
// sign against a price that is at least a minute old.
{
  const fresh: MintPriceState = { lastPriceUsd: null, lastAt: null, pendingPriceUsd: null, pendingSince: null };
  let d = applyJumpGuard(fresh, 1.0, NOW);
  assert.ok(d.accept);
  // An hour of quiet at 1.00, then the ladder 1.14 -> 1.30 -> 1.44 a few seconds apart.
  const later = NOW + 3_600;
  const s1 = applyJumpGuard(d.next, 1.14, later);
  assert.ok(s1.accept, '14% from the anchor is within the band');
  const s2 = applyJumpGuard(s1.next, 1.3, later + 3);
  assert.ok(!s2.accept, '30% from the 1.00 anchor is held back, even though it is 14% from 1.14');
  const s3 = applyJumpGuard(s2.next, 1.44, later + 6);
  assert.ok(!s3.accept, 'the ladder does not ratchet');

  // Liquidations sign the lower of current and anchor; opens the higher.
  assert.equal(anchorPrice(s1.next, later + 5), 1.0, 'the 1.14 accepted 5 s ago is not yet an anchor');
  assert.equal(signingPrice(PriceAction.LiquidatePosition, 1.14, anchorPrice(s1.next, later + 5)), 1.0);
  assert.equal(signingPrice(PriceAction.OpenBorrowPosition, 0.9, 1.0), 1.0);
  assert.equal(signingPrice(PriceAction.ClosePosition, 1.14, 1.0), 1.14);
  assert.equal(signingPrice(PriceAction.DepositStake, 1.14, 1.0), 1.14);
  assert.equal(signingPrice(PriceAction.LiquidatePosition, 1.14, null), 1.14, 'no anchor yet: current price');
  // A minute later the 1.14 is the anchor.
  assert.equal(anchorPrice(s1.next, later + GUARDS.anchorSeconds), 1.14);

  // A genuine big move held through the confirmation window becomes the anchor at once, so the
  // next requests at that level aren't held back a second time.
  const p1 = applyJumpGuard(s1.next, 1.5, later + 100);
  assert.ok(!p1.accept);
  const p2 = applyJumpGuard(p1.next, 1.5, later + 100 + GUARDS.jumpConfirmSeconds);
  assert.ok(p2.accept);
  const after = applyJumpGuard(p2.next, 1.52, later + 100 + GUARDS.jumpConfirmSeconds + 5);
  assert.ok(after.accept, 'the confirmed level is the new reference');
  assert.equal(anchorPrice(p2.next, later + 100 + GUARDS.jumpConfirmSeconds + 1), 1.5);

  // History stays bounded, but keeps one entry older than the window as the anchor after idle.
  let st = d.next;
  for (let i = 1; i <= 50; i++) st = applyJumpGuard(st, 1.0 + i * 0.001, NOW + i * 60).next;
  const h = st.history!;
  assert.ok(h.length <= 12, `history pruned (${h.length} entries)`);
  assert.ok(h[0].at < NOW + 50 * 60 - GUARDS.historySeconds, 'one entry older than the window kept');
  assert.notEqual(anchorPrice(st, NOW + 50 * 60 + 10_000), null, 'still an anchor after a long idle');
}

// Pre-M-01 state rows (no history) still work: the last price is the reference and anchor.
{
  const old: MintPriceState = { lastPriceUsd: 2.0, lastAt: NOW - 600, pendingPriceUsd: null, pendingSince: null };
  assert.equal(anchorPrice(old, NOW), 2.0);
  assert.ok(applyJumpGuard(old, 2.2, NOW).accept);
  assert.ok(!applyJumpGuard(old, 2.5, NOW).accept);
}

// Slot Zero L-05: no reading never touches the state; an unknown action is refused.
{
  const st: MintPriceState = { lastPriceUsd: 1.0, lastAt: NOW, pendingPriceUsd: 1.5, pendingSince: NOW - 30 };
  const nan = applyJumpGuard(st, Number.NaN, NOW + 1);
  assert.ok(!nan.accept);
  assert.deepEqual(nan.next, st, 'a NaN reading leaves the pending move and its timer alone');
  assert.equal(aggregate([q('birdeye', 1.0)], 'constructor' as unknown as PriceAction, NOW).ok, false);
  assert.equal(aggregate([], PriceAction.ClosePosition, NOW).ok, false, 'zero sources is never ok');
}

// Slot Zero L-01: exit fallback when no source survives.
{
  const st: MintPriceState = { lastPriceUsd: 1.0, lastAt: NOW - 3_600, pendingPriceUsd: null, pendingSince: null };
  const thin = [q('dexscreener', 1.2, { liquidityUsd: 5_000 }), q('birdeye', 0.9, { liquidityUsd: 4_000 })];
  assert.equal(exitFallbackPrice(PriceAction.ClosePosition, st, thin), 1.2, 'close: the higher');
  assert.equal(exitFallbackPrice(PriceAction.LiquidatePosition, st, thin), 0.9, 'liquidate: the lower');
  assert.equal(exitFallbackPrice(PriceAction.LiquidatePosition, st, []), 1.0, 'no quotes at all: last accepted');
  assert.equal(exitFallbackPrice(PriceAction.OpenBorrowPosition, st, thin), null, 'never for opens');
  assert.equal(exitFallbackPrice(PriceAction.DepositStake, st, thin), null, 'never for deposits');
  const none: MintPriceState = { lastPriceUsd: null, lastAt: null, pendingPriceUsd: null, pendingSince: null };
  assert.equal(exitFallbackPrice(PriceAction.ClosePosition, none, []), null);
}

// Unit conversion: 10^-12 USD per whole token, exact for sub-cent prices.
assert.equal(usdToPriceUnits(1), 1_000_000_000_000n);
assert.equal(usdToPriceUnits(0.00002), 20_000_000n);
assert.equal(usdToPriceUnits(0.1), 100_000_000_000n);
assert.equal(usdToPriceUnits(97_123.45), 97_123_450_000_000_000n);
assert.throws(() => usdToPriceUnits(1e-13));
assert.throws(() => usdToPriceUnits(2e7));

// Signed message matches the app encoder (which the program's tests mirror).
{
  const programId = address('sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3');
  const mint = address('EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ');
  const enc = getAddressEncoder();
  for (const action of Object.values(PriceAction)) {
    const ours = signedPriceMessage(
      enc.encode(programId) as Uint8Array,
      enc.encode(mint) as Uint8Array,
      123_456_789n,
      1_760_000_000n,
      action,
    );
    assert.deepEqual(ours, new Uint8Array(getSignedPriceMessage(mint, 123_456_789n, 1_760_000_000n, action, programId)));
  }
}

// Source adapters against recorded responses.
const SOL = 'So11111111111111111111111111111111111111112';
const replay = (status: number, body: unknown, seen: { url?: URL; headers?: HeadersInit } = {}): FetchFn =>
  async (input, init) => {
    seen.url = new URL(input.toString());
    seen.headers = init?.headers;
    return new Response(JSON.stringify(body), { status });
  };
const birdeyeOk = {
  data: {
    isScaledUiToken: false,
    value: 0.38622452197470425,
    updateUnixTime: 1745058945,
    updateHumanTime: '2025-04-19T10:35:45',
    priceChange24h: 1.933391934259418,
    priceInNative: 0.0027761598581298626,
    liquidity: 10854103.37938592,
  },
  success: true,
};
{
  const seen: { url?: URL; headers?: HeadersInit } = {};
  const quote = await fetchBirdeye(SOL, 'test-key', replay(200, birdeyeOk, seen));
  assert.deepEqual(quote, {
    source: 'birdeye',
    priceUsd: 0.38622452197470425,
    liquidityUsd: 10854103.37938592,
    observedAt: 1745058945,
  });
  assert.equal(seen.url?.searchParams.get('address'), SOL);
  assert.deepEqual(seen.headers, { 'X-API-KEY': 'test-key', 'x-chain': 'solana' });

  const noLiquidity = { ...birdeyeOk, data: { ...birdeyeOk.data, liquidity: undefined } };
  assert.equal((await fetchBirdeye(SOL, 'k', replay(200, noLiquidity))).liquidityUsd, null);

  await assert.rejects(fetchBirdeye(SOL, 'k', replay(401, { success: false })), /HTTP 401/);
  await assert.rejects(fetchBirdeye(SOL, 'k', replay(200, { success: false, data: null })), /no price/);
  await assert.rejects(fetchBirdeye(SOL, 'k', replay(200, { success: true, data: { value: 'x' } })), /no price/);
  const scaled = { ...birdeyeOk, data: { ...birdeyeOk.data, isScaledUiToken: true } };
  await assert.rejects(fetchBirdeye(SOL, 'k', replay(200, scaled)), /scaled-UI/);
}

{
  const MINT = 'EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ';
  const byMint = (v: string) => ({ [MINT]: v });
  const coingeckoOk = {
    data: {
      id: '49dd0b90-d99d-48b6-8fe2-03e826fbcef2',
      type: 'simple_token_price',
      attributes: {
        token_prices: byMint('2084.57379936447'),
        total_reserve_in_usd: byMint('1015673598.896450024556193850116628967'),
        last_trade_timestamp: byMint('1779882383'),
      },
    },
  };
  // Routes the two CoinGecko endpoints separately and records which were called.
  const cgRouter = (simple: { status: number; body: unknown }, onchain: { status: number; body: unknown }, calls: string[]): FetchFn =>
    async (input) => {
      const url = new URL(input.toString());
      const which = url.pathname.includes('/onchain/') ? 'onchain' : 'simple';
      calls.push(which);
      const r = which === 'onchain' ? onchain : simple;
      return new Response(JSON.stringify(r.body), { status: r.status });
    };

  // Listed token: one call, aggregated price, no liquidity figure.
  resetCoinGeckoListingCache();
  {
    const calls: string[] = [];
    const quote = await fetchCoinGecko(MINT, 'k', cgRouter({ status: 200, body: { [MINT]: { usd: 0.0461489, last_updated_at: 1779882400 } } }, { status: 200, body: coingeckoOk }, calls));
    assert.deepEqual(quote, { source: 'coingecko', priceUsd: 0.0461489, liquidityUsd: null, observedAt: 1779882400 });
    assert.deepEqual(calls, ['simple']);
  }
  // Unlisted token: falls back to the on-chain price, then skips the aggregated endpoint.
  resetCoinGeckoListingCache();
  {
    const calls: string[] = [];
    const router = cgRouter({ status: 200, body: {} }, { status: 200, body: coingeckoOk }, calls);
    assert.equal((await fetchCoinGecko(MINT, 'k', router)).priceUsd, 2084.57379936447);
    assert.equal((await fetchCoinGecko(MINT, 'k', router)).priceUsd, 2084.57379936447);
    assert.deepEqual(calls, ['simple', 'onchain', 'onchain'], 'unlisted mint remembered');
  }
  // Aggregated endpoint erroring: on-chain fallback, and NOT remembered as unlisted.
  resetCoinGeckoListingCache();
  {
    const calls: string[] = [];
    const router = cgRouter({ status: 500, body: {} }, { status: 200, body: coingeckoOk }, calls);
    await fetchCoinGecko(MINT, 'k', router);
    await fetchCoinGecko(MINT, 'k', router);
    assert.deepEqual(calls, ['simple', 'onchain', 'simple', 'onchain']);
  }

  // On-chain endpoint parsing (mint already known as unlisted, so it's the only call).
  resetCoinGeckoListingCache();
  await fetchCoinGecko(MINT, 'k', cgRouter({ status: 200, body: {} }, { status: 200, body: coingeckoOk }, []));
  const seen: { url?: URL; headers?: HeadersInit } = {};
  const quote = await fetchCoinGecko(MINT, 'test-key', replay(200, coingeckoOk, seen));
  assert.deepEqual(quote, {
    source: 'coingecko',
    priceUsd: 2084.57379936447,
    liquidityUsd: 1015673598.89645,
    observedAt: 1779882383,
  });
  assert.ok(seen.url?.pathname.endsWith(`/onchain/simple/networks/solana/token_price/${MINT}`));
  assert.equal(seen.url?.searchParams.get('include_total_reserve_in_usd'), 'true');
  assert.equal(seen.url?.searchParams.get('include_inactive_source'), 'true');
  assert.deepEqual(seen.headers, { 'x-cg-pro-api-key': 'test-key' });

  const priceOnly = { data: { attributes: { token_prices: byMint('0.5') } } };
  assert.deepEqual(await fetchCoinGecko(MINT, 'k', replay(200, priceOnly)), {
    source: 'coingecko', priceUsd: 0.5, liquidityUsd: null, observedAt: null,
  });
  await assert.rejects(fetchCoinGecko(MINT, 'k', replay(200, { data: { attributes: { token_prices: {} } } })), /no price/);
  await assert.rejects(fetchCoinGecko(MINT, 'k', replay(200, { data: { attributes: { token_prices: byMint('abc') } } })), /no price/);
  await assert.rejects(fetchCoinGecko(MINT, 'k', replay(429, {})), /HTTP 429/);
}

{
  // Trimmed from a live WIF response: deepest pool wins, quote-side and thin pools ignored.
  const WIF = 'EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm';
  const pair = (base: string, priceUsd: string | undefined, liq: number | undefined) => ({
    chainId: 'solana', dexId: 'raydium', baseToken: { address: base }, quoteToken: { address: SOL },
    priceUsd, liquidity: liq === undefined ? undefined : { usd: liq },
  });
  const pairs = [
    pair(WIF, '0.2012', 152_812),
    pair('wifoutMint1111111111111111111111111111111111', '0.0002392', 9_000_000), // WIF is the quote here
    pair(WIF, '0.2014', 5_754_695),
    pair(WIF, '0.2098', 20_848),
    pair(WIF, undefined, 99_000_000),
  ];
  const seen: { url?: URL; headers?: HeadersInit } = {};
  assert.deepEqual(await fetchDexScreener(WIF, replay(200, pairs, seen)), {
    source: 'dexscreener', priceUsd: 0.2014, liquidityUsd: 5_754_695, observedAt: null,
  });
  assert.ok(seen.url?.pathname.endsWith(`/token-pairs/v1/solana/${WIF}`));
  await assert.rejects(fetchDexScreener(WIF, replay(200, [pairs[1]])), /no pool/);
  await assert.rejects(fetchDexScreener(WIF, replay(200, [])), /no pool/);
  await assert.rejects(fetchDexScreener(WIF, replay(200, { pairs: null })), /unexpected/);
  await assert.rejects(fetchDexScreener(WIF, replay(500, {})), /HTTP 500/);
}

// Edge Function response -> app client -> Ed25519 instruction, with a throwaway signer.
{
  const programId = address('sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3');
  const mint = address('EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ');
  const signer = await generateKeyPairSigner();
  const enc = getAddressEncoder();
  const units = usdToPriceUnits(0.2014);
  const message = signedPriceMessage(enc.encode(programId) as Uint8Array, enc.encode(mint) as Uint8Array, units, 1_760_000_000n, PriceAction.LiquidatePosition);
  const signature = await signBytes(signer.keyPair.privateKey, message);
  const oracleResponse = {
    tokenMint: mint,
    priceUsd: units.toString(),
    publishTime: '1760000000',
    action: PriceAction.LiquidatePosition,
    signer: signer.address,
    signature: Buffer.from(signature).toString('base64'),
    sources: ['birdeye', 'dexscreener'],
    dropped: [],
  };

  const realFetch = globalThis.fetch;
  let sentBody: unknown;
  globalThis.fetch = (async (_url: unknown, init?: RequestInit) => {
    sentBody = JSON.parse(String(init?.body));
    return new Response(JSON.stringify(oracleResponse), { status: 200 });
  }) as typeof fetch;
  try {
    const signed = await fetchSignedPrice('https://example.invalid/functions/v1/price-oracle', 'anon', mint, PriceAction.LiquidatePosition);
    assert.deepEqual(sentBody, { mint, action: 'liquidate' });
    assert.equal(signed.priceUsd, 201_400_000_000n);
    const ix = getSignedPriceInstruction(signed, programId);
    // The program reads the message at offset 16+32+64 inside the Ed25519 instruction.
    const ixMessage = ix.data!.slice(112, 112 + 81);
    assert.deepEqual(new Uint8Array(ixMessage), message);
    assert.ok(await verifySignature(signer.keyPair.publicKey, signed.signature, ixMessage));

    assert.deepEqual(signed.warnings, []);
    globalThis.fetch = (async () => new Response(JSON.stringify({ ...oracleResponse, warnings: ['Freeze authority is enabled'] }), { status: 200 })) as typeof fetch;
    assert.deepEqual((await fetchSignedPrice('https://x.invalid', 'anon', mint, PriceAction.LiquidatePosition)).warnings, ['Freeze authority is enabled']);
    globalThis.fetch = (async () => new Response(JSON.stringify({ error: 'token failed safety checks', reasons: ['Risk score 71 is above 50', 'Danger: Top 10 holders own 90%'] }), { status: 422 })) as typeof fetch;
    await assert.rejects(fetchSignedPrice('https://x.invalid', 'anon', mint, PriceAction.DepositStake), /safety checks: Risk score 71 is above 50; Danger: Top 10 holders/);
    globalThis.fetch = (async () => new Response(JSON.stringify({ error: 'needs 2 agreeing source(s), have 1', dropped: [{ source: 'coingecko', reason: 'stale' }], failed: [{ source: 'birdeye', error: 'HTTP 429 from public-api.birdeye.so' }] }), { status: 422 })) as typeof fetch;
    await assert.rejects(fetchSignedPrice('https://x.invalid', 'anon', mint, PriceAction.DepositStake), /have 1 \[dropped coingecko \(stale\); failed birdeye \(HTTP 429 from public-api\.birdeye\.so\)\]/);
    globalThis.fetch = (async () => new Response(JSON.stringify({ error: 'large price move awaiting confirmation', retryAfterSeconds: 42 }), { status: 503 })) as typeof fetch;
    await assert.rejects(fetchSignedPrice('https://x.invalid', 'anon', mint, PriceAction.OpenBorrowPosition), /retry in 42s/);

    globalThis.fetch = (async () => new Response(JSON.stringify({ ...oracleResponse, action: PriceAction.ClosePosition }), { status: 200 })) as typeof fetch;
    await assert.rejects(fetchSignedPrice('https://x.invalid', 'anon', mint, PriceAction.LiquidatePosition), /different mint or action/);
  } finally {
    globalThis.fetch = realFetch;
  }
}

// Rugcheck gate. Shapes trimmed from live reports (POPCAT-like pass, USDC-like authorities).
{
  const goodReport = {
    token: { mintAuthority: null, freezeAuthority: null },
    tokenProgram: SPL_TOKEN_PROGRAM,
    score_normalised: 22,
    rugged: false,
    totalMarketLiquidity: 1000,
    risks: [{ name: 'High holder concentration', level: 'warn' }],
    // 900 of 1000 USD locked -> 90%. A market without lp data is ignored.
    markets: [
      { lp: { quoteUSD: 400, baseUSD: 400, lpLockedUSD: 800 } },
      { lp: { quoteUSD: 100, baseUSD: 100, lpLockedUSD: 100 } },
      { lp: null },
    ],
  };
  const unlocked = [{ lp: { quoteUSD: 500, baseUSD: 500, lpLockedUSD: 0 } }];

  const pass = evaluateRugcheck(goodReport);
  assert.equal(pass.eligible, true);
  assert.deepEqual(pass.reasons, []);
  assert.deepEqual(pass.warnings, []);
  assert.equal(pass.scoreNormalised, 22);
  assert.equal(pass.lpLockedPct, 90);
  assert.equal(pass.totalLiquidityUsd, 1000);

  // Locked share is weighted by USD liquidity across markets, as Rugcheck's summary computes it
  // (a deep, mostly locked pool outweighs thin unlocked ones); non-numeric fields count as 0.
  assert.ok(Math.abs(lpLockedPctFromMarkets([
    { lp: { quoteUSD: 700_000, baseUSD: 700_000, lpLockedUSD: 1_000_000 } },
    { lp: { quoteUSD: 50_000, baseUSD: 50_000, lpLockedUSD: 0 } },
    { lp: { quoteUSD: 1, baseUSD: undefined, lpLockedUSD: 'n/a' } },
  ])! - (1_000_000 / 1_500_001) * 100) < 1e-9);
  assert.equal(lpLockedPctFromMarkets([]), null, 'no markets');
  assert.equal(lpLockedPctFromMarkets([{ lp: { quoteUSD: 0, baseUSD: 0, lpLockedUSD: 0 } }]), null, 'no priced liquidity');
  assert.equal(lpLockedPctFromMarkets(undefined), null);

  const reasonsFor = (report: object) => evaluateRugcheck({ ...goodReport, ...report }).reasons;
  assert.match(reasonsFor({ score_normalised: RUGCHECK_RULES.maxScoreNormalised + 1 })[0], /Risk score 51 is above 50/);
  assert.deepEqual(reasonsFor({ score_normalised: RUGCHECK_RULES.maxScoreNormalised }), [], 'exactly at the limit passes');
  assert.deepEqual(reasonsFor({ markets: unlocked }), ['Only 0.0% of liquidity is locked (minimum 20%), and total liquidity $1,000 is below $1,000,000']);
  // Deep enough pools pass without locked LP (BONK: 16% locked, $1.6M liquidity).
  const floor = RUGCHECK_RULES.minLiquidityUsdUnlocked;
  assert.deepEqual(reasonsFor({ markets: unlocked, totalMarketLiquidity: floor }), [], 'exactly at the floor passes');
  assert.match(reasonsFor({ markets: unlocked, totalMarketLiquidity: floor - 1 })[0], /\$999,999 is below \$1,000,000/);
  assert.deepEqual(reasonsFor({ markets: [], totalMarketLiquidity: 50_000_000 }), [], 'depth alone is enough');
  // Locked LP alone is enough for small pools; at exactly the minimum passes.
  assert.deepEqual(reasonsFor({ totalMarketLiquidity: undefined }), [], 'locked LP alone is enough');
  assert.deepEqual(reasonsFor({ markets: [{ lp: { quoteUSD: 50, baseUSD: 50, lpLockedUSD: 20 } }] }), []);
  assert.match(reasonsFor({ rugged: true })[0], /rugged/);
  assert.match(reasonsFor({ risks: [{ name: 'Freeze Authority still enabled', level: 'danger' }] })[0], /Danger: Freeze Authority/);
  // Missing data fails closed.
  assert.match(reasonsFor({ rugged: undefined })[0], /Rugged status unavailable/);
  assert.match(reasonsFor({ score_normalised: '22' })[0], /invalid risk score/);
  assert.match(reasonsFor({ markets: undefined })[0], /Locked liquidity unavailable/);
  assert.match(reasonsFor({ markets: [] })[0], /Locked liquidity unavailable/);
  assert.match(reasonsFor({ markets: unlocked, totalMarketLiquidity: '5000000' })[0], /total liquidity unavailable/);
  assert.match(reasonsFor({ risks: undefined })[0], /Risk findings unavailable/);
  assert.match(reasonsFor({ token: null })[0], /no token data/);

  // Token program and authorities warn but never reject.
  const usdcLike = evaluateRugcheck(
    { ...goodReport, token: { mintAuthority: 'BJE5MMbqXjVwjAF7oxwPYXnTXDyspzZyt4vwenNw5ruG', freezeAuthority: '7dGbd2QZcCKcTndnHcTL8q7SMVXAkp688NTQYwrRCrar' } },
  );
  assert.equal(usdcLike.eligible, true);
  assert.deepEqual(usdcLike.warnings, ['Freeze authority is enabled', 'Mint authority is enabled']);
  const token2022 = evaluateRugcheck({ ...goodReport, tokenProgram: 'TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb', token: {} });
  assert.equal(token2022.eligible, true);
  assert.deepEqual(token2022.warnings, [
    'Unsupported token program TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb',
    'Freeze authority status unavailable',
    'Mint authority status unavailable',
  ]);

  // One call: the report, rescanned (refresh=true), API key in the header (not the URL, where
  // it would leak into logs). /report/summary is never called. Any HTTP error throws (caller
  // fails closed).
  const urls: string[] = [];
  const rcFetch = (status: number): FetchFn => async (input, init) => {
    const url = new URL(input.toString());
    urls.push(`${url.pathname}?${url.searchParams}`);
    assert.equal((init?.headers as Record<string, string>)['X-API-KEY'], 'rc-key');
    return new Response(JSON.stringify(goodReport), { status });
  };
  const live = await checkTokenSafety('7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr', 'rc-key', rcFetch(200));
  assert.equal(live.eligible, true);
  assert.deepEqual(urls, ['/v1/tokens/7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr/report?refresh=true']);
  await assert.rejects(checkTokenSafety('EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ', 'rc-key', rcFetch(400)), /HTTP 400/);

  // HTTP 429 is retried after a wait (a short Retry-After replaces it, a long one is ignored);
  // after 2 retries the 429 throws. Other errors are not retried.
  const sequence = (...statuses: Array<[number, string?]>) => {
    let calls = 0;
    const fn: FetchFn = async () => {
      const [status, retryAfter] = statuses[Math.min(calls++, statuses.length - 1)];
      return new Response(JSON.stringify(goodReport), { status, headers: retryAfter ? { 'retry-after': retryAfter } : {} });
    };
    return { fn, calls: () => calls };
  };
  const waits: number[] = [];
  const noSleep = async (ms: number) => { waits.push(ms); };
  const mint = '7GCihgDB8fe6KNjn2MYtkzZcRjQy3t9GHdC8uHYmW2hr';

  const recovers = sequence([429], [429, '2'], [200]);
  assert.equal((await checkTokenSafety(mint, 'rc-key', recovers.fn, noSleep)).eligible, true);
  assert.equal(recovers.calls(), 3);
  assert.deepEqual(waits, [1_000, 2_000]);

  waits.length = 0;
  const stuck = sequence([429, '60']);
  await assert.rejects(checkTokenSafety(mint, 'rc-key', stuck.fn, noSleep), /HTTP 429/);
  assert.equal(stuck.calls(), 3);
  assert.deepEqual(waits, [1_000, 3_000], 'a 60s Retry-After is not honoured');

  waits.length = 0;
  const serverError = sequence([500]);
  await assert.rejects(checkTokenSafety(mint, 'rc-key', serverError.fn, noSleep), /HTTP 500/);
  assert.equal(serverError.calls(), 1);
  assert.deepEqual(waits, []);
}

// Liquidation prices are backdated so the program's 60 s limit leaves only 15 s (KupiaSec,
// 2026-11-23): a liquidate signature names no position, so a long validity lets a liquidator
// keep the highest price from a falling market. Other actions keep the real time.
{
  assert.equal(PROGRAM_MAX_PRICE_AGE_SECONDS, 60, 'must match signed_price.rs MAX_SIGNED_PRICE_AGE_SECONDS');
  const liq = publishTimeFor(PriceAction.LiquidatePosition, NOW);
  assert.equal(NOW - liq, PROGRAM_MAX_PRICE_AGE_SECONDS - LIQUIDATION_PRICE_VALIDITY_SECONDS);
  // usable on-chain until the program's age limit: NOW + 15 s
  assert.equal(liq + PROGRAM_MAX_PRICE_AGE_SECONDS, NOW + LIQUIDATION_PRICE_VALIDITY_SECONDS);
  for (const action of [PriceAction.DepositStake, PriceAction.OpenBorrowPosition, PriceAction.ClosePosition]) {
    assert.equal(publishTimeFor(action, NOW), NOW);
  }
}

console.log('price oracle core: all assertions passed');
