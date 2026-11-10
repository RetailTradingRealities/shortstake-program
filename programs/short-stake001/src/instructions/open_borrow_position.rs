use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};
use crate::constants::usdc;
use crate::pyth_price_update::PriceUpdateV2;
use crate::{get_current_price, ShortStakeError};
use crate::constants::{FEE_COLLECTOR_PUBKEY, MIN_POSITION_NOTIONAL_USDC, PRICE_TO_USDC_DIVISOR};
use crate::math::{bps_of_ceil, token_divisor};
use crate::signed_price::{PriceAction, INSTRUCTIONS_SYSVAR_ID};
use crate::instructions::{
    GlobalConfig,
    ProtocolState,
    PROTOCOL_STATE_SEED,
    LenderOffer,
    MarketVault,
};

// Make sure to import your Pyth SDK / PriceUpdateV2 struct here



pub fn open_borrow_position(
    ctx: Context<OpenBorrowPosition>,
    borrow_amount: u64,
    position_id: u64,
    provided_collateral_usdc: u64,
    max_interest_rate_bps: u16,
    min_expiry_days: u8,
) -> Result<()> {
    // The terms the borrower saw, like a swap's minimum output: an offer can be closed and
    // re-created at the same address with worse terms before this lands (Slot Zero M-02).
    require!(
        ctx.accounts.lender_offer.interest_rate_bps <= max_interest_rate_bps
            && ctx.accounts.lender_offer.max_expiry_days >= min_expiry_days,
        ShortStakeError::OfferTermsChanged
    );

    let vault = &ctx.accounts.market_vault;
    
    // 1. Fetch normalized price in 10^-12 USD per whole token ($1.00 = 1_000_000_000_000), routed by the vault's stored oracle mode
    let entry_price_usdc = get_current_price(
        vault.token_mint,
        &vault.expected_oracle_feed,
        &ctx.accounts.pyth_price_feed,
        ctx.accounts.instructions_sysvar.as_ref(),
        ctx.accounts.global_config.oracle_signer,
        PriceAction::OpenBorrowPosition,
    )?;

    msg!("The current asset price is: {}", entry_price_usdc);

    // 2. Calculate the token divisor dynamically from the SPL mint
    let token_decimals = ctx.accounts.token_mint.decimals;
    let token_divisor = token_divisor(token_decimals)?;

    // 3. Calculate Notional Debt Value in USDC
    // Widened to u128: borrow_amount * entry_price_usdc can exceed u64::MAX for
    // large positions (same overflow class as the liquidation price calc below).
    let notional_value_usdc: u64 = (borrow_amount as u128)
        .checked_mul(entry_price_usdc as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(token_divisor as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(PRICE_TO_USDC_DIVISOR)
        .ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    require!(notional_value_usdc >= MIN_POSITION_NOTIONAL_USDC, ShortStakeError::PositionTooSmall);

    // 4. Calculate required Initial Margin in USDC (150% collateralization), rounded up
    let required_margin_usdc = bps_of_ceil(notional_value_usdc, 15_000)?;

    require!(
        provided_collateral_usdc >= required_margin_usdc,
        ShortStakeError::InsufficientCollateral
    );

    require!(borrow_amount > 0, ShortStakeError::InvalidBorrowAmount);
    require!(!ctx.accounts.lender_offer.is_closed, ShortStakeError::OfferClosed);

    // Protocol borrow fee: fee_rate_borrowers_bps of the USD notional borrowed (25 bps = 0.25%),
    // rounded up. Paid from the borrower's wallet on top of the collateral, so it doesn't eat
    // into the margin.
    let borrower_fee_usdc = bps_of_ceil(notional_value_usdc, ctx.accounts.global_config.fee_rate_borrowers_bps)?;

    // 5. Calculate Initial Liquidation Price
    // The price P at which liquidate_position's trigger (equity <= maintenance margin) is met:
    //   collateral + amount*(entry - P) - interest = amount*P*thresh
    // with interest = amount*P*rate_bps*hours/10_000/24, billed for the minimum 1 hour at open.
    // Solving for P (scaled by 24 * 10_000 to stay in integers):
    //   P = (collateral + notional) * 240_000 / (amount * (24 * (10_000 + thresh_bps) + rate_bps))
    // A snapshot at open: as interest accrues the real liquidation price drifts slightly lower.
    let liq_threshold_bps = ctx.accounts.global_config.liquidation_thresh_bps as u128;
    let interest_rate_bps = ctx.accounts.lender_offer.interest_rate_bps as u128;
    let liq_price_denominator = (borrow_amount as u128)
        .checked_mul(
            24u128
                .checked_mul(10_000u128 + liq_threshold_bps)
                .ok_or(ShortStakeError::MathOverflow)?
                .checked_add(interest_rate_bps)
                .ok_or(ShortStakeError::MathOverflow)?,
        )
        .ok_or(ShortStakeError::MathOverflow)?;
    // Widened to u128: (collateral + notional) * token_divisor can exceed u64::MAX
    // for realistic position sizes on high-decimal mints (e.g. > ~$7k notional on SOL).
    // Group multiplications before divisions so precision isn't lost before the final division.
    let initial_liquidation_price_usdc: u64 = (provided_collateral_usdc as u128)
        .checked_add(notional_value_usdc as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_mul(token_divisor as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_mul(240_000u128)
        .ok_or(ShortStakeError::MathOverflow)?
        // micro-USDC -> price units, so this is comparable with entry_price_usd
        .checked_mul(PRICE_TO_USDC_DIVISOR)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(liq_price_denominator)
        .ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    // 6. Transfer USDC Collateral IN (Borrower -> Vault USDC Account)
    let cpi_accounts_usdc = Transfer {
        from: ctx.accounts.borrower_usdc_account.to_account_info(),
        to: ctx.accounts.global_usdc_vault.to_account_info(),
        authority: ctx.accounts.borrower.to_account_info(),
    };
    token::transfer(
        CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts_usdc),
        provided_collateral_usdc,
    )?;

    // 6b. Transfer Borrow Fee (Borrower -> Fee Collector)
    // Atomic with the rest of this instruction: if anything below fails (e.g. not enough
    // liquidity in the offer), the whole transaction reverts and the fee is never taken.
    if borrower_fee_usdc > 0 {
        let cpi_accounts_fee = Transfer {
            from: ctx.accounts.borrower_usdc_account.to_account_info(),
            to: ctx.accounts.fee_collector_usdc_account.to_account_info(),
            authority: ctx.accounts.borrower.to_account_info(),
        };
        token::transfer(
            CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts_fee),
            borrower_fee_usdc,
        )?;
    }

    // 7. Transfer Borrowed Tokens OUT (Vault Token Account -> Borrower Token Account)
    // FIXED: Using runtime bump calculation via ctx.bumps for maximum stability
    let token_mint_key = ctx.accounts.token_mint.key();
    let vault_seeds = &[
        b"market_vault".as_ref(),
        token_mint_key.as_ref(), 
        &[vault.bump],
    ];
    let signer = &[&vault_seeds[..]];
    
    let cpi_accounts_token = Transfer {
        from: ctx.accounts.vault_token_account.to_account_info(),
        to: ctx.accounts.borrower_token_account.to_account_info(),
        authority: ctx.accounts.market_vault.to_account_info(), // Point authority to market_vault PDA
    };
    token::transfer(
        CpiContext::new_with_signer(ctx.accounts.token_program.key(), cpi_accounts_token, signer),
        borrow_amount,
    )?;

    // 8. Update PDA State
    let current_time = Clock::get()?.unix_timestamp;
    let position = &mut ctx.accounts.borrow_position;
    position.borrower_pubkey = ctx.accounts.borrower.key();
    position.lender_offer_ref = ctx.accounts.lender_offer.key();
    position.amount_borrowed = borrow_amount;
    position.collateral_deposited = provided_collateral_usdc; 
    position.entry_price_usd = entry_price_usdc;
    position.initial_liquidation_price_usd = initial_liquidation_price_usdc;
    position.loan_start_timestamp = current_time;
    
    // FIXED: Assigned the missing expiry variable using the offer's settings
    let seconds_in_day: i64 = 86_400;
    let expiry_offset = (ctx.accounts.lender_offer.max_expiry_days as i64)
        .checked_mul(seconds_in_day)
        .ok_or(ShortStakeError::MathOverflow)?;

    position.expiry_timestamp = current_time
        .checked_add(expiry_offset)
        .ok_or(ShortStakeError::MathOverflow)?;
        
    position.position_id = position_id;
    position.bump = ctx.bumps.borrow_position;

    let market_vault = &mut ctx.accounts.market_vault;
    market_vault.total_borrowed_amount = market_vault
        .total_borrowed_amount
        .checked_add(borrow_amount)
        .ok_or(ShortStakeError::MathOverflow)?;
    
    
    /* This only changes if unstaked
    market_vault.total_staked_amount = market_vault
        .total_staked_amount
        .checked_sub(borrow_amount)
        .ok_or(ShortStakeError::MathOverflow)?;

    */

    // Deduct from Lender Offer
    let offer = &mut ctx.accounts.lender_offer;
    offer.available_to_borrow = offer
        .available_to_borrow
        .checked_sub(borrow_amount)
        .ok_or(ShortStakeError::InsufficientAvailableLiquidity)?;
    offer.current_active_borrows = offer
        .current_active_borrows
        .checked_add(1)
        .ok_or(ShortStakeError::MathOverflow)?;

    // Emit post-state so the indexer can overwrite columns instead of applying deltas.
    emit!(OpenBorrowPositionEvent {
        borrower: ctx.accounts.borrower.key(),
        lender_offer: ctx.accounts.lender_offer.key(),
        token_mint: token_mint_key,
        position_id,
        amount_borrowed: borrow_amount,
        collateral_deposited: provided_collateral_usdc,
        entry_price_usd: entry_price_usdc,
        initial_liquidation_price_usd: initial_liquidation_price_usdc,
        expiry_timestamp: ctx.accounts.borrow_position.expiry_timestamp,
        borrow_position: ctx.accounts.borrow_position.key(),
        loan_start_timestamp: ctx.accounts.borrow_position.loan_start_timestamp,
        offer_available_to_borrow: ctx.accounts.lender_offer.available_to_borrow,
        offer_active_borrows: ctx.accounts.lender_offer.current_active_borrows,
        market_total_borrowed: ctx.accounts.market_vault.total_borrowed_amount,
        borrower_fee: borrower_fee_usdc,
    });

    Ok(())
}


#[derive(Accounts)]
// FIXED: Variables mapped cleanly matching your function declaration signature exactly
#[instruction(borrow_amount: u64, position_id: u64, provided_collateral_usdc: u64, max_interest_rate_bps: u16, min_expiry_days: u8)]
pub struct OpenBorrowPosition<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,

    pub token_mint: Box<Account<'info, Mint>>,

    #[account(address = usdc::ID)]
    pub usdc_mint: Box<Account<'info, Mint>>,

    // FIXED: Validated with seeds matching your staking architecture
    #[account(
        mut,
        seeds = [b"market_vault", token_mint.key().as_ref()],
        bump = market_vault.bump,
        has_one = token_mint
    )]
    pub market_vault: Box<Account<'info, MarketVault>>,

    // FIXED: Enforced explicit PDA derivation boundaries to stop malicious account injections
    #[account(
        mut,
        seeds = [b"vault_token_account", token_mint.key().as_ref()],
        bump,
        token::mint = token_mint,
        token::authority = market_vault,
    )]
    pub vault_token_account: Box<Account<'info, TokenAccount>>,

    // FIXED: Enforced explicit PDA derivation boundaries for security
    #[account(
        mut,
        seeds = [b"global_usdc_vault"], 
        bump,
        token::mint = usdc_mint,
        token::authority = global_config,
    )]
    pub global_usdc_vault: Box<Account<'info, TokenAccount>>,

    // The authority PDA that owns the vault
    #[account(
        seeds = [b"global_config"],
        bump = global_config.bump,
    )]
    pub global_config: Box<Account<'info, GlobalConfig>>,

    // FIXED: Added seed validation to prevent attackers from using fake offer states
    #[account(
        mut,
        seeds = [
            b"offer",
            lender_offer.staker_pubkey.as_ref(),
            token_mint.key().as_ref(),
            lender_offer.offer_id.to_le_bytes().as_ref()
        ],
        bump = lender_offer.bump,
        has_one = token_mint
    )]
    pub lender_offer: Box<Account<'info, LenderOffer>>,

    // SECURITY FIX: without these, any token account of the right mint could be
    // passed as the transfer destination/source, regardless of who owns it.
    #[account(mut, token::mint = token_mint, token::authority = borrower)]
    pub borrower_token_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, token::mint = usdc_mint, token::authority = borrower)]
    pub borrower_usdc_account: Box<Account<'info, TokenAccount>>,

    // Hardcoded to the protocol cold wallet so the borrow fee can't be redirected
    #[account(
        mut,
        token::mint = usdc_mint,
        token::authority = FEE_COLLECTOR_PUBKEY,
    )]
    pub fee_collector_usdc_account: Box<Account<'info, TokenAccount>>,

    #[account(
        init,
        payer = borrower,
        space = 8 + BorrowPosition::LEN,
        seeds = [b"position", borrower.key().as_ref(), position_id.to_le_bytes().as_ref()],
        bump
    )]
    pub borrow_position: Box<Account<'info, BorrowPosition>>,

    
    //pub pyth_price_feed: Option<UncheckedAccount<'info>>,
    pub pyth_price_feed: Option<Box<Account<'info, PriceUpdateV2>>>,

    /// CHECK: address-constrained to the instructions sysvar; only read through
    /// solana_instructions_sysvar helpers to find the signed custom-oracle price.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,

    // Emergency pause: no new exposure while paused (protocol_state.rs).
    #[account(
        seeds = [PROTOCOL_STATE_SEED],
        bump = protocol_state.bump,
        constraint = !protocol_state.paused @ ShortStakeError::ProtocolPaused,
    )]
    pub protocol_state: Box<Account<'info, ProtocolState>>,
}

#[account]
pub struct BorrowPosition {
    pub borrower_pubkey: Pubkey,
    pub lender_offer_ref: Pubkey,
    pub position_id: u64,
    pub amount_borrowed: u64,
    pub collateral_deposited: u64, // 6-decimal USDC
    pub entry_price_usd: u64,
    pub initial_liquidation_price_usd: u64,
    pub loan_start_timestamp: i64,
    pub expiry_timestamp: i64, // NEW: Unix timestamp for when the loan expires
    pub bump: u8,
}

impl BorrowPosition {
    pub const LEN: usize = 32 + 32 + 8 + 8 + 8 + 8 + 8 + 8 + 8 + 1;
}

#[event]
pub struct OpenBorrowPositionEvent {
    pub borrower: Pubkey,
    pub lender_offer: Pubkey,
    pub token_mint: Pubkey,
    pub position_id: u64,
    pub amount_borrowed: u64,
    pub collateral_deposited: u64,
    pub entry_price_usd: u64,
    pub initial_liquidation_price_usd: u64,
    pub expiry_timestamp: i64,
    /// BorrowPosition PDA - primary key for the indexer's borrow_positions row.
    pub borrow_position: Pubkey,
    pub loan_start_timestamp: i64,
    /// Post-state lender_offer fields.
    pub offer_available_to_borrow: u64,
    pub offer_active_borrows: u16,
    /// Post-state market_vault.total_borrowed_amount.
    pub market_total_borrowed: u64,
    /// Protocol borrow fee (6-decimal USDC) sent to the fee collector.
    pub borrower_fee: u64,
}