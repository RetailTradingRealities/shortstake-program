mod common;

use common::*;

use anchor_lang::prelude::Pubkey;
use short_stake001::get_pyth_price_in_usdc::MAXIMUM_PRICE_AGE_SECONDS;
use short_stake001::pyth_feeds_list::{OracleFlag, PYTH_ALLOWED_FEEDS};
use short_stake001::pyth_price_update::PYTH_RECEIVER_PROGRAM_ID;
use short_stake001::ShortStakeError;
use solana_instruction::Instruction;
use solana_keypair::Keypair;

const NOW: i64 = 1_700_000_000;
const BONK_DECIMALS: u8 = 5;
// $0.00002 as Pyth reports it: 2000 x 10^-8. In program units (10^-12 USD) that's 20_000_000.
const PYTH_PRICE: i64 = 2_000;
const PYTH_EXPONENT: i32 = -8;
const PRICE_UNITS: u64 = 20_000_000;

/// A Pyth-listed token from the real allowlist, with its feed ID and sponsored price account.
fn listed(symbol_mint: &str) -> (Pubkey, [u8; 32], Pubkey) {
    *PYTH_ALLOWED_FEEDS
        .iter()
        .find(|(mint, _, _)| mint.to_string() == symbol_mint)
        .expect("mint not on PYTH_ALLOWED_FEEDS")
}
fn bonk() -> (Pubkey, [u8; 32], Pubkey) {
    listed("DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263")
}
fn sol() -> (Pubkey, [u8; 32], Pubkey) {
    listed("So11111111111111111111111111111111111111112")
}

struct Market {
    ctx: TestCtx,
    mint: Pubkey,
    feed_id: [u8; 32],
    price_account: Pubkey,
    usdc_mint: Pubkey,
    global_config: Pubkey,
    global_usdc_vault: Pubkey,
    market_vault: Pubkey,
    vault_token_account: Pubkey,
    staker: Keypair,
    staker_pk: Pubkey,
    staker_token_account: Pubkey,
}

/// initialize + global vault + funded staker for BONK (Pyth-priced); nothing staked yet.
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

    let (mint, feed_id, price_account) = bonk();
    ctx.create_mint(mint, BONK_DECIMALS, admin_pk);
    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());
    let staker_token_account = ctx.create_ata(staker_pk, mint, 1_000_000_000_000);
    let (market_vault, _) = market_vault_pda(mint);
    let (vault_token_account, _) = vault_token_account_pda(mint);

    Market {
        ctx,
        mint,
        feed_id,
        price_account,
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

impl Market {
    /// A genuine-looking Pyth update for this market's feed at its sponsored account.
    fn set_price(&mut self, publish_time: i64) {
        let (at, feed_id) = (self.price_account, self.feed_id);
        self.ctx.set_pyth_price(at, PYTH_RECEIVER_PROGRAM_ID, feed_id, PYTH_PRICE, PYTH_EXPONENT, publish_time);
    }

    fn deposit_ix(&self, offer_id: u64) -> Instruction {
        let (lender_offer, _) = lender_offer_pda(self.staker_pk, self.mint, offer_id);
        ix_deposit_stake(
            self.staker_pk,
            self.mint,
            self.staker_token_account,
            self.market_vault,
            self.vault_token_account,
            lender_offer,
            self.global_config,
            200_000_000_000, // 2M BONK
            1_200,
            30,
            offer_id,
        )
    }

    /// Distinct `offer_id`s make otherwise-identical retries distinct transactions (the test
    /// validator drops an exact duplicate as AlreadyProcessed, even after a failure).
    fn deposit_with_feed(&mut self, feed: Option<Pubkey>, offer_id: u64) -> Result<(), String> {
        let ix = self.deposit_ix(offer_id);
        let ix = match feed {
            Some(feed) => with_pyth_feed(ix, feed),
            None => ix,
        };
        let staker = self.staker.insecure_clone();
        self.ctx.send(&staker, ix, &[])
    }
}

fn assert_err(result: Result<(), String>, expected: ShortStakeError) {
    assert_custom(result, u32::from(expected));
}

fn assert_custom(result: Result<(), String>, code: u32) {
    let err = result.expect_err("transaction should have been rejected");
    assert!(err.contains(&format!("Custom({code})")), "expected custom error {code}, got {err}");
}

#[test]
fn deposit_accepts_pyth_sponsored_price_account() {
    let mut m = setup_market();
    m.set_price(NOW);
    let feed = m.price_account;
    m.deposit_with_feed(Some(feed), 1).expect("deposit priced by the sponsored Pyth account should succeed");
    let vault: short_stake001::MarketVault = m.ctx.fetch(m.market_vault);
    assert!(vault.expected_oracle_feed == OracleFlag::Pyth, "a listed mint must create a Pyth market");
}

/// The old bug: the feed ID's bytes read as an address. On mainnet that is Pyth's legacy price
/// account (another owner); here it is even given a valid Receiver-owned update, and must still
/// be refused because it isn't the sponsored account.
#[test]
fn deposit_rejects_feed_id_used_as_account_address() {
    let mut m = setup_market();
    let feed_id_address = Pubkey::new_from_array(m.feed_id);
    let feed_id = m.feed_id;
    m.ctx.set_pyth_price(feed_id_address, PYTH_RECEIVER_PROGRAM_ID, feed_id, PYTH_PRICE, PYTH_EXPONENT, NOW);
    assert_err(m.deposit_with_feed(Some(feed_id_address), 1), ShortStakeError::InvalidPythFeed);
}

/// A caller-posted update (valid, right feed, Receiver-owned) at an address of their choosing.
#[test]
fn deposit_rejects_self_posted_update_account() {
    let mut m = setup_market();
    let own_account = random_pubkey();
    let feed_id = m.feed_id;
    m.ctx.set_pyth_price(own_account, PYTH_RECEIVER_PROGRAM_ID, feed_id, PYTH_PRICE, PYTH_EXPONENT, NOW);
    assert_err(m.deposit_with_feed(Some(own_account), 1), ShortStakeError::InvalidPythFeed);
}

#[test]
fn deposit_rejects_another_tokens_price_account() {
    let mut m = setup_market();
    let (_, sol_feed_id, sol_account) = sol();
    m.ctx.set_pyth_price(sol_account, PYTH_RECEIVER_PROGRAM_ID, sol_feed_id, 15_000_000_000, -8, NOW);
    assert_err(m.deposit_with_feed(Some(sol_account), 1), ShortStakeError::InvalidPythFeed);
}

#[test]
fn deposit_rejects_wrong_feed_inside_sponsored_account() {
    let mut m = setup_market();
    let (_, sol_feed_id, _) = sol();
    let at = m.price_account;
    m.ctx.set_pyth_price(at, PYTH_RECEIVER_PROGRAM_ID, sol_feed_id, PYTH_PRICE, PYTH_EXPONENT, NOW);
    assert_err(m.deposit_with_feed(Some(at), 1), ShortStakeError::InvalidPrice);
}

#[test]
fn deposit_rejects_price_account_not_owned_by_pyth_receiver() {
    let mut m = setup_market();
    let (at, feed_id) = (m.price_account, m.feed_id);
    m.ctx.set_pyth_price(at, random_pubkey(), feed_id, PYTH_PRICE, PYTH_EXPONENT, NOW);
    assert_custom(m.deposit_with_feed(Some(at), 1), 3007); // Anchor AccountOwnedByWrongProgram
}

#[test]
fn deposit_rejects_missing_pyth_account() {
    let mut m = setup_market();
    m.set_price(NOW);
    assert_err(m.deposit_with_feed(None, 1), ShortStakeError::MissingPythFeed);
}

#[test]
fn deposit_accepts_pyth_price_at_max_age_and_rejects_older() {
    let mut m = setup_market();
    m.set_price(NOW - MAXIMUM_PRICE_AGE_SECONDS as i64 - 1);
    let feed = m.price_account;
    assert_err(m.deposit_with_feed(Some(feed), 1), ShortStakeError::InvalidPrice);

    m.set_price(NOW - MAXIMUM_PRICE_AGE_SECONDS as i64);
    m.deposit_with_feed(Some(feed), 2).expect("a price exactly at the max age is still valid");
}

#[test]
fn max_age_covers_pyth_55s_heartbeat() {
    assert!(MAXIMUM_PRICE_AGE_SECONDS >= 90, "sponsored feeds update every 55s; leave room to land a tx");
}

/// A Pyth price reaches the position exactly, normalized to 10^-12 USD units.
#[test]
fn open_borrow_records_normalized_pyth_price() {
    let mut m = setup_market();
    m.set_price(NOW);
    let feed = m.price_account;
    m.deposit_with_feed(Some(feed), 1).expect("deposit_stake should succeed");

    let borrower = m.ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = m.ctx.create_ata(borrower_pk, m.mint, 0);
    let borrower_usdc_account = m.ctx.create_ata(borrower_pk, m.usdc_mint, 500_000_000);
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, m.mint, 1);
    let (borrow_position, _) = borrow_position_pda(borrower_pk, 1);
    // 1M BONK @ $0.00002 = $20 notional; 40 USDC collateral (200%).
    let open = ix_open_borrow_position(
        borrower_pk,
        m.mint,
        m.usdc_mint,
        m.market_vault,
        m.vault_token_account,
        m.global_usdc_vault,
        m.global_config,
        lender_offer,
        borrower_token_account,
        borrower_usdc_account,
        borrow_position,
        100_000_000_000,
        1,
        40_000_000,
    );
    m.ctx
        .send(&borrower, with_pyth_feed(open, feed), &[])
        .expect("open_borrow_position with the Pyth price should succeed");

    let position: short_stake001::BorrowPosition = m.ctx.fetch(borrow_position);
    assert_eq!(position.entry_price_usd, PRICE_UNITS);
}

// ---- Slot Zero I-02 / I-06 -----------------------------------------------------------------

/// A publish_time a few seconds ahead of the cluster clock is accepted (I-02); far ahead is not.
#[test]
fn deposit_accepts_small_future_skew_and_rejects_large() {
    let mut m = setup_market();
    m.set_price(NOW + 5);
    let feed = m.price_account;
    m.deposit_with_feed(Some(feed), 1).expect("5 s of future skew should be accepted");

    let mut m = setup_market();
    m.set_price(NOW + 11);
    let feed = m.price_account;
    assert_err(m.deposit_with_feed(Some(feed), 1), ShortStakeError::InvalidPrice);
}

/// A Pyth price whose confidence band is wider than 5% of the price is refused (I-06).
#[test]
fn deposit_rejects_wide_pyth_confidence() {
    let mut m = setup_market();
    let (at, feed_id) = (m.price_account, m.feed_id);
    // conf exactly 5% of the price: accepted.
    m.ctx.set_pyth_price_with_conf(at, PYTH_RECEIVER_PROGRAM_ID, feed_id, PYTH_PRICE, (PYTH_PRICE as u64) / 20, PYTH_EXPONENT, NOW);
    m.deposit_with_feed(Some(at), 1).expect("5% confidence should be accepted");

    let mut m = setup_market();
    let (at, feed_id) = (m.price_account, m.feed_id);
    m.ctx.set_pyth_price_with_conf(at, PYTH_RECEIVER_PROGRAM_ID, feed_id, PYTH_PRICE, (PYTH_PRICE as u64) / 20 + 1, PYTH_EXPONENT, NOW);
    assert_err(m.deposit_with_feed(Some(at), 1), ShortStakeError::PythConfidenceTooWide);
}
