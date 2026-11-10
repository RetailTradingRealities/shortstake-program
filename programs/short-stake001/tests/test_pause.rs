//! Emergency pause (protocol_state.rs): blocks new stakes and borrows only. Closing, unstaking and
//! liquidating must keep working, or a pause would trap funds and stop liquidations.
mod common;

use common::market::*;
use common::*;

const ERR_PROTOCOL_PAUSED: u32 = 6024;

fn admin_sets_paused(m: &mut Market, paused: bool) -> Result<(), String> {
    let admin = m.ctx.admin.insecure_clone();
    let admin_pk = to_anchor(admin.pubkey());
    m.ctx.svm.expire_blockhash();
    m.ctx.send(&admin, ix_set_paused(admin_pk, m.global_config, paused), &[])
}

/// A stake from a second, freshly funded staker (the market's staker staked everything).
fn deposit_second_offer(m: &mut Market) -> Result<(), String> {
    let staker = m.ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());
    let staker_token_account = m.ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, 1_000_000);
    let (offer, _) = lender_offer_pda(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, 1);
    let ix = ix_deposit_stake(
        staker_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        staker_token_account,
        m.market_vault,
        m.vault_token_account,
        offer,
        m.global_config,
        1_000_000,
        1_200,
        30,
        1,
    );
    m.ctx.send_with_price(&staker, CUSTOM_ORACLE_TOKEN_MINT, ix, &[])
}

#[test]
fn pause_blocks_new_stakes_and_borrows() {
    let mut m = new_market(1_200);
    admin_sets_paused(&mut m, true).expect("authority can pause");
    let state: short_stake001::ProtocolState = m.ctx.fetch(protocol_state_pda().0);
    assert!(state.paused);

    assert_custom_error(deposit_second_offer(&mut m), ERR_PROTOCOL_PAUSED);

    let loan = m.new_borrower(1);
    let ix = m.open_ix(&loan, 1, COLLATERAL);
    assert_custom_error(
        m.ctx.send_with_price(&loan.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[]),
        ERR_PROTOCOL_PAUSED,
    );

    // Unpausing restores both.
    admin_sets_paused(&mut m, false).expect("authority can unpause");
    deposit_second_offer(&mut m).expect("staking works again");
    m.open_loan(1, COLLATERAL);
}

#[test]
fn pause_never_traps_funds_or_stops_liquidations() {
    let mut m = new_market(1_200);
    let to_close = m.open_loan(1, COLLATERAL);
    let to_liquidate = m.open_loan(2, COLLATERAL);
    admin_sets_paused(&mut m, true).expect("authority can pause");

    m.close_loan(&to_close).expect("closing works while paused");

    // $2 -> $3: notional $150, equity 160 - 50 - interest <= 110% of 150 -> liquidatable.
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 3_000_000_000_000);
    m.liquidate(&to_liquidate, None).0.expect("liquidations work while paused");

    m.early_unstake().expect("unstaking works while paused");
}

#[test]
fn only_the_config_authority_can_pause() {
    let mut m = new_market(1_200);
    let intruder = m.ctx.new_funded_keypair(1_000_000_000);
    let intruder_pk = to_anchor(intruder.pubkey());
    assert!(
        m.ctx.send(&intruder, ix_set_paused(intruder_pk, m.global_config, true), &[]).is_err(),
        "a non-authority must not be able to pause"
    );
    let state: short_stake001::ProtocolState = m.ctx.fetch(protocol_state_pda().0);
    assert!(!state.paused);
}

/// Deployments initialized before ProtocolState existed (devnet): stakes and borrows fail until
/// the authority's first set_paused(false) creates it.
#[test]
fn first_set_paused_creates_the_state_on_older_deployments() {
    let mut m = new_market(1_200);
    let pda = to_svm(protocol_state_pda().0);
    m.ctx.svm.set_account(pda, solana_account::Account::default()).expect("remove the account");
    assert!(!m.ctx.account_exists(protocol_state_pda().0));
    assert!(deposit_second_offer(&mut m).is_err(), "no state account yet: refused");

    admin_sets_paused(&mut m, false).expect("set_paused(false) creates it");
    let state: short_stake001::ProtocolState = m.ctx.fetch(protocol_state_pda().0);
    assert!(!state.paused);
    deposit_second_offer(&mut m).expect("staking works once it exists");
}
