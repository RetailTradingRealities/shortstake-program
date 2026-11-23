use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Mint, Transfer};
use anchor_spl::associated_token::AssociatedToken; // Added missing import
use crate::pyth_feeds_list::OracleFlag;
use crate::pyth_price_update::PriceUpdateV2;
use crate::ShortStakeError;
use crate::GlobalConfig;
use crate::instructions::{ProtocolState, PROTOCOL_STATE_SEED};
use crate::signed_price::{PriceAction, INSTRUCTIONS_SYSVAR_ID};
use crate::is_pyth_supported;
use crate::get_current_price;


pub fn deposit_stake(
    ctx: Context<DepositStake>,
    amount_to_stake: u64,
    interest_rate_bps: u16,
    max_expiry_days: u8,
    offer_id: u64,
) -> Result<()> {
    require!(amount_to_stake > 0, ShortStakeError::InvalidStakeAmount);

    // Market tokens: not USDC itself (its staker accounts would collide with the USDC payout
    // accounts, Slot Zero H-01), no more decimals than the borrowing math handles (I-09), and no
    // freeze authority: its holder could freeze the market's vault and lock every staker, close
    // and liquidation in it (I-07).
    require!(
        ctx.accounts.token_mint.key() != crate::constants::usdc::ID
            && ctx.accounts.token_mint.decimals <= crate::constants::MAX_MARKET_TOKEN_DECIMALS
            && ctx.accounts.token_mint.freeze_authority.is_none(),
        ShortStakeError::UnsupportedMarketToken
    );

    let global_config = &ctx.accounts.global_config;
    require!(
        max_expiry_days >= global_config.min_expiry_days
            && max_expiry_days <= global_config.max_expiry_days,
        ShortStakeError::InvalidExpiryDays
    );

    // Every market now uses the signed oracle (2026-11-21, see get_current_price.rs). A market
    // created as Pyth (e.g. WIF) is relabelled Custom on its next stake, below, so the stored
    // flag and the StakeEvent the indexer reads match how the market is actually priced.
    let is_new_market = ctx.accounts.market_vault.token_mint == Pubkey::default();
    let oracle_flag = is_pyth_supported(ctx.accounts.token_mint.key());

    // Reject the deposit up front if no valid price source is available for this mint -
    // avoids sinking rent into a market that can never be borrowed against or liquidated.
    get_current_price(
        ctx.accounts.token_mint.key(),
        &oracle_flag,
        &ctx.accounts.pyth_price_feed,
        ctx.accounts.instructions_sysvar.as_ref(),
        ctx.accounts.global_config.oracle_signer,
        PriceAction::DepositStake,
    )?;

    // 1. Transfer tokens from staker's wallet to the global Market Vault token account
    let cpi_accounts = Transfer {
        from: ctx.accounts.staker_token_account.to_account_info(),
        to: ctx.accounts.vault_token_account.to_account_info(),
        authority: ctx.accounts.staker.to_account_info(),
    };
    //let cpi_program = ctx.accounts.token_program.to_account_info();
    //let cpi_ctx = CpiContext::new(cpi_program, cpi_accounts);
    let cpi_ctx = CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts);
    token::transfer(cpi_ctx, amount_to_stake)?;

    // 2. Initialize the Parent LenderOffer PDA state
    let offer = &mut ctx.accounts.lender_offer;
    offer.staker_pubkey = ctx.accounts.staker.key();
    offer.token_mint = ctx.accounts.token_mint.key();
    offer.total_offered = amount_to_stake;
    offer.available_to_borrow = amount_to_stake;
    offer.offer_id = offer_id;
    offer.interest_rate_bps = interest_rate_bps;
    offer.current_active_borrows = 0;
    offer.max_expiry_days = max_expiry_days;
    offer.bump = ctx.bumps.lender_offer;
    offer.is_closed = false;

    // 3. Update global market tracking state safely
    let market_vault = &mut ctx.accounts.market_vault;
    
    // If this is a brand new market_vault, populate its permanent baseline fields
    if is_new_market {
        market_vault.token_mint = ctx.accounts.token_mint.key();
        market_vault.vault_token_account = ctx.accounts.vault_token_account.key();
        market_vault.bump = ctx.bumps.market_vault;
        // Track whoever paid to create this market's infra, so it can be refunded on full drain.
        market_vault.payer = ctx.accounts.staker.key();
    }

    market_vault.expected_oracle_feed = oracle_flag;

    market_vault.total_staked_amount = market_vault
        .total_staked_amount
        .checked_add(amount_to_stake)
        .ok_or(ShortStakeError::MathOverflow)?;

    // Emit post-state so the indexer can overwrite columns instead of applying deltas:
    // a missed delta drifts forever, a missed absolute heals on the next event.
    emit!(StakeEvent {
        staker: ctx.accounts.staker.key(),
        token_mint: ctx.accounts.token_mint.key(),
        amount: amount_to_stake,
        interest_rate_bps,
        offer_id,
        lender_offer: ctx.accounts.lender_offer.key(),
        max_expiry_days,
        available_to_borrow: ctx.accounts.lender_offer.available_to_borrow,
        market_total_staked: market_vault.total_staked_amount,
        market_payer: market_vault.payer,
        oracle_flag: market_vault.expected_oracle_feed.clone(),
        token_decimals: ctx.accounts.token_mint.decimals,
        timestamp: Clock::get()?.unix_timestamp,
    });

    Ok(())
}


#[derive(Accounts)]
#[instruction(amount_to_stake: u64, interest_rate_bps: u16, max_expiry_days: u8, offer_id: u64)]
pub struct DepositStake<'info> {
    #[account(mut)]
    pub staker: Signer<'info>,

    pub token_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = token_mint,
        associated_token::authority = staker,
    )]
    pub staker_token_account: Account<'info, TokenAccount>,

    // Permissionless: the first staker into a new mint pays to create the shared market
    // infra; every staker after that hits this branch as a no-op (PDAs already exist).
    #[account(
        init_if_needed,
        payer = staker,
        space = 8 + MarketVault::LEN,
        seeds = [b"market_vault", token_mint.key().as_ref()],
        bump
    )]
    pub market_vault: Account<'info, MarketVault>,

    #[account(
        init_if_needed,
        payer = staker,
        seeds = [b"vault_token_account", token_mint.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = market_vault,
    )]
    pub vault_token_account: Account<'info, TokenAccount>,

    #[account(
        init,
        payer = staker,
        space = 8 + LenderOffer::LEN,
        seeds = [
            b"offer",
            staker.key().as_ref(),
            token_mint.key().as_ref(),
            offer_id.to_le_bytes().as_ref()
        ],
        bump
    )]
    pub lender_offer: Account<'info, LenderOffer>,

    #[account(
        seeds = [b"global_config"],
        bump = global_config.bump
    )]
    pub global_config: Account<'info, GlobalConfig>,

    pub pyth_price_feed: Option<Box<Account<'info, PriceUpdateV2>>>,

    /// CHECK: address-constrained to the instructions sysvar; only read through
    /// solana_instructions_sysvar helpers to find the signed custom-oracle price.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>, // Added missing validation program

    // Emergency pause: no new exposure while paused (protocol_state.rs).
    #[account(
        seeds = [PROTOCOL_STATE_SEED],
        bump = protocol_state.bump,
        constraint = !protocol_state.paused @ ShortStakeError::ProtocolPaused,
    )]
    pub protocol_state: Account<'info, ProtocolState>,
}

//OracleFlag imported at the top of this file from pyth_feeds_list.rs


#[account]
pub struct MarketVault {
    pub token_mint: Pubkey,
    pub vault_token_account: Pubkey,
    pub expected_oracle_feed: OracleFlag,
    pub total_staked_amount: u64,
    pub total_borrowed_amount: u64,
    pub bump: u8,
    pub payer: Pubkey, // whoever last (re)created this market's infra; refund target on full drain
}

impl MarketVault {
    // 32 (token_mint) + 32 (vault_token_account) + 1 (OracleFlag) + 8 (total_staked) + 8 (total_borrowed) + 1 (bump) + 32 (payer) = 114 bytes
    pub const LEN: usize = 32 + 32 + 1 + 8 + 8 + 1 + 32;
}

#[account]
pub struct LenderOffer {
    pub staker_pubkey: Pubkey,
    pub token_mint: Pubkey,
    pub total_offered: u64,
    pub available_to_borrow: u64,
    pub offer_id: u64,
    pub interest_rate_bps: u16,
    pub current_active_borrows: u16,
    pub max_expiry_days: u8,
    pub bump: u8,
    // Set when one of this offer's loans is written off as bad debt, or the staker early_unstakes
    // while loans are open: no new borrows, and the staker withdraws whatever comes back via
    // early_unstake. Offers are `init`-only, so it's final.
    pub is_closed: bool,
}

impl LenderOffer {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 2 + 2 + 1 + 1 + 1;
}

#[event]
pub struct StakeEvent {
    pub staker: Pubkey,
    pub token_mint: Pubkey,
    pub amount: u64,
    pub interest_rate_bps: u16,
    pub offer_id: u64,
    /// LenderOffer PDA - primary key for the indexer's lender_offers row.
    pub lender_offer: Pubkey,
    pub max_expiry_days: u8,
    /// Post-state (equals `amount` on creation; explicit so the indexer never derives).
    pub available_to_borrow: u64,
    /// Post-state market_vault.total_staked_amount.
    pub market_total_staked: u64,
    /// market_vault.payer (rent-refund wallet, set on first stake).
    pub market_payer: Pubkey,
    /// market_vault.expected_oracle_feed - always Custom since 2026-11-21 (older markets may
    /// still say Pyth until their next stake; the program ignores it), so the indexer never guesses.
    pub oracle_flag: OracleFlag,
    pub token_decimals: u8,
    pub timestamp: i64,
}


