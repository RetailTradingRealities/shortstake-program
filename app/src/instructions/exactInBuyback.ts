// Buying back a borrowed token when Jupiter has no ExactOut route (e.g. Pump.fun AMM tokens,
// 2026-11-14: BULLISH and Fartcat had ExactIn routes but no ExactOut route at any size).
//
// Instead of "buy exactly N tokens" (ExactOut) we buy with a fixed USDC amount (ExactIn), sized so
// that even the WORST fill Jupiter allows (otherAmountThreshold = the least output after
// slippage) is at least the N tokens owed. The program then takes exactly N; the small surplus
// stays with the buyer (the borrower on close, the keeper on liquidation).
//
// Shared by the website (close) and the keeper (liquidation). Runtime-agnostic: the caller passes
// its own quote function (the site goes through jupiter-proxy, the keeper calls Jupiter directly).

/** The fields of a Jupiter quote response used here. */
export interface QuoteAmounts {
    inAmount: string;
    outAmount: string;
    /** ExactIn: the least output after slippage. */
    otherAmountThreshold: string;
}

/** ExactIn quote: spend exactly `usdcIn` (USDC base units) on the token. */
export type ExactInBuyQuote<Q extends QuoteAmounts> = (usdcIn: bigint) => Promise<Q>;

/** Headroom over the estimate: the sell quote is the bid, and a buy pays the spread + fees again. */
const FIRST_GUESS_HEADROOM_BPS = 300n; // 3%
/** Each resize adds this much on top of the exact ratio, so the next quote lands above the need. */
const RESIZE_MARGIN_BPS = 50n; // 0.5%
const MAX_RESIZES = 4;

export class NoBuybackRouteError extends Error {
    constructor(message: string) {
        super(message);
        this.name = "NoBuybackRouteError";
    }
}

/**
 * Finds an ExactIn USDC -> token quote whose worst-case output covers `tokensNeeded`.
 * `estimateUsdc` is a first guess of the USDC value of `tokensNeeded` (e.g. an ExactIn
 * token -> USDC quote's outAmount for the same amount, or notional at the program's price).
 * Returns the quote and the exact USDC it spends. Throws NoBuybackRouteError if no quote covers it.
 */
export async function sizeExactInBuyback<Q extends QuoteAmounts>(
    tokensNeeded: bigint,
    estimateUsdc: bigint,
    quote: ExactInBuyQuote<Q>,
): Promise<{ quote: Q; usdcIn: bigint }> {
    if (tokensNeeded <= 0n) throw new Error("tokensNeeded must be positive");
    let usdcIn = (maxBig(estimateUsdc, 1n) * (10_000n + FIRST_GUESS_HEADROOM_BPS)) / 10_000n + 1n;
    for (let attempt = 0; attempt <= MAX_RESIZES; attempt++) {
        const q = await quote(usdcIn);
        const worstOut = BigInt(q.otherAmountThreshold);
        if (worstOut >= tokensNeeded) return { quote: q, usdcIn: BigInt(q.inAmount) };
        if (worstOut <= 0n) break;
        // Scale by the shortfall ratio, plus a margin (price impact grows with size).
        usdcIn = (usdcIn * tokensNeeded * (10_000n + RESIZE_MARGIN_BPS)) / (worstOut * 10_000n) + 1n;
    }
    throw new NoBuybackRouteError(`no ExactIn quote covers ${tokensNeeded} tokens after slippage`);
}

/** Whether a Jupiter error means "no route" (as opposed to a network or rate-limit error). */
export function isNoRouteError(error: unknown): boolean {
    const message = error instanceof Error ? error.message : String(error);
    return /no routes found|could_not_find_any_route|route not found/i.test(message);
}

function maxBig(a: bigint, b: bigint): bigint {
    return a > b ? a : b;
}
