//! The stored initial_liquidation_price_usd must be the price at which liquidate_position's
//! margin trigger (equity <= maintenance margin, with accrued interest) actually fires.
mod common;

use common::market::*;
use common::*;

const ERR_POSITION_HEALTHY: u32 = 6008;
// $0.000001: wider than the program's micro-USDC rounding of the notional (50 tokens), small
// enough that a wrong formula can't land inside it.
const TOLERANCE: u64 = 1_000_000;

/// $100 notional at $2 with $160 collateral, 1_200 bps/day, liquidated in the first hour:
/// 260 / (50 * (1 + 1.1 + 0.12 / 24)) = $2.470308...
#[test]
fn initial_liquidation_price_is_the_margin_trigger() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    let position: short_stake001::BorrowPosition = m.ctx.fetch(loan.borrow_position);
    let liq_price = position.initial_liquidation_price_usd;
    assert_eq!(liq_price, 2_470_308_788_598);

    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, liq_price - TOLERANCE);
    assert_custom_error(m.liquidate(&loan, None).0, ERR_POSITION_HEALTHY);

    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, liq_price + TOLERANCE);
    m.liquidate(&loan, None).0.expect("liquidation just above the stored price should succeed");
}
