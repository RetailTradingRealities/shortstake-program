mod common;

use common::market::*;
use common::*;

/// deposit_stake -> early_unstake (no borrows against the offer): funds return to the
/// staker and, since the pool is now fully empty, the vault/offer PDAs get closed.
#[test]
fn early_unstake_returns_funds_and_closes_pool() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, 6, admin_pk);

    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 2_000_000_000_000);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());

    let stake_amount: u64 = 250_000_000;
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, stake_amount);

    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let offer_id: u64 = 7;
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

    ctx.send(
        &staker,
        ix_early_unstake(
            staker_pk,
            staker_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            staker_token_account,
            market_vault,
            vault_token_account,
            lender_offer,
        ),
        &[],
    )
    .expect("early_unstake should succeed");

    assert_eq!(ctx.token_balance(staker_token_account), stake_amount);
    assert_eq!(ctx.lamports(vault_token_account), 0);
    assert_eq!(ctx.lamports(market_vault), 0);
    assert_eq!(ctx.lamports(lender_offer), 0);
}

/// With a loan open, early_unstake returns only the unborrowed tokens. The offer stays (the loan
/// still settles against it) but is closed to new borrowing; once the loan closes, a second
/// early_unstake returns the rest and closes the offer and, with the market now empty, the vault.
#[test]
fn early_unstake_with_open_loan_returns_unborrowed_and_closes_offer_to_borrowing() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);

    m.early_unstake().expect("early_unstake should return the unborrowed tokens");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - BORROW);
    let offer = m.offer();
    assert!(offer.is_closed, "closed to new borrowing");
    assert_eq!(offer.available_to_borrow, 0);
    assert_eq!(offer.total_offered, BORROW, "only the lent-out tokens remain");
    assert_eq!(offer.current_active_borrows, 1);
    let vault: short_stake001::MarketVault = m.ctx.fetch(m.market_vault);
    assert_eq!(vault.total_staked_amount, BORROW);
    assert_eq!(m.ctx.token_balance(m.vault_token_account), 0, "the lent tokens are with the borrower");

    // Nobody can borrow from it any more, and there is nothing left to withdraw yet.
    let newcomer = m.new_borrower(2);
    let ix = m.open_ix(&newcomer, 2, COLLATERAL);
    let result = m.ctx.send_with_price(&newcomer.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[]);
    assert_custom_error(result, ERR_OFFER_CLOSED);
    assert_custom_error(m.early_unstake(), ERR_NOTHING_TO_WITHDRAW);

    // The loan still settles normally and pays the staker interest.
    m.close_loan(&loan).expect("the open loan can still close");
    assert!(m.staker_usdc() > 0, "staker paid interest");
    let offer = m.offer();
    assert_eq!(offer.available_to_borrow, BORROW);
    assert_eq!(offer.current_active_borrows, 0);

    m.early_unstake().expect("early_unstake should return the rest");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE);
    assert!(!m.ctx.account_exists(m.lender_offer), "offer account closed");
    assert_eq!(m.ctx.lamports(m.market_vault), 0, "empty market closed");
    assert_eq!(m.ctx.lamports(m.vault_token_account), 0);
}

/// After a partial early_unstake, the remaining loan can still be liquidated (buyback); the
/// bought-back tokens return to the offer and the staker collects them.
#[test]
fn early_unstake_then_liquidation_returns_tokens_to_staker() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.early_unstake().expect("partial early_unstake");

    // $3.07: undercollateralised but the collateral still covers the buyback + bounty.
    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 3_070_000_000_000);
    m.liquidate(&loan, None).0.expect("liquidation of the remaining loan should succeed");
    let offer = m.offer();
    assert!(offer.is_closed, "stays closed after a buyback");
    assert_eq!(offer.available_to_borrow, BORROW);

    m.early_unstake().expect("early_unstake should return the bought-back tokens");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE);
    assert!(!m.ctx.account_exists(m.lender_offer));
}

/// After a partial early_unstake, a write-off of the last loan closes the offer outright; the
/// staker keeps the unborrowed tokens and gets the collateral for the written-off ones.
#[test]
fn early_unstake_then_write_off_closes_offer() {
    let mut m = new_market(1_200);
    let loan = m.open_loan(1, COLLATERAL);
    m.early_unstake().expect("partial early_unstake");

    m.ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, 4_000_000_000_000);
    m.liquidate(&loan, None).0.expect("write-off should succeed");
    assert!(!m.ctx.account_exists(m.lender_offer), "no loans left: offer closed by the write-off");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - BORROW);
    assert_eq!(m.staker_usdc(), COLLATERAL);
    let vault: short_stake001::MarketVault = m.ctx.fetch(m.market_vault);
    assert_eq!(vault.total_staked_amount, 0);
    assert_eq!(vault.total_borrowed_amount, 0);
}

/// Two loans: unstake, one closes, unstake collects it while the other is still out.
#[test]
fn early_unstake_collects_each_loan_as_it_settles() {
    let mut m = new_market(1_200);
    let first = m.open_loan(1, COLLATERAL);
    let _second = m.open_loan(2, COLLATERAL);
    m.early_unstake().expect("partial early_unstake");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - 2 * BORROW);

    m.close_loan(&first).expect("first loan closes");
    m.early_unstake().expect("collect the first loan's tokens");
    assert_eq!(m.ctx.token_balance(m.staker_token_account), STAKE - BORROW);
    let offer = m.offer();
    assert!(offer.is_closed);
    assert_eq!(offer.total_offered, BORROW);
    assert_eq!(offer.current_active_borrows, 1, "the account stays for the second loan");
}

/// A full unstake closes market_vault. Re-funding its address later in the SAME transaction (the
/// classic revival attempt) must not bring back the old account: Anchor's close hands it to the
/// System Program with no data, unlike draining only the lamports.
#[test]
fn closed_market_vault_cannot_be_revived_in_the_same_transaction() {
    let mut m = new_market(1_200);
    let staker = m.staker.insecure_clone();
    let unstake = ix_early_unstake(
        m.staker_pk,
        m.staker_pk,
        CUSTOM_ORACLE_TOKEN_MINT,
        m.staker_token_account,
        m.market_vault,
        m.vault_token_account,
        m.lender_offer,
    );
    // System Program Transfer: u32 instruction index 2, then u64 lamports (little-endian).
    let mut data = 2u32.to_le_bytes().to_vec();
    data.extend_from_slice(&10_000_000u64.to_le_bytes());
    let refund_the_vault = solana_instruction::Instruction {
        program_id: solana_pubkey::Pubkey::default(),
        accounts: vec![
            solana_instruction::AccountMeta::new(staker.pubkey(), true),
            solana_instruction::AccountMeta::new(to_svm(m.market_vault), false),
        ],
        data,
    };
    m.ctx
        .send_ixs(&staker, vec![unstake, refund_the_vault], &[])
        .expect("unstake + transfer should succeed");

    let account = m.ctx.svm.get_account(&to_svm(m.market_vault)).expect("re-funded address exists");
    assert_eq!(account.lamports, 10_000_000);
    assert_eq!(account.owner, solana_pubkey::Pubkey::default(), "owned by the System Program, not ours");
    assert!(account.data.is_empty(), "the old market state is gone");
}
