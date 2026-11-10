mod common;

use common::*;

const CUSTOM_ORACLE_TOKEN_MINT: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");

#[test]
fn initialize_rejects_non_admin_signer() {
    let mut ctx = setup();
    let intruder = ctx.new_funded_keypair(10_000_000_000);
    let intruder_pk = to_anchor(intruder.pubkey());
    let (global_config, _) = global_config_pda();

    let result = ctx.send(
        &intruder,
        ix_initialize(intruder_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    );
    assert!(result.is_err(), "initialize should reject a non-admin signer");
}

#[test]
fn open_borrow_position_rejects_insufficient_collateral() {
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
            30,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 500_000_000);
    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    // notional = 100 tokens * $2.00 = $200; required margin @150% = $300.
    // Offering only $100 must be rejected.
    let result = ctx.send_with_price(
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
            100_000_000,
            position_id,
            100_000_000,
        ),
        &[],
    );
    assert!(
        result.is_err(),
        "open_borrow_position should reject undercollateralized requests"
    );
}

/// Regression test for the missing `token::mint`/`token::authority` constraints on
/// `borrower_token_account`/`borrower_usdc_account`: without them, the borrowed
/// tokens (and the collateral source) could be redirected to any account of the
/// right mint regardless of ownership.
#[test]
fn open_borrow_position_rejects_borrower_token_account_owned_by_someone_else() {
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
            30,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 500_000_000);
    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    // An unrelated third party's token account, of the right mint but the wrong owner.
    let attacker_pk = random_pubkey();
    let attacker_token_account = ctx.create_ata(attacker_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);

    let result = ctx.send_with_price(
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
            attacker_token_account,
            borrower_usdc_account,
            borrow_position,
            100_000_000,
            position_id,
            320_000_000,
        ),
        &[],
    );
    assert!(
        result.is_err(),
        "open_borrow_position should reject a borrower_token_account owned by someone else"
    );
}

/// Regression test for the missing constraint on `close_position`'s
/// `borrower_usdc_account`: without `token::mint`/`token::authority`, the refunded
/// collateral could be redirected to any USDC account regardless of ownership.
#[test]
fn close_position_rejects_borrower_usdc_account_owned_by_someone_else() {
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
            30,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrow_amount: u64 = 100_000_000;
    let provided_collateral_usdc: u64 = 320_000_000;
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

    let staker_usdc_account = ctx.create_ata(staker_pk, usdc_mint, 0);

    // An unrelated third party's USDC account - the refund must not be redirectable here.
    let attacker_pk = random_pubkey();
    let attacker_usdc_account = ctx.create_ata(attacker_pk, usdc_mint, 0);

    let result = ctx.send_with_price(
        &borrower,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_close_position(
            borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            usdc_mint,
            borrower_token_account,
            market_vault,
            vault_token_account,
            global_usdc_vault,
            global_config,
            staker_usdc_account,
            attacker_usdc_account,
            lender_offer,
            borrow_position,
        ),
        &[],
    );
    assert!(
        result.is_err(),
        "close_position should reject a borrower_usdc_account owned by someone else"
    );
}

/// Atomicity: the borrow fee is transferred *before* the borrowed tokens leave the vault, so a
/// borrow that fails afterwards (here: more tokens than the offer holds) must roll the fee back.
#[test]
fn failed_open_borrow_position_does_not_charge_borrow_fee() {
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

    let stake_amount: u64 = 500_000_000; // 500 tokens available
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
            1_200,
            30,
            offer_id,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    let borrower = ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let starting_usdc: u64 = 5_000_000_000;
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, starting_usdc);
    let fee_collector_usdc_account = fee_collector_usdc_ata(usdc_mint);
    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    // 600 tokens > 500 staked. notional = $1,200 -> margin $1,800 (we post $2,000), fee $3.
    // Collateral and fee transfers succeed, then the vault -> borrower transfer fails.
    let result = ctx.send_with_price(
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
            600_000_000,
            position_id,
            2_000_000_000,
        ),
        &[],
    );
    assert!(result.is_err(), "borrowing more than the offer holds must fail");

    assert_eq!(ctx.token_balance(borrower_usdc_account), starting_usdc, "no collateral or fee taken");
    assert_eq!(ctx.token_balance(fee_collector_usdc_account), 0, "fee must be rolled back");
    assert_eq!(ctx.token_balance(global_usdc_vault), 0);
    assert_eq!(ctx.token_balance(borrower_token_account), 0);
    assert!(!ctx.account_exists(borrow_position), "position must not be created");
}
