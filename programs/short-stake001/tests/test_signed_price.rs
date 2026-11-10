mod common;

use common::*;

use anchor_lang::prelude::Pubkey;
use short_stake001::signed_price::{
    PriceAction, MAX_SIGNED_PRICE_AGE_SECONDS, MAX_SIGNED_PRICE_FUTURE_SKEW_SECONDS,
};
use short_stake001::ShortStakeError;
use solana_instruction::Instruction;
use solana_keypair::Keypair;

const CUSTOM_ORACLE_TOKEN_MINT: Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");
const PRICE: u64 = 2_000_000_000_000; // $2.00
const NOW: i64 = 1_700_000_000;

struct Market {
    ctx: TestCtx,
    usdc_mint: Pubkey,
    global_config: Pubkey,
    global_usdc_vault: Pubkey,
    market_vault: Pubkey,
    vault_token_account: Pubkey,
    staker: Keypair,
    staker_pk: Pubkey,
    staker_token_account: Pubkey,
}

/// initialize + global vault + funded staker for a custom-oracle mint; nothing staked yet.
fn setup_market() -> Market {
    let mut ctx = setup();
    ctx.warp_unix_timestamp(NOW);
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
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, PRICE);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());
    // 1M tokens: enough to borrow the $10 minimum position even at sub-cent prices.
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, 1_000_000_000_000);
    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);

    Market {
        ctx,
        usdc_mint,
        global_config,
        global_usdc_vault,
        market_vault,
        vault_token_account,
        staker,
        staker_pk,
        staker_token_account,
    }
}

fn deposit_ix(m: &Market, offer_id: u64) -> Instruction {
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);
    ix_deposit_stake(
        m.staker_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        m.staker_token_account,
        m.market_vault,
        m.vault_token_account,
        lender_offer,
        m.global_config,
        600_000_000_000, // 600k tokens
        1_200,
        30,
        offer_id,
    )
}

fn deposit_with_ixs(m: &mut Market, ixs: Vec<Instruction>) -> Result<(), String> {
    let staker = m.staker.insecure_clone();
    m.ctx.send_ixs(&staker, ixs, &[])
}

/// deposit_stake price instruction signed by `signer` over an arbitrary (mint, publish_time).
fn price_ix(signer: &Keypair, token_mint: Pubkey, publish_time: i64) -> Instruction {
    price_ix_for(signer, token_mint, publish_time, PriceAction::DepositStake)
}

fn price_ix_for(
    signer: &Keypair,
    token_mint: Pubkey,
    publish_time: i64,
    action: PriceAction,
) -> Instruction {
    ed25519_ix(signer, &signed_price_message(token_mint, PRICE, publish_time, action))
}

fn assert_err(result: Result<(), String>, expected: ShortStakeError) {
    let code = u32::from(expected);
    let err = result.expect_err("transaction should have been rejected");
    assert!(
        err.contains(&format!("Custom({code})")),
        "expected custom error {code}, got {err}"
    );
}

#[test]
fn deposit_accepts_fresh_signed_price() {
    let mut m = setup_market();
    let price = price_ix(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, NOW);
    let deposit = deposit_ix(&m, 1);
    deposit_with_ixs(&mut m, vec![price, deposit]).expect("fresh signed price should be accepted");
}

#[test]
fn deposit_rejects_missing_price_instruction() {
    let mut m = setup_market();
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![deposit]), ShortStakeError::MissingCustomOracle);
}

#[test]
fn deposit_rejects_price_instruction_not_directly_before() {
    let mut m = setup_market();
    let price = price_ix(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, NOW);
    let deposit = deposit_ix(&m, 1);
    assert_err(
        deposit_with_ixs(&mut m, vec![deposit, price]),
        ShortStakeError::MissingCustomOracle,
    );
}

#[test]
fn deposit_rejects_stale_price() {
    let mut m = setup_market();
    let stale = NOW - MAX_SIGNED_PRICE_AGE_SECONDS - 1;
    let price = price_ix(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, stale);
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![price, deposit]), ShortStakeError::StaleOraclePrice);
}

#[test]
fn deposit_accepts_price_at_max_age() {
    let mut m = setup_market();
    let oldest_ok = NOW - MAX_SIGNED_PRICE_AGE_SECONDS;
    let price = price_ix(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, oldest_ok);
    let deposit = deposit_ix(&m, 1);
    deposit_with_ixs(&mut m, vec![price, deposit]).expect("price exactly at max age is still valid");
}

#[test]
fn deposit_rejects_price_too_far_in_future() {
    let mut m = setup_market();
    let future = NOW + MAX_SIGNED_PRICE_FUTURE_SKEW_SECONDS + 1;
    let price = price_ix(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, future);
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![price, deposit]), ShortStakeError::StaleOraclePrice);
}

#[test]
fn deposit_rejects_price_signed_by_wrong_key() {
    let mut m = setup_market();
    let impostor = Keypair::new();
    let price = price_ix(&impostor, CUSTOM_ORACLE_TOKEN_MINT, NOW);
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![price, deposit]), ShortStakeError::InvalidSignedPrice);
}

#[test]
fn deposit_rejects_price_signed_for_another_mint() {
    let mut m = setup_market();
    let price = price_ix(&m.ctx.oracle_signer, random_pubkey(), NOW);
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![price, deposit]), ShortStakeError::InvalidSignedPrice);
}

#[test]
fn deposit_rejects_price_signed_for_another_program() {
    let mut m = setup_market();
    let mut message =
        signed_price_message(CUSTOM_ORACLE_TOKEN_MINT, PRICE, NOW, PriceAction::DepositStake);
    message[0..32].copy_from_slice(random_pubkey().as_ref());
    let price = ed25519_ix(&m.ctx.oracle_signer, &message);
    let deposit = deposit_ix(&m, 1);
    assert_err(deposit_with_ixs(&mut m, vec![price, deposit]), ShortStakeError::InvalidSignedPrice);
}

/// A price the backend issued under one action's sourcing rules can't be reused for another.
#[test]
fn deposit_rejects_price_signed_for_another_action() {
    for action in [
        PriceAction::OpenBorrowPosition,
        PriceAction::ClosePosition,
        PriceAction::LiquidatePosition,
    ] {
        let mut m = setup_market();
        let price = price_ix_for(&m.ctx.oracle_signer, CUSTOM_ORACLE_TOKEN_MINT, NOW, action);
        let deposit = deposit_ix(&m, 1);
        assert_err(
            deposit_with_ixs(&mut m, vec![price, deposit]),
            ShortStakeError::InvalidSignedPrice,
        );
    }
}

#[test]
fn update_config_rotates_oracle_signer() {
    let mut m = setup_market();
    let admin = m.ctx.admin.insecure_clone();
    let admin_pk = to_anchor(admin.pubkey());
    let old_signer = m.ctx.oracle_signer.insecure_clone();
    let new_signer = Keypair::new();

    m.ctx
        .send(
            &admin,
            ix_update_config(
                admin_pk,
                m.global_config,
                None,
                None,
                None,
                None,
                None,
                Some(to_anchor(new_signer.pubkey())),
            ),
            &[],
        )
        .expect("authority should be able to rotate oracle_signer");

    let config: short_stake001::GlobalConfig = m.ctx.fetch(m.global_config);
    assert_eq!(config.oracle_signer, to_anchor(new_signer.pubkey()));

    let old_price = price_ix(&old_signer, CUSTOM_ORACLE_TOKEN_MINT, NOW);
    let deposit = deposit_ix(&m, 1);
    assert_err(
        deposit_with_ixs(&mut m, vec![old_price, deposit]),
        ShortStakeError::InvalidSignedPrice,
    );

    let new_price = price_ix(&new_signer, CUSTOM_ORACLE_TOKEN_MINT, NOW);
    let deposit = deposit_ix(&m, 2);
    deposit_with_ixs(&mut m, vec![new_price, deposit]).expect("new signer's price should be accepted");
}

/// A borrower can't close against an old price once it has aged out, but can with a fresh one.
#[test]
fn close_position_rejects_stale_price_then_accepts_fresh() {
    let mut m = setup_market();
    let offer_id = 1;
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);
    let price = m.ctx.signed_price_ix(CUSTOM_ORACLE_TOKEN_MINT, PriceAction::DepositStake);
    let deposit = deposit_ix(&m, offer_id);
    deposit_with_ixs(&mut m, vec![price, deposit]).expect("deposit_stake should succeed");

    let borrower = m.ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = m.ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = m.ctx.create_ata(borrower_pk, m.usdc_mint, 500_000_000);
    let position_id: u64 = 1;
    let (borrow_position, _) = borrow_position_pda(borrower_pk, position_id);

    m.ctx
        .send_with_price(
            &borrower,
            CUSTOM_ORACLE_TOKEN_MINT,
            ix_open_borrow_position(
                borrower_pk,
                CUSTOM_ORACLE_TOKEN_MINT,
                m.usdc_mint,
                m.market_vault,
                m.vault_token_account,
                m.global_usdc_vault,
                m.global_config,
                lender_offer,
                borrower_token_account,
                borrower_usdc_account,
                borrow_position,
                100_000_000,
                position_id,
                320_000_000,
            ),
            &[],
        )
        .expect("open_borrow_position should succeed");

    let staker_usdc_account = m.ctx.create_ata(m.staker_pk, m.usdc_mint, 0);
    let close_ix = ix_close_position(
        borrower_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        m.usdc_mint,
        borrower_token_account,
        m.market_vault,
        m.vault_token_account,
        m.global_usdc_vault,
        m.global_config,
        staker_usdc_account,
        borrower_usdc_account,
        lender_offer,
        borrow_position,
    );

    // Price signed at open time, replayed after it has aged out.
    let old_price = price_ix_for(
        &m.ctx.oracle_signer,
        CUSTOM_ORACLE_TOKEN_MINT,
        NOW,
        PriceAction::ClosePosition,
    );
    m.ctx.warp_unix_timestamp(NOW + MAX_SIGNED_PRICE_AGE_SECONDS + 1);
    assert_err(
        m.ctx.send_ixs(&borrower, vec![old_price, close_ix.clone()], &[]),
        ShortStakeError::StaleOraclePrice,
    );
    assert!(m.ctx.account_exists(borrow_position), "position must still be open");

    m.ctx
        .send_with_price(&borrower, CUSTOM_ORACLE_TOKEN_MINT, close_ix, &[])
        .expect("close_position with a fresh price should succeed");
    assert!(!m.ctx.account_exists(borrow_position));
}

/// A deposit-signed price (strict sourcing) can't be replayed to liquidate or close, and a
/// close-signed price (relaxed sourcing) can't open a position.
#[test]
fn open_borrow_rejects_price_signed_for_close() {
    let mut m = setup_market();
    let offer_id = 1;
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);
    let staker = m.staker.insecure_clone();
    let deposit = deposit_ix(&m, offer_id);
    m.ctx
        .send_with_price(&staker, CUSTOM_ORACLE_TOKEN_MINT, deposit, &[])
        .expect("deposit_stake should succeed");

    let borrower = m.ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = m.ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = m.ctx.create_ata(borrower_pk, m.usdc_mint, 500_000_000);
    let (borrow_position, _) = borrow_position_pda(borrower_pk, 1);
    let open_ix = ix_open_borrow_position(
        borrower_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        m.usdc_mint,
        m.market_vault,
        m.vault_token_account,
        m.global_usdc_vault,
        m.global_config,
        lender_offer,
        borrower_token_account,
        borrower_usdc_account,
        borrow_position,
        100_000_000,
        1,
        320_000_000,
    );

    let close_price = m
        .ctx
        .signed_price_ix(CUSTOM_ORACLE_TOKEN_MINT, PriceAction::ClosePosition);
    assert_err(
        m.ctx.send_ixs(&borrower, vec![close_price, open_ix], &[]),
        ShortStakeError::InvalidSignedPrice,
    );
}

/// Sub-cent tokens keep full precision: $0.00002 is 20_000_000 price units, not 20.
#[test]
fn open_borrow_prices_sub_cent_token_precisely() {
    let mut m = setup_market();
    let price: u64 = 20_000_000; // $0.00002
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, price);
    let offer_id = 1;
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, CUSTOM_ORACLE_TOKEN_MINT, offer_id);
    let staker = m.staker.insecure_clone();
    let deposit = deposit_ix(&m, offer_id);
    m.ctx
        .send_with_price(&staker, CUSTOM_ORACLE_TOKEN_MINT, deposit, &[])
        .expect("deposit_stake should succeed");

    let borrower = m.ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = m.ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0);
    let borrower_usdc_account = m.ctx.create_ata(borrower_pk, m.usdc_mint, 500_000_000);
    let (borrow_position, _) = borrow_position_pda(borrower_pk, 1);
    // 600k tokens @ $0.00002 = $12 notional = 12_000_000 micro-USDC; 150% margin = 18_000_000.
    let open = |amount: u64, collateral: u64| {
        ix_open_borrow_position(
            borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            m.usdc_mint,
            m.market_vault,
            m.vault_token_account,
            m.global_usdc_vault,
            m.global_config,
            lender_offer,
            borrower_token_account,
            borrower_usdc_account,
            borrow_position,
            amount,
            1,
            collateral,
        )
    };

    // The $10 minimum: 499,999 tokens is $9.99998, refused even with ample collateral; 500k is $10.
    assert_err(
        m.ctx.send_with_price(&borrower, CUSTOM_ORACLE_TOKEN_MINT, open(499_999_000_000, 100_000_000), &[]),
        ShortStakeError::PositionTooSmall,
    );

    let (under, exact) = (open(600_000_000_000, 17_999_999), open(600_000_000_000, 18_000_000));

    assert_err(
        m.ctx.send_with_price(&borrower, CUSTOM_ORACLE_TOKEN_MINT, under, &[]),
        ShortStakeError::InsufficientCollateral,
    );
    m.ctx
        .send_with_price(&borrower, CUSTOM_ORACLE_TOKEN_MINT, exact, &[])
        .expect("exactly 150% margin should be accepted");

    let position: short_stake001::BorrowPosition = m.ctx.fetch(borrow_position);
    assert_eq!(position.entry_price_usd, price);
    // Equity (collateral + PnL - 1h interest at 1_200 bps/day) meets 110% of notional at
    // (18 + 12) USDC / (600k tokens * (1 + 1.1 + 0.12/24)) = $0.0000237529... (scale-free).
    assert_eq!(position.initial_liquidation_price_usd, 23_752_969);
}
