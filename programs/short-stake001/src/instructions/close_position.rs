use anchor_lang::prelude::*;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};
use crate::constants::usdc;
use crate::pyth_price_update::PriceUpdateV2;
use crate::{get_current_price, ShortStakeError};
use crate::constants::{FEE_COLLECTOR_PUBKEY, PRICE_TO_USDC_DIVISOR};
use crate::math::{accrued_interest_usdc, billable_hours, bps_of_ceil, token_divisor};
use crate::signed_price::{PriceAction, INSTRUCTIONS_SYSVAR_ID};
use crate::instructions::{
    BorrowPosition,
    GlobalConfig,
    LenderOffer,
    MarketVault,
};



pub fn close_position<'info>(
    ctx: Context<ClosePosition<'info>>,
) -> Result<()> {
    let borrow_pos = &ctx.accounts.borrow_position;
    let lender_offer = &mut ctx.accounts.lender_offer;
    let market_vault = &mut ctx.accounts.market_vault;

    // 1. Fetch live price (10^-12 USD per whole token) to calculate interest, routed by the vault's stored oracle mode
    let current_price_usdc = get_current_price(
            market_vault.token_mint,
            &market_vault.expected_oracle_feed,
            &ctx.accounts.pyth_price_feed,
            ctx.accounts.instructions_sysvar.as_ref(),
            ctx.accounts.global_config.oracle_signer,
            PriceAction::ClosePosition,
        )?;

    msg!("The current asset price is: {}", current_price_usdc);

    let token_decimals = ctx.accounts.token_mint.decimals;
    let token_divisor = token_divisor(token_decimals)?;

    // Widened to u128: amount_borrowed * current_price_usdc can exceed u64::MAX
    // for large positions on high-decimal mints (same class as open_borrow_position).
    let current_notional_usdc: u64 = (borrow_pos.amount_borrowed as u128)
        .checked_mul(current_price_usdc as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(token_divisor as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(PRICE_TO_USDC_DIVISOR)
        .ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    // 2. Calculate Accrued Interest in USDC
    let current_time = Clock::get()?.unix_timestamp;
    let seconds_elapsed = current_time
        .checked_sub(borrow_pos.loan_start_timestamp)
        .ok_or(ShortStakeError::MathOverflow)?;

    // Every started hour at the offer's daily rate, rounded up (see math.rs).
    let accrued_interest_usdc = accrued_interest_usdc(
        current_notional_usdc,
        lender_offer.interest_rate_bps,
        billable_hours(seconds_elapsed)?,
    )?;

    // 3. Ensure collateral covers the interest
    require!(
        borrow_pos.collateral_deposited >= accrued_interest_usdc,
        ShortStakeError::BadDebtDetected
    );

    // Protocol cut of the staker's interest: fee_rate_stakers_bps of accrued interest (500 bps = 5%),
    // rounded up. The borrower still pays the full interest; it's split between staker and fee
    // collector. Fee rates are capped at 20%, so the cut never exceeds the interest.
    let staker_fee_usdc = bps_of_ceil(accrued_interest_usdc, ctx.accounts.global_config.fee_rate_stakers_bps)?;

    let staker_interest_usdc = accrued_interest_usdc
        .checked_sub(staker_fee_usdc)
        .ok_or(ShortStakeError::MathOverflow)?;

    let remaining_collateral_usdc = borrow_pos
        .collateral_deposited
        .checked_sub(accrued_interest_usdc)
        .ok_or(ShortStakeError::MathOverflow)?;

    // 4. Return Borrowed SPL Tokens (Borrower -> Vault)
    let cpi_accounts_return = Transfer {
        from: ctx.accounts.borrower_token_account.to_account_info(),
        to: ctx.accounts.vault_token_account.to_account_info(),
        authority: ctx.accounts.borrower.to_account_info(),
    };
    token::transfer(
        CpiContext::new(ctx.accounts.token_program.key(), cpi_accounts_return),
        borrow_pos.amount_borrowed,
    )?;

    // 5. Payout USDC (Global Protocol Vault -> Staker & Borrower)
    let token_mint_key = ctx.accounts.token_mint.key();
    let global_config_seeds = &[
    b"global_config".as_ref(),
    &[ctx.accounts.global_config.bump],
];
    let signer = &[&global_config_seeds[..]];

    let transfer_usdc = |to: AccountInfo<'info>, amount: u64| -> Result<()> {
        if amount == 0 { return Ok(()); }
        let cpi_accounts = Transfer {
            from: ctx.accounts.global_usdc_vault.to_account_info(),
            to,
            authority: ctx.accounts.global_config.to_account_info(),
        };
        token::transfer(
            CpiContext::new_with_signer(ctx.accounts.token_program.key(), cpi_accounts, signer),
            amount,
        )
    };

    // Pay interest (net of protocol fee) safely to the verified staker token account destination
    transfer_usdc(ctx.accounts.staker_usdc_account.to_account_info(), staker_interest_usdc)?;

    // Protocol's share of the interest
    transfer_usdc(ctx.accounts.fee_collector_usdc_account.to_account_info(), staker_fee_usdc)?;
    
    // Return original collateral (minus interest) to borrower
    transfer_usdc(ctx.accounts.borrower_usdc_account.to_account_info(), remaining_collateral_usdc)?;

    // 6. Update Parent Offer & Global State counters
    lender_offer.available_to_borrow = lender_offer
        .available_to_borrow
        .checked_add(borrow_pos.amount_borrowed)
        .ok_or(ShortStakeError::MathOverflow)?;
    lender_offer.current_active_borrows = lender_offer
        .current_active_borrows
        .checked_sub(1)
        .ok_or(ShortStakeError::MathOverflow)?;

    market_vault.total_borrowed_amount = market_vault
        .total_borrowed_amount
        .checked_sub(borrow_pos.amount_borrowed)
        .ok_or(ShortStakeError::MathOverflow)?;

    emit!(ClosePositionEvent {
        borrower: ctx.accounts.borrower.key(),
        staker: lender_offer.staker_pubkey,
        token_mint: token_mint_key,
        lender_offer: lender_offer.key(),
        borrow_position: ctx.accounts.borrow_position.key(),
        position_id: borrow_pos.position_id,
        offer_id: lender_offer.offer_id,
        amount_returned: borrow_pos.amount_borrowed,
        interest_paid: staker_interest_usdc,
        collateral_returned: remaining_collateral_usdc,
        exit_price_usd: current_price_usdc,
        offer_available_to_borrow: lender_offer.available_to_borrow,
        offer_active_borrows: lender_offer.current_active_borrows,
        market_total_borrowed: market_vault.total_borrowed_amount,
        timestamp: current_time,
        staker_fee: staker_fee_usdc,
        offer_is_closed: lender_offer.is_closed,
    });

   

    Ok(())
}

#[derive(Accounts)]
pub struct ClosePosition<'info> {
    #[account(mut)]
    pub borrower: Signer<'info>,

    pub token_mint: Box<Account<'info, Mint>>,
    
    #[account(address = usdc::ID)]
    pub usdc_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = token_mint,
        associated_token::authority = borrower,
    )]
    pub borrower_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [b"market_vault", token_mint.key().as_ref()],
        bump = market_vault.bump,
        has_one = token_mint
    )]
    pub market_vault: Box<Account<'info, MarketVault>>,

    // FIXED: Enforced strict PDA boundaries to neutralize account substitution exploits
    #[account(
        mut,
        seeds = [b"vault_token_account", token_mint.key().as_ref()],
        bump,
    )]
    pub vault_token_account: Box<Account<'info, TokenAccount>>,

    
    // FIXED: Switched to the global static string seed configuration
    #[account(
        mut,
        seeds = [b"global_usdc_vault"],
        bump ,
        token::mint = usdc_mint,
        token::authority = global_config,
    )]
    pub global_usdc_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        seeds = [b"global_config"],
        bump = global_config.bump, // <-- Anchor reads it directly from the struct
    )]
    pub global_config: Box<Account<'info, GlobalConfig>>,


    // Interest goes to ANY USDC account the staker owns, not only its ATA: an ATA-only rule let a
    // staker block its borrowers' closes by handing its ATA to another key (Slot Zero H-01).
    #[account(
        mut,
        token::mint = usdc_mint,
        token::authority = lender_offer.staker_pubkey,
    )]
    pub staker_usdc_account: Box<Account<'info, TokenAccount>>,

    #[account(mut, token::mint = usdc_mint, token::authority = borrower)]
    pub borrower_usdc_account: Box<Account<'info, TokenAccount>>,

    // Hardcoded to the protocol cold wallet so the staker fee can't be redirected
    #[account(
        mut,
        token::mint = usdc_mint,
        token::authority = FEE_COLLECTOR_PUBKEY,
    )]
    pub fee_collector_usdc_account: Box<Account<'info, TokenAccount>>,

    // FIXED: Anchored verification parameters directly against state properties instead of untrusted signatures
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

    // FIXED: Aligned derivation parameters perfectly with open_borrow_position seeds prefix
    #[account(
        mut,
        close = borrower, 
        seeds = [b"position", borrower.key().as_ref(), borrow_position.position_id.to_le_bytes().as_ref()],
        bump = borrow_position.bump,
        constraint = borrow_position.lender_offer_ref == lender_offer.key(),
    )]
    pub borrow_position: Box<Account<'info, BorrowPosition>>,

    
    pub pyth_price_feed: Option<Box<Account<'info, PriceUpdateV2>>>,

    /// CHECK: address-constrained to the instructions sysvar; only read through
    /// solana_instructions_sysvar helpers to find the signed custom-oracle price.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[event]
pub struct ClosePositionEvent {
    pub borrower: Pubkey,
    pub staker: Pubkey,
    pub token_mint: Pubkey,
    pub lender_offer: Pubkey,
    pub borrow_position: Pubkey,
    pub position_id: u64,
    pub offer_id: u64,
    pub amount_returned: u64,
    /// Interest received by the staker, net of the protocol fee.
    pub interest_paid: u64,
    pub collateral_returned: u64,
    pub exit_price_usd: u64,
    pub offer_available_to_borrow: u64,
    pub offer_active_borrows: u16,
    pub market_total_borrowed: u64,
    pub timestamp: i64,
    /// Protocol's cut of the interest (6-decimal USDC) sent to the fee collector.
    pub staker_fee: u64,
    /// Post-state lender_offer.is_closed (closed by an earlier bad-debt write-off).
    pub offer_is_closed: bool,
}
