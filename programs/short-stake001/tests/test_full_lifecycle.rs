mod common;

use common::*;

const CUSTOM_ORACLE_TOKEN_MINT: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");
const USDC_MINT: anchor_lang::prelude::Pubkey = short_stake001::constants::usdc::ID;

const TOKEN_DECIMALS: u8 = 6;
const USDC_DECIMALS: u8 = 6;

/// Exercises the full happy-path lifecycle in one flow, since every later
/// instruction depends on state created by the ones before it:
/// initialize -> initialize_global_vault -> (signed custom price) deposit_stake
/// -> open_borrow_position -> close_position.
#[test]
fn full_lifecycle_happy_path() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());

    // ---- initialize ----
    let (global_config, _) = global_config_pda();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    // ---- initialize_global_vault ----
    ctx.create_mint(USDC_MINT, USDC_DECIMALS, admin_pk);
    let (global_usdc_vault, _) = global_usdc_vault_pda();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize_global_vault(admin_pk, global_config, USDC_MINT, global_usdc_vault),
        &[],
    )
    .expect("initialize_global_vault should succeed");

    // ---- custom oracle price ----
    // Not in the Pyth allow-list, so every priced instruction below is preceded by an
    // Ed25519 instruction carrying this price signed by ctx.oracle_signer.
    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, TOKEN_DECIMALS, admin_pk);
    let price_per_token_usdc: u64 = 2_000_000_000_000; // $2.00 in 10^-12 USD price units
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, price_per_token_usdc);

    // ---- deposit_stake ----
    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    let stake_amount: u64 = 500_000_000; // 500 tokens @ 6 decimals
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, stake_amount);

    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let offer_id: u64 = 1;
    let (lender_offer, _) = lender_offer_pda(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);

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
            1_200, // 12% interest_rate_bps
            30,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    assert_eq!(ctx.token_balance(staker_token_account), 0);
    assert_eq!(ctx.token_balance(vault_token_account), stake_amount);

    // ---- open_borrow_position ----
    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());

    let borrow_amount: u64 = 100_000_000; // 100 tokens
    // notional = 100 * $2.00 = $200; required margin @150% = $300
    let provided_collateral_usdc: u64 = 320_000_000;

    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, USDC_MINT, 500_000_000);

    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    ctx.send_with_price(
        &borrower,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_open_borrow_position(
            borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            USDC_MINT,
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

    // Borrow fee: 25 bps of the $200 notional = $0.50, paid on top of the collateral.
    let borrower_fee: u64 = 500_000;
    let fee_collector_usdc_account = fee_collector_usdc_ata(USDC_MINT);
    assert_eq!(ctx.token_balance(borrower_token_account), borrow_amount);
    assert_eq!(
        ctx.token_balance(borrower_usdc_account),
        500_000_000 - provided_collateral_usdc - borrower_fee
    );
    assert_eq!(ctx.token_balance(global_usdc_vault), provided_collateral_usdc);
    assert_eq!(ctx.token_balance(fee_collector_usdc_account), borrower_fee);

    // ---- close_position ----
    let staker_usdc_account = ctx.create_ata(staker_pk, USDC_MINT, 0);

    ctx.send_with_price(
        &borrower,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_close_position(
            borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            USDC_MINT,
            borrower_token_account,
            market_vault,
            vault_token_account,
            global_usdc_vault,
            global_config,
            staker_usdc_account,
            borrower_usdc_account,
            lender_offer,
            borrow_position,
        ),
        &[],
    )
    .expect("close_position should succeed");

    // Borrowed tokens returned to the vault, staker earned some interest,
    // and the borrower got back collateral minus that interest.
    assert_eq!(ctx.token_balance(vault_token_account), stake_amount);
    assert_eq!(ctx.token_balance(borrower_token_account), 0);
    assert!(ctx.token_balance(staker_usdc_account) > 0);

    // The borrower paid the full interest out of collateral; it was split 95% staker / 5% protocol.
    let borrower_after_open = 500_000_000 - provided_collateral_usdc - borrower_fee;
    let interest = provided_collateral_usdc - (ctx.token_balance(borrower_usdc_account) - borrower_after_open);
    let staker_fee = interest * 500 / 10_000;
    assert!(staker_fee > 0, "test interest should be large enough to produce a fee");
    assert_eq!(ctx.token_balance(staker_usdc_account), interest - staker_fee);
    assert_eq!(ctx.token_balance(fee_collector_usdc_account), borrower_fee + staker_fee);
    assert_eq!(ctx.token_balance(global_usdc_vault), 0, "vault holds nothing after close");
}
