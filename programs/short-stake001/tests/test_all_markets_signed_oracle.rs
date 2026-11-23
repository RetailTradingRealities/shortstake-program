//! Every market is priced by the signed oracle since 2026-11-21 (Pyth stopped pushing its
//! sponsored meme-coin feeds around 2026-11-17, leaving Pyth markets such as WIF unable to close
//! or be liquidated). The Pyth-path tests this replaces are preserved at git tag v1-pyth.

mod common;

use common::*;

use anchor_lang::prelude::Pubkey;
use short_stake001::pyth_feeds_list::{OracleFlag, PYTH_ALLOWED_FEEDS};
use short_stake001::pyth_price_update::PYTH_RECEIVER_PROGRAM_ID;
use short_stake001::signed_price::PriceAction;
use solana_instruction::Instruction;
use solana_keypair::Keypair;

const NOW: i64 = 1_700_000_000;
const WIF_DECIMALS: u8 = 6;
/// $2 in program units (10^-12 USD per whole token).
const PRICE_UNITS: u64 = 2_000_000_000_000;
/// Byte offset of market_vault.expected_oracle_feed: 8 (discriminator) + 32 + 32.
const ORACLE_FLAG_OFFSET: usize = 72;

/// WIF from the old Pyth list, with its feed ID and sponsored price account.
fn wif() -> (Pubkey, [u8; 32], Pubkey) {
    *PYTH_ALLOWED_FEEDS
        .iter()
        .find(|(mint, _, _)| mint.to_string() == "EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm")
        .expect("WIF on the old Pyth list")
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

    let (mint, feed_id, price_account) = wif();
    ctx.create_mint(mint, WIF_DECIMALS, admin_pk);
    ctx.set_custom_price(mint, PRICE_UNITS);
    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());
    let staker_token_account = ctx.create_ata(staker_pk, mint, 1_000_000_000);
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
            100_000_000, // 100 WIF
            1_200,
            30,
            offer_id,
        )
    }

    fn oracle_flag(&self) -> OracleFlag {
        let vault: short_stake001::MarketVault = self.ctx.fetch(self.market_vault);
        vault.expected_oracle_feed
    }

    /// Rewrites the stored flag to Pyth, as on a market created before the upgrade (WIF).
    fn make_legacy_pyth_market(&mut self) {
        let address = to_svm(self.market_vault);
        let mut account = self.ctx.svm.get_account(&address).expect("market_vault exists");
        account.data[ORACLE_FLAG_OFFSET] = 0; // OracleFlag::Pyth
        self.ctx.svm.set_account(address, account).unwrap();
        assert!(self.oracle_flag() == OracleFlag::Pyth);
    }

    /// The frozen Pyth account a pre-upgrade market points at: genuine, but days old.
    fn set_frozen_pyth_price(&mut self) {
        let (at, feed_id) = (self.price_account, self.feed_id);
        self.ctx.set_pyth_price(at, PYTH_RECEIVER_PROGRAM_ID, feed_id, 200_000_000, -8, NOW - 346_000);
    }
}

/// A token on the old Pyth list now gets a signed-oracle market.
#[test]
fn pyth_listed_mint_creates_custom_market() {
    let mut m = setup_market();
    let (staker, mint, ix) = (m.staker.insecure_clone(), m.mint, m.deposit_ix(1));
    m.ctx.send_with_price(&staker, mint, ix, &[]).expect("deposit with a signed price should succeed");
    assert!(m.oracle_flag() == OracleFlag::Custom);
}

/// A Pyth account, even a fresh one, no longer prices anything: a signed price is required.
#[test]
fn pyth_account_alone_is_refused() {
    let mut m = setup_market();
    let (at, feed_id) = (m.price_account, m.feed_id);
    m.ctx.set_pyth_price(at, PYTH_RECEIVER_PROGRAM_ID, feed_id, 200_000_000, -8, NOW);
    let staker = m.staker.insecure_clone();
    let ix = with_pyth_feed(m.deposit_ix(1), m.price_account);
    assert!(m.ctx.send(&staker, ix, &[]).is_err(), "a deposit without a signed price must fail");
}

/// The WIF case: a market created as Pyth, whose Pyth feed is frozen, can still be borrowed
/// from and closed with signed prices - also with the stale Pyth account passed alongside, as
/// the site does during the transition - and its next stake relabels it Custom.
#[test]
fn legacy_pyth_market_works_on_signed_prices_and_is_relabelled() {
    let mut m = setup_market();
    let (staker, mint) = (m.staker.insecure_clone(), m.mint);
    let ix = m.deposit_ix(1);
    m.ctx.send_with_price(&staker, mint, ix, &[]).expect("deposit_stake should succeed");
    m.make_legacy_pyth_market();
    m.set_frozen_pyth_price();

    let borrower = m.ctx.new_funded_keypair(10_000_000_000);
    let borrower_pk = to_anchor(borrower.pubkey());
    let borrower_token_account = m.ctx.create_ata(borrower_pk, m.mint, 0);
    let borrower_usdc_account = m.ctx.create_ata(borrower_pk, m.usdc_mint, 500_000_000);
    let staker_usdc_account = m.ctx.create_ata(m.staker_pk, m.usdc_mint, 0);
    let (lender_offer, _) = lender_offer_pda(m.staker_pk, m.mint, 1);
    let (borrow_position, _) = borrow_position_pda(borrower_pk, 1);
    // 10 WIF @ $2 = $20 notional; 40 USDC collateral (200%).
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
        10_000_000,
        1,
        40_000_000,
    );
    let open = with_pyth_feed(open, m.price_account);
    m.ctx
        .send_ixs(&borrower, vec![m.ctx.signed_price_ix(mint, PriceAction::OpenBorrowPosition), open], &[])
        .expect("a Pyth-flagged market must open on a signed price, ignoring the frozen Pyth account");
    let position: short_stake001::BorrowPosition = m.ctx.fetch(borrow_position);
    assert_eq!(position.entry_price_usd, PRICE_UNITS);

    let close = ix_close_position(
        borrower_pk,
        m.mint,
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
    m.ctx
        .send_with_price(&borrower, mint, close, &[])
        .expect("a Pyth-flagged market must close on a signed price");
    assert_eq!(m.ctx.token_balance(borrower_token_account), 0, "the borrowed WIF went back to the vault");

    assert!(m.oracle_flag() == OracleFlag::Pyth, "only a stake relabels the market");
    let ix = with_pyth_feed(m.deposit_ix(2), m.price_account);
    m.ctx.send_with_price(&staker, mint, ix, &[]).expect("a second stake should succeed");
    assert!(m.oracle_flag() == OracleFlag::Custom, "the next stake relabels the market Custom");
}
