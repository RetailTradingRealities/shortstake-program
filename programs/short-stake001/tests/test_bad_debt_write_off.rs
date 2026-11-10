//! Liquidation payout order, bad-debt write-offs (offer closed outright), non-ATA payouts,
//! and the interest formula's precision for rates below 24 bps/day.
mod common;

use common::market::*;
use common::*;

/// Price gaps to $4: notional $200, so a liquidator would need $208 but only $160 of collateral
/// exists. Instead of a stuck position: no buyback, all collateral to the staker, the unborrowed
/// tokens back to the staker, and the offer account closed.
#[test]
fn write_off_pays_staker_all_collateral_and_closes_offer() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let (result, liquidator_token_account, liquidator_usdc_account) = m.liquidate(&loan, None);
    result.expect("write-off liquidation should succeed");

    assert_eq!(m.staker_usdc(), COLLATERAL, "staker receives all collateral");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - BORROW, "unborrowed tokens returned");
    assert!(!m.ctx.account_exists(m.lender_offer), "offer closed outright");
    assert!(!m.ctx.account_exists(loan.borrow_position));

    // No buyback happened and nobody else was paid.
    assert_eq!(m.ctx.token_balance(liquidator_token_account), BORROW);
    assert_eq!(m.ctx.token_balance(liquidator_usdc_account), 0);
    assert_eq!(m.ctx.token_balance(fee_collector_usdc_ata(USDC_MINT)), OPEN_FEE, "no liquidation fees");
    assert_eq!(m.ctx.token_balance(m.global_usdc_vault), 0);
    assert_eq!(m.ctx.token_balance(m.vault_token_account), 0);

    let vault: short_stake001::MarketVault = m.ctx.fetch(m.market_vault);
    assert_eq!(vault.total_staked_amount, 0);
    assert_eq!(vault.total_borrowed_amount, 0);
}

/// With another loan still open on the offer, the write-off can't delete the offer (that loan
/// still needs it). The offer is closed to new borrowing instead; once the other loan settles,
/// the staker withdraws the remainder with early_unstake, which closes the account.
#[test]
fn write_off_with_other_open_loan_closes_offer_to_new_borrows() {
    let mut m = new_market(1_200);
    let bad = m.open_loan(1, COLLATERAL);
    let healthy = m.open_loan(2, 400_000_000); // stays healthy at $4
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    m.liquidate(&bad, None).0.expect("write-off should succeed");

    let offer: short_stake001::LenderOffer = m.ctx.fetch(m.lender_offer);
    assert!(offer.is_closed);
    assert_eq!(offer.current_active_borrows, 1);
    assert_eq!(offer.available_to_borrow, 0);
    assert_eq!(offer.total_offered, BORROW, "only the healthy loan's tokens remain");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - 2 * BORROW);

    // The healthy loan settles normally; its tokens return to the (closed) offer.
    m.close_loan(&healthy).expect("the other loan can still close");

    // Even with tokens back in the offer, nobody can borrow from it again.
    let newcomer = m.new_borrower(3);
    let ix = m.open_ix(&newcomer, 3, COLLATERAL * 2);
    let result = m.ctx.send_with_price(&newcomer.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[]);
    assert_custom_error(result, ERR_OFFER_CLOSED);

    // The staker withdraws the rest; early_unstake closes the offer account.
    let staker = m.staker.insecure_clone();
    m.ctx
        .send(
            &staker,
            ix_early_unstake(
                m.staker_pk,
                m.staker_pk, // payer_wallet = market_vault.payer (the first staker)
                CUSTOM_ORACLE_TOKEN_MINT,
                m.staker_token_account,
                m.market_vault,
                m.vault_token_account,
                m.lender_offer,
            ),
            &[],
        )
        .expect("early_unstake should drain the closed offer");
    assert!(!m.ctx.account_exists(m.lender_offer));
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - BORROW, "everything but the written-off tokens");
}

/// Collateral covers the liquidator's buyback + bounty but not everything: the liquidator is
/// paid in full first, the staker gets what's left of their interest, the borrower nothing.
/// (The old order paid staker interest first and shorted the liquidator's bounty.)
#[test]
fn bad_debt_buyback_pays_liquidator_before_staker_interest() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    // $3.07: notional $153.50 -> liquidator needs $153.50 + $6.14 bounty = $159.64 of the $160.
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 3_070_000_000_000);

    let (result, liquidator_token_account, liquidator_usdc_account) = m.liquidate(&loan, None);
    result.expect("liquidation should succeed");

    assert_eq!(m.ctx.token_balance(liquidator_token_account), 0, "buyback happened");
    assert_eq!(m.ctx.token_balance(liquidator_usdc_account), 159_640_000, "liquidator made whole");
    // $0.36 left for interest (owed $0.7675); 5% protocol fee on what was actually paid.
    assert_eq!(m.staker_usdc(), 342_000);
    assert_eq!(m.ctx.token_balance(fee_collector_usdc_ata(USDC_MINT)), OPEN_FEE + 18_000);
    assert_eq!(m.ctx.token_balance(loan.borrower_usdc_account), 500_000_000 - COLLATERAL - OPEN_FEE);
    assert_eq!(m.ctx.token_balance(m.global_usdc_vault), 0);

    let offer: short_stake001::LenderOffer = m.ctx.fetch(m.lender_offer);
    assert!(!offer.is_closed, "a buyback liquidation leaves the offer open");
    assert_eq!(offer.available_to_borrow, STAKE);
}

/// Since the Slot Zero H-01 fix, payouts may go to ANY USDC account the staker owns (an
/// ATA-only rule let a counterparty veto settlement). The full attack tests are in
/// test_slotzero_audit.rs; this keeps the payout itself covered.
#[test]
fn liquidate_pays_a_non_ata_usdc_account_the_staker_owns() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let side_account = m.ctx.create_token_account(random_pubkey(), m.staker_pk, USDC_MINT, 0);
    let (result, _, _) = m.liquidate(&loan, Some(side_account));
    result.expect("write-off into a staker-owned non-ATA account should succeed");
    assert_eq!(m.ctx.token_balance(side_account), COLLATERAL, "staker's side account gets the collateral");
    assert!(!m.ctx.account_exists(loan.borrow_position), "position closed");
}

/// 12 bps/day used to compute an hourly rate of 12 / 24 = 0, so the staker earned nothing.
/// Dividing by 24 last: $100 * 0.12% for 24h = $0.12 interest, $0.114 to the staker after the 5% fee.
#[test]
fn sub_24_bps_daily_rate_still_accrues_interest() {
    let mut m = new_market(12);
    let loan = m.open_loan(1, COLLATERAL);
    let now = m.ctx.unix_timestamp();
    m.ctx.warp_unix_timestamp(now + 86_400);

    m.close_loan(&loan).expect("close_position should succeed");

    assert_eq!(m.staker_usdc(), 114_000);
    assert_eq!(m.ctx.token_balance(fee_collector_usdc_ata(USDC_MINT)), OPEN_FEE + 6_000);
}
