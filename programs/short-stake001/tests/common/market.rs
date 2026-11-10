//! One staker offer on a fresh custom-oracle market, with helpers to open, close and liquidate
//! loans against it and to early_unstake from it. Shared by the write-off and early-unstake tests.

use super::*;

use anchor_lang::prelude::Pubkey;
use solana_instruction::Instruction;
use solana_keypair::Keypair;

pub const CUSTOM_ORACLE_TOKEN_MINT: Pubkey =
    anchor_lang::prelude::pubkey!("EHpXp3TSr7Zkkz8Z2wBEHt7zBEhL8x4JCMHUEL2jT3zQ");
pub const USDC_MINT: Pubkey = short_stake001::constants::usdc::ID;

pub const PRICE_2: u64 = 2_000_000_000_000; // $2.00, the opening price
pub const STAKE: u64 = 500_000_000; // 500 tokens (6 decimals)
pub const BORROW: u64 = 50_000_000; // 50 tokens = $100 notional at $2
pub const COLLATERAL: u64 = 160_000_000; // $160, 160% of the opening notional
pub const OPEN_FEE: u64 = 250_000; // 25 bps of the $100 notional

// Program error codes (6000 + ShortStakeError variant index).
pub const ERR_OFFER_CLOSED: u32 = 6018;
pub const ERR_NOT_ATA: u32 = 6019;
pub const ERR_NOTHING_TO_WITHDRAW: u32 = 6020;
pub const ERR_UNSUPPORTED_MARKET_TOKEN: u32 = 6025;
pub const ERR_OFFER_TERMS_CHANGED: u32 = 6026;
pub const ERR_INVALID_EXPIRY_BOUNDS: u32 = 6027;
pub const ERR_MISSING_STAKER_TOKEN_ACCOUNT: u32 = 6029;
/// Anchor's ConstraintTokenOwner: the token account isn't owned by the expected key.
pub const ERR_CONSTRAINT_TOKEN_OWNER: u32 = 2015;

/// One staker offer on a fresh market, plus helpers to open/close/liquidate loans against it.
pub struct Market {
    pub ctx: TestCtx,
    pub staker: Keypair,
    pub staker_pk: Pubkey,
    pub staker_token_account: Pubkey,
    pub global_config: Pubkey,
    pub global_usdc_vault: Pubkey,
    pub market_vault: Pubkey,
    pub vault_token_account: Pubkey,
    pub lender_offer: Pubkey,
}

pub struct Loan {
    pub borrower: Keypair,
    pub borrower_pk: Pubkey,
    pub borrower_token_account: Pubkey,
    pub borrower_usdc_account: Pubkey,
    pub borrow_position: Pubkey,
}

pub fn new_market(interest_rate_bps: u16) -> Market {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    ctx.create_mint(USDC_MINT, 6, admin_pk);
    let (global_usdc_vault, _) = global_usdc_vault_pda();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize_global_vault(admin_pk, global_config, USDC_MINT, global_usdc_vault),
        &[],
    )
    .expect("initialize_global_vault should succeed");

    ctx.create_mint(CUSTOM_ORACLE_TOKEN_MINT, 6, admin_pk);
    ctx.set_custom_price(CUSTOM_ORACLE_TOKEN_MINT, PRICE_2);

    let staker = ctx.new_funded_keypair(10_000_000_000);
    let staker_pk = to_anchor(staker.pubkey());
    let staker_token_account = ctx.create_ata(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, STAKE);
    let (market_vault, _) = market_vault_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (vault_token_account, _) = vault_token_account_pda(CUSTOM_ORACLE_TOKEN_MINT);
    let (lender_offer, _) = lender_offer_pda(staker_pk, CUSTOM_ORACLE_TOKEN_MINT, 1);

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
            STAKE,
            interest_rate_bps,
            30,
            1,
        ),
        &[],
    )
    .expect("deposit_stake should succeed");

    Market {
        ctx,
        staker,
        staker_pk,
        staker_token_account,
        global_config,
        global_usdc_vault,
        market_vault,
        vault_token_account,
        lender_offer,
    }
}

impl Market {
    pub fn open_ix(&self, loan: &Loan, position_id: u64, collateral: u64) -> Instruction {
        ix_open_borrow_position(
            loan.borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            USDC_MINT,
            self.market_vault,
            self.vault_token_account,
            self.global_usdc_vault,
            self.global_config,
            self.lender_offer,
            loan.borrower_token_account,
            loan.borrower_usdc_account,
            loan.borrow_position,
            BORROW,
            position_id,
            collateral,
        )
    }

    pub fn new_borrower(&mut self, position_id: u64) -> Loan {
        let borrower = self.ctx.new_funded_keypair(10_000_000_000);
        let borrower_pk = to_anchor(borrower.pubkey());
        Loan {
            borrower_token_account: self.ctx.create_ata(borrower_pk, CUSTOM_ORACLE_TOKEN_MINT, 0),
            borrower_usdc_account: self.ctx.create_ata(borrower_pk, USDC_MINT, 500_000_000),
            borrow_position: borrow_position_pda(borrower_pk, position_id).0,
            borrower,
            borrower_pk,
        }
    }

    pub fn open_loan(&mut self, position_id: u64, collateral: u64) -> Loan {
        let loan = self.new_borrower(position_id);
        let ix = self.open_ix(&loan, position_id, collateral);
        self.ctx
            .send_with_price(&loan.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[])
            .expect("open_borrow_position should succeed");
        loan
    }

    pub fn close_loan(&mut self, loan: &Loan) -> Result<(), String> {
        let staker_usdc_account = self.ctx.create_ata(self.staker_pk, USDC_MINT, 0);
        self.close_loan_with(loan, staker_usdc_account)
    }

    /// close_position paying the staker's interest into `staker_usdc_account` (created by the
    /// caller; not reset here, so a real SetAuthority on it stays in effect).
    pub fn close_loan_with(&mut self, loan: &Loan, staker_usdc_account: Pubkey) -> Result<(), String> {
        let ix = ix_close_position(
            loan.borrower_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            USDC_MINT,
            loan.borrower_token_account,
            self.market_vault,
            self.vault_token_account,
            self.global_usdc_vault,
            self.global_config,
            staker_usdc_account,
            loan.borrower_usdc_account,
            self.lender_offer,
            loan.borrow_position,
        );
        self.ctx.send_with_price(&loan.borrower.insecure_clone(), CUSTOM_ORACLE_TOKEN_MINT, ix, &[])
    }

    /// Liquidates `loan` with a fresh liquidator holding the borrowed tokens. Returns the
    /// liquidator's (token, USDC) accounts. `staker_usdc_account` defaults to the staker's ATA.
    pub fn liquidate(
        &mut self,
        loan: &Loan,
        staker_usdc_account: Option<Pubkey>,
    ) -> (Result<(), String>, Pubkey, Pubkey) {
        self.liquidate_with(loan, staker_usdc_account, None)
    }

    /// Like `liquidate`, but `borrower_usdc_account` (default: the loan's ATA) can be any account.
    pub fn liquidate_with(
        &mut self,
        loan: &Loan,
        staker_usdc_account: Option<Pubkey>,
        borrower_usdc_account: Option<Pubkey>,
    ) -> (Result<(), String>, Pubkey, Pubkey) {
        let staker_token_ata =
            spl_associated_token_account_interface::address::get_associated_token_address(&self.staker_pk, &CUSTOM_ORACLE_TOKEN_MINT);
        self.liquidate_full(loan, staker_usdc_account, Some(staker_token_ata), borrower_usdc_account)
    }

    /// Full control: `staker_token_account` None = not passed (allowed on a buyback).
    pub fn liquidate_full(
        &mut self,
        loan: &Loan,
        staker_usdc_account: Option<Pubkey>,
        staker_token_account: Option<Pubkey>,
        borrower_usdc_account: Option<Pubkey>,
    ) -> (Result<(), String>, Pubkey, Pubkey) {
        let liquidator = self.ctx.new_funded_keypair(10_000_000_000);
        let liquidator_pk = to_anchor(liquidator.pubkey());
        let liquidator_token_account = self.ctx.create_ata(liquidator_pk, CUSTOM_ORACLE_TOKEN_MINT, BORROW);
        let liquidator_usdc_account = self.ctx.create_ata(liquidator_pk, USDC_MINT, 0);
        let staker_usdc_account =
            staker_usdc_account.unwrap_or_else(|| self.ctx.create_ata(self.staker_pk, USDC_MINT, 0));
        let ix = ix_liquidate_position_with_staker_token(
            liquidator_pk,
            loan.borrower_pk,
            self.staker_pk,
            CUSTOM_ORACLE_TOKEN_MINT,
            USDC_MINT,
            liquidator_token_account,
            self.market_vault,
            self.vault_token_account,
            self.global_usdc_vault,
            self.global_config,
            staker_usdc_account,
            staker_token_account,
            liquidator_usdc_account,
            fee_collector_usdc_ata(USDC_MINT),
            borrower_usdc_account.unwrap_or(loan.borrower_usdc_account),
            self.lender_offer,
            loan.borrow_position,
        );
        let result = self.ctx.send_with_price(&liquidator, CUSTOM_ORACLE_TOKEN_MINT, ix, &[]);
        (result, liquidator_token_account, liquidator_usdc_account)
    }

    /// A REAL SPL Token SetAuthority(AccountOwner) signed by `owner`: hands `account` to
    /// `new_owner`. The ATA address stays taken, so nobody can recreate a correct ATA there.
    pub fn reassign_token_account(&mut self, owner: &Keypair, account: Pubkey, new_owner: Pubkey) -> Result<(), String> {
        let ix = spl_token_interface::instruction::set_authority(
            &to_svm(TOKEN_PROGRAM_ID),
            &to_svm(account),
            Some(&to_svm(new_owner)),
            spl_token_interface::instruction::AuthorityType::AccountOwner,
            &owner.pubkey(),
            &[],
        )
        .expect("set_authority instruction");
        self.ctx.svm.expire_blockhash();
        self.ctx.send(owner, ix, &[])
    }

    pub fn early_unstake(&mut self) -> Result<(), String> {
        let staker = self.staker.insecure_clone();
        let ix = ix_early_unstake(
            self.staker_pk,
            self.staker_pk, // payer_wallet = market_vault.payer (the first staker)
            CUSTOM_ORACLE_TOKEN_MINT,
            self.staker_token_account,
            self.market_vault,
            self.vault_token_account,
            self.lender_offer,
        );
        // A repeat early_unstake is byte-identical: a fresh blockhash keeps it from being
        // rejected as AlreadyProcessed.
        self.ctx.svm.expire_blockhash();
        self.ctx.send(&staker, ix, &[])
    }

    pub fn offer(&self) -> short_stake001::LenderOffer {
        self.ctx.fetch(self.lender_offer)
    }

    pub fn staker_usdc(&self) -> u64 {
        self.ctx.token_balance(usdc_ata(self.staker_pk))
    }
}

pub fn usdc_ata(owner: Pubkey) -> Pubkey {
    spl_associated_token_account_interface::address::get_associated_token_address(&owner, &USDC_MINT)
}

pub fn assert_custom_error(result: Result<(), String>, code: u32) {
    let err = result.expect_err("transaction should fail");
    assert!(err.contains(&format!("Custom({code})")), "expected Custom({code}), got: {err}");
}
