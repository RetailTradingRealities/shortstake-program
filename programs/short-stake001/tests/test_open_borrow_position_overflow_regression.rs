mod common;

use common::*;

const CUSTOM_ORACLE_TOKEN_MINT: anchor_lang::prelude::Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");

/// Regression test for the `initial_liquidation_price_usdc` overflow: on a 9-decimal
/// mint (like SOL), `(collateral + notional) * token_divisor` used to be computed in
/// u64 and would panic via `.unwrap()` for any position with combined collateral +
/// notional above ~$18.4k (i.e. any short over ~$7k notional at the required 150%
/// margin). This opens a $100,000-notional position and expects it to succeed.
#[test]
fn open_borrow_position_handles_large_notional_on_high_decimal_mint() {
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

    // 9 decimals, like SOL - this is what makes `token_divisor` large enough to overflow.
    const TOKEN_DECIMALS: u8 = 9;
    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, TOKEN_DECIMALS, admin_pk);
    let price_per_token_usdc: u64 = 1_000_000_000_000; // $1.00
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, price_per_token_usdc);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    // 200,000 tokens @ 9 decimals.
    let stake_amount: u64 = 200_000_000_000_000;
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

    // 100,000 tokens @ $1.00 = $100,000 notional; required margin @150% = $150,000.
    let borrow_amount: u64 = 100_000_000_000_000;
    let provided_collateral_usdc: u64 = 160_000_000_000; // $160,000

    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 200_000_000_000);
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
    .expect("a $100k-notional short on a 9-decimal mint must not overflow/panic");

    let position: short_stake001::BorrowPosition = ctx.fetch(borrow_position);
    // Liquidation price should sit above the $1.00 entry (shorts get liquidated as
    // price rises), and must be a plausible, non-garbage value.
    assert!(position.initial_liquidation_price_usd > price_per_token_usdc);
    assert!(position.initial_liquidation_price_usd < 10 * price_per_token_usdc);
}

/// Same overflow class as above, but for `close_position`'s `current_notional_usdc`
/// and `accrued_interest_usdc` calculations (`amount_borrowed * price` and
/// `notional * hourly_rate_bps * billable_hours`, both previously plain u64 math).
#[test]
fn close_position_handles_large_notional_on_high_decimal_mint() {
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

    const TOKEN_DECIMALS: u8 = 9;
    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, TOKEN_DECIMALS, admin_pk);
    let price_per_token_usdc: u64 = 1_000_000_000_000; // $1.00
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, price_per_token_usdc);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    let stake_amount: u64 = 200_000_000_000_000;
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
    let borrow_amount: u64 = 100_000_000_000_000; // $100,000 notional @ $1.00
    let provided_collateral_usdc: u64 = 160_000_000_000;
    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 200_000_000_000);
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

    ctx.send_with_price(
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
            borrower_usdc_account,
            lender_offer,
            borrow_position,
        ),
        &[],
    )
    .expect("closing a $100k-notional position on a 9-decimal mint must not overflow/panic");

    assert_eq!(ctx.token_balance(vault_token_account), stake_amount);
    assert!(ctx.token_balance(staker_usdc_account) > 0, "staker should earn interest");
}

/// Same overflow class again, this time exercised through `liquidate_position`'s
/// `current_notional_usdc`/`initial_notional_usdc`/`accrued_interest_usdc` math.
#[test]
fn liquidate_position_handles_large_notional_on_high_decimal_mint() {
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

    const TOKEN_DECIMALS: u8 = 9;
    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, TOKEN_DECIMALS, admin_pk);
    let price_per_token_usdc: u64 = 1_000_000_000_000; // $1.00
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, price_per_token_usdc);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    let stake_amount: u64 = 200_000_000_000_000;
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, stake_amount);
    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let offer_id: u64 = 1;
    let (lender_offer, _) = lender_offer_pda(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);

    // max_expiry_days = 1 so the loan can be pushed past expiry quickly.
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
    let borrow_amount: u64 = 100_000_000_000_000; // $100,000 notional @ $1.00
    let provided_collateral_usdc: u64 = 160_000_000_000;
    let borrower_token_account = ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = ctx.create_ata(borrower_pk, usdc_mint, 200_000_000_000);
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

    ctx.warp_unix_timestamp(2 * 24 * 60 * 60);

    let liquidator = ctx.new_funded_keypair(10_000_000_000);
    let liquidator_pk = to_anchor(liquidator.pubkey());
    let liquidator_token_account = ctx.create_ata(liquidator_pk, CUSTOM_ORACLE_TOKEN_MINT, borrow_amount);
    let liquidator_usdc_account = ctx.create_ata(liquidator_pk, usdc_mint, 0);
    let staker_usdc_account = ctx.create_ata(staker_pk, usdc_mint, 0);
    let fee_collector_usdc_account =
        ctx.create_ata(short_stake001::constants::FEE_COLLECTOR_PUBKEY, usdc_mint, 0);

    ctx.send_with_price(
        &liquidator,
        CUSTOM_ORACLE_TOKEN_MINT,
        ix_liquidate_position(
            liquidator_pk,
            borrower_pk,
            staker_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            usdc_mint,
            liquidator_token_account,
            market_vault,
            vault_token_account,
            global_usdc_vault,
            global_config,
            staker_usdc_account,
            liquidator_usdc_account,
            fee_collector_usdc_account,
            borrower_usdc_account,
            lender_offer,
            borrow_position,
        ),
        &[],
    )
    .expect("liquidating a $100k-notional position on a 9-decimal mint must not overflow/panic");

    assert_eq!(ctx.token_balance(vault_token_account), stake_amount);
    assert!(ctx.token_balance(liquidator_usdc_account) > 0);
    assert!(ctx.token_balance(fee_collector_usdc_account) > 0);
}
