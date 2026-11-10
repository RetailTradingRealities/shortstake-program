mod common;

use common::*;

use anchor_lang::prelude::Pubkey;

const CUSTOM_ORACLE_TOKEN_MINT: Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");

/// Everything needed to attempt `liquidate_position` against a single open loan,
/// shared across the before/at/after-expiry boundary tests below.
struct Scenario {
    ctx: TestCtx,
    usdc_mint: Pubkey,
    staker_pk: Pubkey,
    market_vault: Pubkey,
    vault_token_account: Pubkey,
    global_usdc_vault: Pubkey,
    global_config: Pubkey,
    lender_offer: Pubkey,
    borrower_pk: Pubkey,
    borrower_usdc_account: Pubkey,
    borrow_position: Pubkey,
    stake_amount: u64,
    borrow_amount: u64,
    /// `loan_start_timestamp + max_expiry_days * 86400`, read back from the
    /// position account so the boundary tests don't have to duplicate the math.
    expiry_timestamp: i64,
}

/// Opens a loan with `max_expiry_days = 1` and collateral healthy enough that
/// `is_undercollateralized` stays false, so expiry is the only liquidation trigger.
fn open_short_lived_position() -> Scenario {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let usdc_mint = short_stake001::constants::usdc::ID;
    let (global_config, _) = global_config_pda();

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    ctx.create_mint(usdc_mint, 6, admin_pk);
    let (global_usdc_vault, _) = global_usdc_vault_pda();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize_global_vault(admin_pk, global_config, usdc_mint, global_usdc_vault),
        &[],
    )
    .expect("initialize_global_vault should succeed");

    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, 6, admin_pk);
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 2_000_000_000_000);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    let stake_amount: u64 = 500_000_000;
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, stake_amount);
    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let offer_id: u64 = 1;
    let (lender_offer, _) = lender_offer_pda(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);

    // max_expiry_days = 1 (the global minimum) so the loan expires quickly.
    ctx.send_with_price(
        &staker,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_deposit_stake(
            staker_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            staker_token_account,
            market_vault,
            vault_token_account,
            lender_offer,
            global_config,
            stake_amount,
            1_200,
            1,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrow_amount: u64 = 50_000_000; // 50 tokens @ $2.00 = $100 notional
    let provided_collateral_usdc: u64 = 160_000_000; // comfortably above the 150% margin
    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 500_000_000);
    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    ctx.send_with_price(
        &borrower,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_open_borrow_position(
            borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            usdc_mint,
            market_vault,
            vault_token_account,
            global_usdc_vault,
            global_config,
            lender_offer,
            borrower_token_account,
            borrower_usdc_account,
            borrow_position,
            borrow_amount,
            position_id,
            provided_collateral_usdc,
        ),
        &[],
    )
    .expect("open_borrow_position should succeed");

    let position: short_stake001::BorrowPosition = ctx.fetch(borrow_position);

    Scenario {
        ctx,
        usdc_mint,
        staker_pk,
        market_vault,
        vault_token_account,
        global_usdc_vault,
        global_config,
        lender_offer,
        borrower_pk,
        borrower_usdc_account,
        borrow_position,
        stake_amount,
        borrow_amount,
        expiry_timestamp: position.expiry_timestamp,
    }
}

fn attempt_liquidate(s: &mut Scenario) -> Result<(), String> {
    let liquidator = s.ctx.new_funded_keypair(10_000_000_000);
    let liquidator_pk = to_anchor(liquidator.pubkey());
    let liquidator_token_account = s.ctx.create_ata(liquidator_pk, CUSTOM_ORACLE_TOKEN_MINT, s.borrow_amount);
    let liquidator_usdc_account = s.ctx.create_ata(liquidator_pk, s.usdc_mint, 0);
    let staker_usdc_account = s.ctx.create_ata(s.staker_pk, s.usdc_mint, 0);
    let fee_collector_usdc_account =
        s.ctx.create_ata(short_stake001::constants::FEE_COLLECTOR_PUBKEY, s.usdc_mint, 0);

    s.ctx.send_with_price(
        &liquidator,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_liquidate_position(
            liquidator_pk,
            s.borrower_pk,
            s.staker_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            s.usdc_mint,
            liquidator_token_account,
            s.market_vault,
            s.vault_token_account,
            s.global_usdc_vault,
            s.global_config,
            staker_usdc_account,
            liquidator_usdc_account,
            fee_collector_usdc_account,
            s.borrower_usdc_account,
            s.lender_offer,
            s.borrow_position,
        ),
        &[],
    )
}

/// A healthy position that hasn't reached its expiry yet must not be liquidatable.
#[test]
fn liquidate_position_rejects_before_expiry_when_healthy() {
    let mut s = open_short_lived_position();
    s.ctx.warp_unix_timestamp(s.expiry_timestamp - 1);

    let result = attempt_liquidate(&mut s);
    assert!(
        result.is_err(),
        "a healthy, not-yet-expired position must not be liquidatable"
    );

    // State must be untouched: the position is still open with its single borrow.
    let offer: short_stake001::LenderOffer = s.ctx.fetch(s.lender_offer);
    assert_eq!(offer.current_active_borrows, 1);
    assert!(s.ctx.lamports(s.borrow_position) > 0);
}

/// `current_time >= expiry_timestamp` — equality must already count as expired.
#[test]
fn liquidate_position_succeeds_exactly_at_expiry() {
    let mut s = open_short_lived_position();
    s.ctx.warp_unix_timestamp(s.expiry_timestamp);

    attempt_liquidate(&mut s).expect("liquidation exactly at expiry should succeed");

    assert_eq!(s.ctx.token_balance(s.vault_token_account), s.stake_amount);
    assert_eq!(s.ctx.lamports(s.borrow_position), 0);
}

/// One second past expiry is unambiguously liquidatable.
#[test]
fn liquidate_position_succeeds_just_after_expiry() {
    let mut s = open_short_lived_position();
    s.ctx.warp_unix_timestamp(s.expiry_timestamp + 1);

    attempt_liquidate(&mut s).expect("liquidation just after expiry should succeed");

    assert_eq!(s.ctx.token_balance(s.vault_token_account), s.stake_amount);
    assert_eq!(s.ctx.lamports(s.borrow_position), 0);
}
