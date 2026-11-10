// Pure liquidation math, mirroring liquidate_position.rs exactly (same integer order of
// operations), so the keeper knows which path the program will take before building a
// transaction. No I/O here - unit-tested by scripts/test-liquidation-plan.mts.

/** constants.rs: price units are 10^-12 USD, USDC is 10^-6 -> divide by 10^6 to get micro-USDC. */
export const PRICE_TO_USDC_DIVISOR = 1_000_000n;
/** liquidate_position.rs: liquidator bounty = 4% of the current notional. */
export const LIQUIDATOR_BOUNTY_BPS = 400n;

export type LiquidationPath =
    /** Collateral covers buyback + bounty: buy the tokens back (flash loan + Jupiter) and get paid. */
    | "buyback"
    /** Collateral can't make a liquidator whole: no buyback, all collateral to the staker, $0 to us. */
    | "write_off";

export interface LiquidationPlan {
    path: LiquidationPath;
    /** micro-USDC value of amount_borrowed at the oracle price (what the program reimburses). */
    notionalUsdc: bigint;
    bountyUsdc: bigint;
    /** What the program pays the liquidator on the buyback path: notional + bounty (0 on a write-off). */
    payoutUsdc: bigint;
}

/** amount (base units) * price (10^-12 USD per whole token) -> micro-USDC, floored like the program. */
export function notionalUsdc(amountBorrowed: bigint, priceUsd: bigint, decimals: number): bigint {
    return (amountBorrowed * priceUsd) / 10n ** BigInt(decimals) / PRICE_TO_USDC_DIVISOR;
}

/** ceil(n / d) for positive bigints - the program rounds amounts owed up (programs' math.rs). */
function divCeil(n: bigint, d: bigint): bigint {
    return (n + d - 1n) / d;
}

/** The program writes a position off when `collateral < notional + bounty`. */
export function planLiquidation(params: {
    amountBorrowed: bigint;
    collateralUsdc: bigint;
    priceUsd: bigint;
    decimals: number;
}): LiquidationPlan {
    const notional = notionalUsdc(params.amountBorrowed, params.priceUsd, params.decimals);
    const bounty = (notional * LIQUIDATOR_BOUNTY_BPS) / 10_000n;
    const isWriteOff = params.collateralUsdc < notional + bounty;
    return {
        path: isWriteOff ? "write_off" : "buyback",
        notionalUsdc: notional,
        bountyUsdc: bounty,
        payoutUsdc: isWriteOff ? 0n : notional + bounty,
    };
}

/**
 * Buyback economics: we flash-borrow the swap's worst-case USDC input and must repay it plus the
 * flash-loan fee from the program's payout. `expectedInUsdc` is Jupiter's estimate; anything at
 * or below `minProfitUsdc` isn't worth sending (a loss would revert anyway, but costs fees).
 */
export function expectedBuybackProfit(plan: LiquidationPlan, expectedInUsdc: bigint, flashFeeUsdc: bigint): bigint {
    return plan.payoutUsdc - expectedInUsdc - flashFeeUsdc;
}

export interface LiquidationCheck {
    liquidatable: boolean;
    isExpired: boolean;
    isUndercollateralized: boolean;
    /** Collateral + PnL - interest, floored at 0 (micro-USDC). */
    equityUsdc: bigint;
    /** current notional * liquidation_thresh_bps (micro-USDC); liquidatable at or below it. */
    maintenanceUsdc: bigint;
    accruedInterestUsdc: bigint;
}

/**
 * liquidate_position.rs's trigger: expired, or equity <= maintenance margin. Same integer order
 * of operations as the program, so the keeper only sends liquidations the program will accept.
 */
export function checkLiquidatable(params: {
    amountBorrowed: bigint;
    collateralUsdc: bigint;
    entryPriceUsd: bigint;
    priceUsd: bigint;
    decimals: number;
    interestRateBps: number;
    loanStartTimestamp: bigint;
    expiryTimestamp: bigint;
    liquidationThreshBps: number;
    nowUnix: bigint;
}): LiquidationCheck {
    const current = notionalUsdc(params.amountBorrowed, params.priceUsd, params.decimals);
    const initial = notionalUsdc(params.amountBorrowed, params.entryPriceUsd, params.decimals);

    // math.rs: every started hour (at least one), interest in one division, rounded up.
    const elapsed = params.nowUnix - params.loanStartTimestamp;
    let billableHours = divCeil(elapsed, 3600n);
    if (billableHours < 1n) billableHours = 1n;
    const interest = divCeil(current * BigInt(params.interestRateBps) * billableHours, 240_000n);

    // A short profits when the price falls (initial >= current).
    let equity = params.collateralUsdc;
    if (initial >= current) equity += initial - current;
    else equity = equity >= current - initial ? equity - (current - initial) : 0n;
    equity = equity >= interest ? equity - interest : 0n;

    // Rounded up, like the program: the margin protects the staker.
    const maintenance = divCeil(current * BigInt(params.liquidationThreshBps), 10_000n);
    const isUndercollateralized = equity <= maintenance;
    const isExpired = params.nowUnix >= params.expiryTimestamp;
    return {
        liquidatable: isUndercollateralized || isExpired,
        isExpired,
        isUndercollateralized,
        equityUsdc: equity,
        maintenanceUsdc: maintenance,
        accruedInterestUsdc: interest,
    };
}
