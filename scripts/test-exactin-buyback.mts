// Unit tests for app/src/instructions/exactInBuyback.ts (sizing an ExactIn buy-back).
//   npm run test:buyback
import assert from 'node:assert/strict';
import { NoBuybackRouteError, isNoRouteError, sizeExactInBuyback } from '../app/src/instructions/exactInBuyback.ts';

/** A pool: tokens out for usdc in, with a fee and price impact; slippage = worst-case haircut. */
function pool(tokensPerUsdc: number, feeBps: number, impactPerMillionUsdc: number, slippageBps: number, liquidityCap = Infinity) {
    let calls = 0;
    const quote = async (usdcIn: bigint) => {
        calls++;
        const usd = Number(usdcIn) / 1e6;
        const impact = Math.min(0.99, impactPerMillionUsdc * usd / 1e6);
        const out = Math.min(liquidityCap, Math.floor(Number(usdcIn) * tokensPerUsdc * (1 - feeBps / 1e4) * (1 - impact)));
        if (out <= 0) throw new Error('Jupiter /quote: No routes found');
        return { inAmount: usdcIn.toString(), outAmount: String(out), otherAmountThreshold: String(Math.floor(out * (1 - slippageBps / 1e4))) };
    };
    return { quote, calls: () => calls };
}

let passed = 0;
const test = async (name: string, fn: () => Promise<void>) => { await fn(); passed++; console.log(`ok - ${name}`); };

// 70,000 BULLISH (6 dp) at ~$0.00027 -> ~$19; tokens per USDC base unit = 1e6/0.00027/1e6.
const need = 70_000n * 10n ** 6n;
const estimate = 18_900_000n; // what selling them would fetch (the bid), USDC base units

await test('normal pool: covers the need at the worst fill, little surplus', async () => {
    const p = pool(1 / 0.00027, 30, 2000, 50);
    const { quote, usdcIn } = await sizeExactInBuyback(need, estimate, p.quote);
    assert.ok(BigInt(quote.otherAmountThreshold) >= need, 'worst-case output covers the need');
    assert.ok(Number(quote.otherAmountThreshold) < Number(need) * 1.05, 'surplus under 5%');
    assert.equal(usdcIn, BigInt(quote.inAmount));
    assert.ok(p.calls() <= 3, `few quotes (${p.calls()})`);
});

await test('steep price impact: resizes until covered', async () => {
    const p = pool(1 / 0.00027, 100, 400_000, 100); // ~8% impact at $20
    const { quote } = await sizeExactInBuyback(need, estimate, p.quote);
    assert.ok(BigInt(quote.otherAmountThreshold) >= need);
});

await test('pool too thin to ever cover: NoBuybackRouteError', async () => {
    const p = pool(1 / 0.00027, 30, 0, 50, Number(need) / 2);
    await assert.rejects(sizeExactInBuyback(need, estimate, p.quote), NoBuybackRouteError);
});

await test('no route at all: the Jupiter error propagates and is recognised', async () => {
    const quote = async () => { throw new Error('Jupiter https://api.jup.ag/swap/v1/quote: No routes found'); };
    await assert.rejects(sizeExactInBuyback(need, estimate, quote), (err) => isNoRouteError(err));
});

await test('network errors are not "no route"', async () => {
    assert.equal(isNoRouteError(new Error('fetch failed')), false);
    assert.equal(isNoRouteError(new Error('Jupiter /quote: 429')), false);
    assert.equal(isNoRouteError(new Error('COULD_NOT_FIND_ANY_ROUTE')), true);
});

console.log(`\n${passed} passed`);
