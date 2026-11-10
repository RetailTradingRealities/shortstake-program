// Unit test for the keeper's write-off decision: it must match liquidate_position.rs exactly,
// using the same figures as programs/short-stake001/tests/test_bad_debt_write_off.rs.
//   npm run test:keeper
import assert from 'node:assert/strict';
import { checkLiquidatable, expectedBuybackProfit, notionalUsdc, planLiquidation } from '../app/src/instructions/liquidationPlan.ts';

const base = { amountBorrowed: 50_000_000n, collateralUsdc: 160_000_000n, decimals: 6 }; // 50 tokens, $160

// $4.00: notional $200, needs $208 > $160 collateral -> write-off, nothing paid to the liquidator.
const writeOff = planLiquidation({ ...base, priceUsd: 4_000_000_000_000n });
assert.equal(writeOff.path, 'write_off');
assert.equal(writeOff.notionalUsdc, 200_000_000n);
assert.equal(writeOff.payoutUsdc, 0n);

// $3.07: notional $153.50 + $6.14 bounty = $159.64 <= $160 -> buyback (the Rust test's payout).
const buyback = planLiquidation({ ...base, priceUsd: 3_070_000_000_000n });
assert.equal(buyback.path, 'buyback');
assert.equal(buyback.notionalUsdc, 153_500_000n);
assert.equal(buyback.bountyUsdc, 6_140_000n);
assert.equal(buyback.payoutUsdc, 159_640_000n);

// Exact boundary: collateral == notional + bounty is still a buyback (the program uses `<`).
const edge = planLiquidation({ amountBorrowed: 1_000_000n, collateralUsdc: 104_000_000n, priceUsd: 100_000_000_000_000n, decimals: 6 });
assert.equal(edge.notionalUsdc, 100_000_000n);
assert.equal(edge.path, 'buyback');
assert.equal(planLiquidation({ ...edgeArgs(), collateralUsdc: 103_999_999n }).path, 'write_off');
function edgeArgs() {
  return { amountBorrowed: 1_000_000n, priceUsd: 100_000_000_000_000n, decimals: 6 };
}

// Flooring matches the program: sub-micro-USDC remainders are dropped, 9-decimal mints work.
assert.equal(notionalUsdc(1n, 1_999_999n, 0), 1n); // 1 token at $0.000001999999 -> 1 micro-USDC
assert.equal(notionalUsdc(1_500_000_000n, 2_000_000_000_000n, 9), 3_000_000n); // 1.5 SOL-like @ $2 = $3

// Profit = payout - Jupiter's expected input - flash fee.
assert.equal(expectedBuybackProfit(buyback, 154_000_000n, 0n), 5_640_000n);
assert.equal(expectedBuybackProfit(buyback, 159_640_000n, 1n), -1n);

// --- checkLiquidatable: same scenario as the Rust tests (50 tokens @ $2 entry, $160 collateral,
// 12%/day, 110% maintenance). Interest at 1 billable hour on the current notional.
const position = {
  amountBorrowed: 50_000_000n, collateralUsdc: 160_000_000n, entryPriceUsd: 2_000_000_000_000n, decimals: 6,
  interestRateBps: 1_200, loanStartTimestamp: 1_000_000n, expiryTimestamp: 1_000_000n + 86_400n,
  liquidationThreshBps: 11_000, nowUnix: 1_000_000n,
};
// At entry: equity $160 - $0.50 interest (12%/day = 0.5%/h on $100) vs maintenance $110 -> healthy.
const healthy = checkLiquidatable({ ...position, priceUsd: 2_000_000_000_000n });
assert.equal(healthy.liquidatable, false);
assert.equal(healthy.accruedInterestUsdc, 500_000n);
assert.equal(healthy.equityUsdc, 159_500_000n);
assert.equal(healthy.maintenanceUsdc, 110_000_000n);
// $3.07 (the Rust payout-order test): equity 160 - 53.5 - 0.7675 = $105.7325 <= $168.85.
const underwater = checkLiquidatable({ ...position, priceUsd: 3_070_000_000_000n });
assert.equal(underwater.isUndercollateralized, true);
assert.equal(underwater.equityUsdc, 105_732_500n);
assert.equal(underwater.maintenanceUsdc, 168_850_000n);
// The crossover is p = 260 / 105.25 = $2.4703: 160 - (50p - 100) - 0.005*50p <= 1.1*50p.
// $2.47: equity 135.8825 > maintenance 135.85 -> healthy. $2.48: 135.38 <= 136.40 -> liquidatable.
const at247 = checkLiquidatable({ ...position, priceUsd: 2_470_000_000_000n });
assert.equal(at247.equityUsdc, 135_882_500n);
assert.equal(at247.maintenanceUsdc, 135_850_000n);
assert.equal(at247.liquidatable, false);
assert.equal(checkLiquidatable({ ...position, priceUsd: 2_480_000_000_000n }).liquidatable, true);
// Expiry: one second before is healthy, at expiry it's liquidatable (program uses `>=`).
assert.equal(checkLiquidatable({ ...position, priceUsd: 2_000_000_000_000n, nowUnix: position.expiryTimestamp - 1n }).liquidatable, false);
const expired = checkLiquidatable({ ...position, priceUsd: 2_000_000_000_000n, nowUnix: position.expiryTimestamp });
assert.equal(expired.isExpired, true);
assert.equal(expired.isUndercollateralized, false);
// Billable hours round up: 1 second past an hour bills 2 hours ($1.00).
assert.equal(checkLiquidatable({ ...position, priceUsd: 2_000_000_000_000n, nowUnix: position.loanStartTimestamp + 3_601n }).accruedInterestUsdc, 1_000_000n);
// A short in profit (price halves): +$50 PnL, interest $0.25 on the $50 notional.
assert.equal(checkLiquidatable({ ...position, priceUsd: 1_000_000_000_000n }).equityUsdc, 160_000_000n + 50_000_000n - 250_000n);

console.log('liquidation plan: all assertions passed');
