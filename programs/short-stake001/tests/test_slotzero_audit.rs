//! Regression tests for the Slot Zero AI audit (2026-11-07): each attack from the report, now
//! refused or harmless. H-01 uses a REAL SPL Token SetAuthority (not a ledger edit), as the
//! report's proof of concept did. Run: anchor build -- --features devnet && cargo test --features devnet
mod common;

use common::market::*;
use common::*;

use anchor_lang::prelude::Pubkey;

// ---- H-01: a counterparty can't veto settlement by handing its ATA to another key -----------

/// Borrower hands its USDC ATA to a key it controls. Its ATA can no longer be paid, but a
/// liquidator pays a fresh USDC account the borrower owns, and the liquidation goes through.
#[test]
fn h01_borrower_reassigning_its_usdc_ata_cannot_block_liquidation() {
    // 12 bps/day, so 31 days of interest leave a refund for the borrower.
    let mut m = new_market(12);
    let loan = m.open_loan(1, COLLATERAL);
    let accomplice = random_pubkey();
    m.reassign_token_account(&loan.borrower.insecure_clone(), loan.borrower_usdc_account, accomplice)
        .expect("SetAuthority by the owner");

    // Past expiry at an unchanged price: the buyback path, which refunds the borrower.
    let now = m.ctx.unix_timestamp();
    m.ctx.warp_unix_timestamp(now + 31 * 86_400);

    // The old ATA no longer belongs to the borrower: paying it is refused...
    let (result, _, _) = m.liquidate(&loan, None);
    assert_custom_error(result, ERR_CONSTRAINT_TOKEN_OWNER);
    assert!(m.ctx.account_exists(loan.borrow_position), "position still open");

    // ...but anyone can create another account the borrower owns, so the attack fails.
    let refund_account = m.ctx.create_token_account(random_pubkey(), loan.borrower_pk, USDC_MINT, 0);
    let (result, _, _) = m.liquidate_with(&loan, None, Some(refund_account));
    result.expect("liquidation into a fresh borrower-owned account should succeed");
    assert!(!m.ctx.account_exists(loan.borrow_position), "position liquidated");
    assert!(m.ctx.token_balance(refund_account) > 0, "the borrower's refund lands in its own account");
}

/// The same on the write-off path (price gapped up so collateral can't cover the buyback).
#[test]
fn h01_borrower_reassigning_its_usdc_ata_cannot_block_write_off() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.reassign_token_account(&loan.borrower.insecure_clone(), loan.borrower_usdc_account, random_pubkey())
        .expect("SetAuthority by the owner");
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let refund_account = m.ctx.create_token_account(random_pubkey(), loan.borrower_pk, USDC_MINT, 0);
    let (result, _, _) = m.liquidate_with(&loan, None, Some(refund_account));
    result.expect("write-off should succeed");
    assert_eq!(m.staker_usdc(), COLLATERAL, "staker receives all collateral");
}

/// Staker hands its USDC ATA away to stop its borrowers closing: the borrower closes by paying
/// the interest to another USDC account the staker owns.
#[test]
fn h01_staker_reassigning_its_usdc_ata_cannot_block_close() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    let staker_ata = m.ctx.create_ata(m.staker_pk, USDC_MINT, 0);
    m.reassign_token_account(&m.staker.insecure_clone(), staker_ata, random_pubkey())
        .expect("SetAuthority by the owner");
    let now = m.ctx.unix_timestamp();
    m.ctx.warp_unix_timestamp(now + 86_400);

    assert_custom_error(m.close_loan_with(&loan, staker_ata), ERR_CONSTRAINT_TOKEN_OWNER);

    let interest_account = m.ctx.create_token_account(random_pubkey(), m.staker_pk, USDC_MINT, 0);
    m.close_loan_with(&loan, interest_account).expect("close into a fresh staker-owned account should succeed");
    assert!(!m.ctx.account_exists(loan.borrow_position), "position closed");
    assert!(m.ctx.token_balance(interest_account) > 0, "interest lands in the staker's own account");
}

/// ...and it can't block a third-party liquidation either.
#[test]
fn h01_staker_reassigning_its_usdc_ata_cannot_block_liquidation() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    let staker_ata = m.ctx.create_ata(m.staker_pk, USDC_MINT, 0);
    m.reassign_token_account(&m.staker.insecure_clone(), staker_ata, random_pubkey())
        .expect("SetAuthority by the owner");
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let payout_account = m.ctx.create_token_account(random_pubkey(), m.staker_pk, USDC_MINT, 0);
    let (result, _, _) = m.liquidate(&loan, Some(payout_account));
    result.expect("liquidation into a fresh staker-owned account should succeed");
    assert_eq!(m.ctx.token_balance(payout_account), COLLATERAL);
}

/// The staker hands its TOKEN ATA away too: a write-off returns the unborrowed tokens to a fresh
/// token account the staker owns.
#[test]
fn h01_staker_reassigning_its_token_ata_cannot_block_write_off() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.reassign_token_account(&m.staker.insecure_clone(), m.staker_token_account, random_pubkey())
        .expect("SetAuthority by the owner");
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let (result, _, _) = m.liquidate_full(&loan, None, Some(m.staker_token_account), None);
    assert_custom_error(result, ERR_CONSTRAINT_TOKEN_OWNER);

    let refund = m.ctx.create_token_account(random_pubkey(), m.staker_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let (result, _, _) = m.liquidate_full(&loan, None, Some(refund), None);
    result.expect("write-off into a fresh staker-owned token account should succeed");
    assert_eq!(m.ctx.token_balance(refund), STAKE - BORROW, "unborrowed tokens back to the staker");
}

/// staker_token_account is optional: a buyback, which never uses it, works without it...
#[test]
fn h01_buyback_needs_no_staker_token_account() {
    let mut m = new_market(12);
    let loan = m.open_loan(1, COLLATERAL);
    let now = m.ctx.unix_timestamp();
    m.ctx.warp_unix_timestamp(now + 31 * 86_400); // expired, unchanged price: buyback path
    let (result, _, _) = m.liquidate_full(&loan, None, None, None);
    result.expect("buyback without a staker token account");
    assert!(!m.ctx.account_exists(loan.borrow_position));
}

/// ...while a write-off that has unborrowed tokens to return needs one, and says so.
#[test]
fn h01_write_off_without_staker_token_account_is_refused_clearly() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);
    let (result, _, _) = m.liquidate_full(&loan, None, None, None);
    assert_custom_error(result, ERR_MISSING_STAKER_TOKEN_ACCOUNT);
    assert!(m.ctx.account_exists(loan.borrow_position), "position untouched");
}

/// The owner check still holds: an account owned by anyone else is refused.
#[test]
fn h01_payout_account_owned_by_someone_else_is_refused() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);

    let thief_account = m.ctx.create_token_account(random_pubkey(), random_pubkey(), USDC_MINT, 0);
    let (result, _, _) = m.liquidate(&loan, Some(thief_account));
    assert_custom_error(result, ERR_CONSTRAINT_TOKEN_OWNER);
    let (result, _, _) = m.liquidate_with(&loan, None, Some(thief_account));
    assert_custom_error(result, ERR_CONSTRAINT_TOKEN_OWNER);
    assert!(m.ctx.account_exists(loan.borrow_position), "position untouched");
}

// ---- H-01 (USDC market), I-07, I-09: tokens that can't form a market ------------------------

fn deposit_new_mint(m: &mut Market, mint: Pubkey, offer_id: u64) -> Result<(), String> {
    m.ctx.set_custom_price(mint, PRICE_2);
    let staker_token_account = m.ctx.create_ata(m.staker_pk, mint, STAKE);
    let ix = ix_deposit_stake(
        m.staker_pk,
        mint,
        staker_token_account,
        market_vault_pda(mint).0,
        vault_token_account_pda(mint).0,
        lender_offer_pda(m.staker_pk, mint, offer_id).0,
        m.global_config,
        STAKE,
        1_200,
        30,
        offer_id,
    );
    let staker = m.staker.insecure_clone();
    m.ctx.send_with_price(&staker, mint, ix, &[])
}

#[test]
fn usdc_cannot_be_a_market_token() {
    let mut m = new_market(1_200);
    assert_custom_error(deposit_new_mint(&mut m, USDC_MINT, 7), ERR_UNSUPPORTED_MARKET_TOKEN);
}

#[test]
fn freezable_mint_cannot_form_a_market() {
    let mut m = new_market(1_200);
    let admin_pk = to_anchor(m.ctx.admin.pubkey());
    let mint = random_pubkey();
    m.ctx.create_mint_with_freeze_authority(mint, 6, admin_pk, Some(random_pubkey()));
    assert_custom_error(deposit_new_mint(&mut m, mint, 7), ERR_UNSUPPORTED_MARKET_TOKEN);

    let unfreezable = random_pubkey();
    m.ctx.create_mint(unfreezable, 6, admin_pk);
    deposit_new_mint(&mut m, unfreezable, 8).expect("a mint without a freeze authority is fine");
}

#[test]
fn mint_with_too_many_decimals_cannot_form_a_market() {
    let mut m = new_market(1_200);
    let admin_pk = to_anchor(m.ctx.admin.pubkey());
    let mint = random_pubkey();
    m.ctx.create_mint(mint, short_stake001::constants::MAX_MARKET_TOKEN_DECIMALS + 1, admin_pk);
    assert_custom_error(deposit_new_mint(&mut m, mint, 7), ERR_UNSUPPORTED_MARKET_TOKEN);

    let ok_mint = random_pubkey();
    m.ctx.create_mint(ok_mint, short_stake001::constants::MAX_MARKET_TOKEN_DECIMALS, admin_pk);
    deposit_new_mint(&mut m, ok_mint, 8).expect("the maximum decimals are accepted");
}

// ---- M-02: an open only lands on the terms the borrower accepted -----------------------------

fn open_with_terms(m: &mut Market, loan: &Loan, max_rate: u16, min_expiry_days: u8) -> Result<(), String> {
    let ix = ix_open_borrow_position_with_terms(
        loan.borrower_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        USDC_MINT,
        m.market_vault,
        m.vault_token_account,
        m.global_usdc_vault,
        m.global_config,
        m.lender_offer,
        loan.borrower_token_account,
        loan.borrower_usdc_account,
        loan.borrow_position,
        BORROW,
        1,
        COLLATERAL,
        max_rate,
        min_expiry_days,
    );
    m.ctx.send_with_price(&loan.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[])
}

/// The offer (1,200 bps/day, 30 days) is exactly what the borrower saw: the open lands.
#[test]
fn m02_open_on_the_terms_shown_succeeds() {
    let mut m = new_market(1_200);
    let loan = m.new_borrower(1);
    open_with_terms(&mut m, &loan, 1_200, 30).expect("open on the offer's own terms");
}

#[test]
fn m02_open_refused_when_rate_is_above_the_borrowers_max() {
    let mut m = new_market(1_200);
    let loan = m.new_borrower(1);
    assert_custom_error(open_with_terms(&mut m, &loan, 1_199, 30), ERR_OFFER_TERMS_CHANGED);
}

#[test]
fn m02_open_refused_when_expiry_is_below_the_borrowers_min() {
    let mut m = new_market(1_200);
    let loan = m.new_borrower(1);
    assert_custom_error(open_with_terms(&mut m, &loan, 1_200, 31), ERR_OFFER_TERMS_CHANGED);
}

/// The attack itself: the staker closes the offer and re-creates it at the same address at
/// 65,535 bps/day for 1 day before the borrower's open lands. The open is now refused.
#[test]
fn m02_switched_offer_terms_are_refused() {
    let mut m = new_market(1_200);
    let loan = m.new_borrower(1);

    m.early_unstake().expect("no loans: early_unstake closes the offer");
    assert!(!m.ctx.account_exists(m.lender_offer), "offer account closed");
    let ix = ix_deposit_stake(
        m.staker_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        m.staker_token_account,
        m.market_vault,
        m.vault_token_account,
        m.lender_offer,
        m.global_config,
        STAKE,
        u16::MAX,
        1,
        1, // same offer ID, so the same address
    );
    let staker = m.staker.insecure_clone();
    m.ctx.svm.expire_blockhash();
    m.ctx.send_with_price(&staker, CUSTOM_ORACLE_TOKEN_MINT, ix, &[]).expect("re-deposit with worse terms");

    assert_custom_error(open_with_terms(&mut m, &loan, 1_200, 30), ERR_OFFER_TERMS_CHANGED);
    assert!(!m.ctx.account_exists(loan.borrow_position), "no position opened");
}

// ---- L-04: a fully lent offer can still be closed to new borrowing ---------------------------

#[test]
fn l04_fully_lent_offer_can_be_closed_to_new_borrowing() {
    let mut m = new_market(1_200);
    let loan = m.new_borrower(1);
    // Borrow the whole offer: 500 tokens = $1,000 at $2, so $1,500+ collateral and the fee.
    m.ctx.create_ata(loan.borrower_pk, USDC_MINT, 2_000_000_000);
    let ix = ix_open_borrow_position(
        loan.borrower_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        USDC_MINT,
        m.market_vault,
        m.vault_token_account,
        m.global_usdc_vault,
        m.global_config,
        m.lender_offer,
        loan.borrower_token_account,
        loan.borrower_usdc_account,
        loan.borrow_position,
        STAKE,
        1,
        1_600_000_000,
    );
    m.ctx
        .send_with_price(&loan.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[])
        .expect("borrow the whole offer");
    assert_eq!(m.offer().available_to_borrow, 0);

    m.early_unstake().expect("a fully lent offer can be closed to new borrowing");
    let offer = m.offer();
    assert!(offer.is_closed, "offer closed to new borrowing");
    assert_eq!(offer.current_active_borrows, 1, "the loan stays open");

    // A repeat call with nothing to withdraw and the offer already closed still fails.
    assert_custom_error(m.early_unstake(), ERR_NOTHING_TO_WITHDRAW);
}

// ---- I-04: expiry bounds --------------------------------------------------------------------

#[test]
fn i04_update_config_rejects_bad_expiry_bounds() {
    let mut m = new_market(1_200);
    let admin = m.ctx.admin.insecure_clone();
    let admin_pk = to_anchor(admin.pubkey());
    let update = |min: Option<u8>, max: Option<u8>| ix_update_config(admin_pk, m.global_config, None, None, None, min, max, None);

    let zero_min = update(Some(0), None);
    let inverted = update(Some(10), Some(5));
    let above_max = update(Some(31), None); // max is 30
    let both_moved = update(Some(40), Some(60));
    for ix in [zero_min, inverted, above_max] {
        m.ctx.svm.expire_blockhash();
        assert_custom_error(m.ctx.send(&admin, ix, &[]), ERR_INVALID_EXPIRY_BOUNDS);
    }
    m.ctx.svm.expire_blockhash();
    m.ctx.send(&admin, both_moved, &[]).expect("moving min and max together in one call is fine");
}

#[test]
fn i04_initialize_rejects_bad_expiry_bounds() {
    let mut ctx = setup();
    let admin = ctx.admin.insecure_clone();
    let admin_pk = to_anchor(admin.pubkey());
    let (global_config, _) = global_config_pda();
    let ix = ix_initialize(admin_pk, global_config, 500, 25, 11_000, 0, 30, ctx.oracle_signer_pk());
    assert_custom_error(ctx.send(&admin, ix, &[]), ERR_INVALID_EXPIRY_BOUNDS);
}
