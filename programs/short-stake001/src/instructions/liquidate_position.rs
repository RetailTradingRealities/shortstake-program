use anchor_lang::prelude::*;
use crate::pyth_price_update::PriceUpdateV2;
use anchor_spl::token::{self, Token, TokenAccount, Transfer, Mint};
use crate::constants::usdc;
use crate::{get_current_price, ShortStakeError};
use crate::constants::PRICE_TO_USDC_DIVISOR;
use crate::math::{accrued_interest_usdc, billable_hours, bps_of_ceil, token_divisor};
use crate::signed_price::{PriceAction, INSTRUCTIONS_SYSVAR_ID};
use crate::instructions::{
    BorrowPosition,
    GlobalConfig,
    LenderOffer,
    MarketVault,
};

pub fn liquidate_position<'info>(
    ctx: Context<LiquidatePosition<'info>>,
) -> Result<()> {
    let token_mint_key = ctx.accounts.token_mint.key();
    // Payout accounts: ANY token account the counterparty owns (the token::authority constraints
    // below), not only its ATA. An owner can hand its ATA to another key (SetAuthority), and an
    // ATA-only rule then let a borrower block its own liquidation, or a staker block its
    // borrowers' closes (Slot Zero H-01). Anyone can create an account owned by the counterparty
    // (InitializeAccount3) and gets no rights over it, so nobody can veto settlement.

    let borrow_pos = &ctx.accounts.borrow_position;
    let lender_offer = &mut ctx.accounts.lender_offer;
    let market_vault = &mut ctx.accounts.market_vault;
    let current_time = Clock::get()?.unix_timestamp;

    // 1. Fetch live price (10^-12 USD per whole token), routed by the vault's stored oracle mode
    let current_price_usdc = get_current_price(
            market_vault.token_mint,
            &market_vault.expected_oracle_feed,
            &ctx.accounts.pyth_price_feed,
            ctx.accounts.instructions_sysvar.as_ref(),
            ctx.accounts.global_config.oracle_signer,
            PriceAction::LiquidatePosition,
        )?;

    let token_decimals = ctx.accounts.token_mint.decimals;
    let token_divisor = token_divisor(token_decimals)?;

    // 2. Calculate Current & Initial Notional Value (in USDC)
    // Widened to u128: amount_borrowed * price can exceed u64::MAX for large
    // positions on high-decimal mints (same class as open_borrow_position).
    let current_notional_usdc: u64 = (borrow_pos.amount_borrowed as u128)
        .checked_mul(current_price_usdc as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(token_divisor as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(PRICE_TO_USDC_DIVISOR)
        .ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    let initial_notional_usdc: u64 = (borrow_pos.amount_borrowed as u128)
        .checked_mul(borrow_pos.entry_price_usd as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(token_divisor as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_div(PRICE_TO_USDC_DIVISOR)
        .ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    // 3. Calculate Unrealized PnL
    let (pnl_is_profit, pnl_usdc) = if initial_notional_usdc >= current_notional_usdc {
        (true, initial_notional_usdc - current_notional_usdc)
    } else {
        (false, current_notional_usdc - initial_notional_usdc)
    };

    // 4. Calculate Accrued Interest 
    let seconds_elapsed = current_time
        .checked_sub(borrow_pos.loan_start_timestamp)
        .ok_or(ShortStakeError::MathOverflow)?;

    // Same rule as close_position (math.rs): every started hour, rounded up.
    let accrued_interest_usdc = accrued_interest_usdc(
        current_notional_usdc,
        lender_offer.interest_rate_bps,
        billable_hours(seconds_elapsed)?,
    )?;

    // 5. Compute Total Equity Safely (Capped at 0 to avoid breaking under bad debt)
    let mut total_equity = borrow_pos.collateral_deposited;
    if pnl_is_profit {
        total_equity = total_equity.checked_add(pnl_usdc).ok_or(ShortStakeError::MathOverflow)?;
    } else {
        total_equity = if total_equity >= pnl_usdc { total_equity - pnl_usdc } else { 0 };
    }

    total_equity = if total_equity >= accrued_interest_usdc { total_equity - accrued_interest_usdc } else { 0 };

    // 6. Check Liquidation Triggers (Margin OR Time)
    // Rounded up: the margin protects the staker, so rounding never delays a liquidation.
    let maintenance_margin = bps_of_ceil(current_notional_usdc, ctx.accounts.global_config.liquidation_thresh_bps)?;

    let is_undercollateralized = total_equity <= maintenance_margin;
    let is_expired = current_time >= borrow_pos.expiry_timestamp;

    require!(
        is_undercollateralized || is_expired,
        ShortStakeError::PositionHealthy
    );

    // 7. Calculate Liquidation Penalties & Bounties
    // Owed by the borrower: rounded up. The bounty below is a payout: rounded down.
    let total_penalty = bps_of_ceil(current_notional_usdc, 500)?;

    let liquidator_bounty: u64 = (current_notional_usdc as u128)
        .checked_mul(400u128).ok_or(ShortStakeError::MathOverflow)?
        .checked_div(10_000u128).ok_or(ShortStakeError::MathOverflow)?
        .try_into()
        .map_err(|_| ShortStakeError::MathOverflow)?;

    let protocol_fee = total_penalty.checked_sub(liquidator_bounty).ok_or(ShortStakeError::MathOverflow)?;

    let total_obligation = current_notional_usdc
        .checked_add(accrued_interest_usdc).ok_or(ShortStakeError::MathOverflow)?
        .checked_add(total_penalty).ok_or(ShortStakeError::MathOverflow)?;

    // The liquidator fronts current_notional_usdc buying the SPL tokens back off-chain, so they
    // must be reimbursed that principal plus the bounty - otherwise no rational bot will act.
    let liquidator_total_payout = current_notional_usdc
        .checked_add(liquidator_bounty)
        .ok_or(ShortStakeError::MathOverflow)?;

    let collateral = borrow_pos.collateral_deposited;
    let is_bad_debt = collateral < total_obligation;

    // Write-off: the collateral can't make a liquidator whole, so a buyback would never happen and
    // the position would be stuck forever. Instead the token debt is forgiven, ALL collateral goes
    // to the staker as compensation for the tokens they won't get back, and the offer is closed.
    let is_written_off = collateral < liquidator_total_payout;

    // 8. Payout waterfall. Liquidator first (without them the staker's tokens never come back),
    // then staker interest, then the protocol penalty share, and the borrower gets the rest.
    let (pay_staker, liquidator_reimbursement, liquidator_bounty_paid, pay_protocol, remaining_borrower_usdc) =
        if is_written_off {
            (0, 0, 0, 0, 0)
        } else {
            let mut budget = collateral - liquidator_total_payout;
            let s_pay = std::cmp::min(budget, accrued_interest_usdc);
            budget -= s_pay;
            let p_pay = std::cmp::min(budget, protocol_fee);
            budget -= p_pay;
            (s_pay, current_notional_usdc, liquidator_bounty, p_pay, budget)
        };
    let staker_compensation = if is_written_off { collateral } else { 0 };

    // Protocol cut of the staker's interest (fee_rate_stakers_bps, 500 bps = 5%), taken from the
    // interest actually paid. No fee on a write-off: that payout is principal compensation.
    let staker_fee_usdc = bps_of_ceil(pay_staker, ctx.accounts.global_config.fee_rate_stakers_bps)?;
    let staker_interest_usdc = pay_staker.checked_sub(staker_fee_usdc).ok_or(ShortStakeError::MathOverflow)?;
    let fee_collector_total = pay_protocol.checked_add(staker_fee_usdc).ok_or(ShortStakeError::MathOverflow)?;
    let pay_staker_usdc = staker_interest_usdc.checked_add(staker_compensation).ok_or(ShortStakeError::MathOverflow)?;

    let pay_bot = liquidator_reimbursement
        .checked_add(liquidator_bounty_paid)
        .ok_or(ShortStakeError::MathOverflow)?;

    let cpi_program = ctx.accounts.token_program.key();
    let market_vault_seeds = &[
        b"market_vault".as_ref(),
        token_mint_key.as_ref(),
        &[market_vault.bump],
    ];
    let market_vault_signer = &[&market_vault_seeds[..]];

    // 9. Tokens. Buyback: the liquidator returns the borrowed tokens to the vault. Write-off: none
    // come back, and the offer's still-unborrowed tokens are returned to the staker.
    let tokens_returned_to_staker = if is_written_off { lender_offer.available_to_borrow } else { 0 };
    if is_written_off {
        if tokens_returned_to_staker > 0 {
            let staker_token_account = ctx
                .accounts
                .staker_token_account
                .as_ref()
                .ok_or(ShortStakeError::MissingStakerTokenAccount)?;
            let cpi_accounts_return = Transfer {
                from: ctx.accounts.vault_token_account.to_account_info(),
                to: staker_token_account.to_account_info(),
                authority: market_vault.to_account_info(),
            };
            token::transfer(
                CpiContext::new_with_signer(cpi_program.clone(), cpi_accounts_return, market_vault_signer),
                tokens_returned_to_staker,
            )?;
        }
    } else {
        let cpi_accounts_buyback = Transfer {
            from: ctx.accounts.liquidator_token_account.to_account_info(),
            to: ctx.accounts.vault_token_account.to_account_info(),
            authority: ctx.accounts.liquidator.to_account_info(),
        };
        token::transfer(CpiContext::new(cpi_program.clone(), cpi_accounts_buyback), borrow_pos.amount_borrowed)?;
    }

    // 10. Execute USDC Payouts from Global Vault
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
        token::transfer(CpiContext::new_with_signer(cpi_program.clone(), cpi_accounts, signer), amount)
    };

    transfer_usdc(ctx.accounts.staker_usdc_account.to_account_info(), pay_staker_usdc)?;
    transfer_usdc(ctx.accounts.liquidator_usdc_account.to_account_info(), pay_bot)?;
    // Liquidation penalty share + staker-interest fee, in one transfer
    transfer_usdc(ctx.accounts.fee_collector_usdc_account.to_account_info(), fee_collector_total)?;
    transfer_usdc(ctx.accounts.borrower_usdc_account.to_account_info(), remaining_borrower_usdc)?;

    // Snapshot identifiers/amounts before borrow_position is closed by the accounts constraint
    let position_id = borrow_pos.position_id;
    let amount_borrowed = borrow_pos.amount_borrowed;
    let borrower_pubkey = borrow_pos.borrower_pubkey;
    let offer_id = lender_offer.offer_id;
    let staker_pubkey = lender_offer.staker_pubkey;

    // 11. Update Parent Offer & Global State
    lender_offer.current_active_borrows = lender_offer
        .current_active_borrows
        .checked_sub(1)
        .ok_or(ShortStakeError::MathOverflow)?;
    market_vault.total_borrowed_amount = market_vault
        .total_borrowed_amount
        .checked_sub(amount_borrowed)
        .ok_or(ShortStakeError::MathOverflow)?;

    if is_written_off {
        // The written-off tokens and the returned unborrowed tokens both leave the offer for good.
        let removed = amount_borrowed
            .checked_add(tokens_returned_to_staker)
            .ok_or(ShortStakeError::MathOverflow)?;
        lender_offer.total_offered = lender_offer
            .total_offered
            .checked_sub(removed)
            .ok_or(ShortStakeError::MathOverflow)?;
        lender_offer.available_to_borrow = 0;
        lender_offer.is_closed = true;
        market_vault.total_staked_amount = market_vault
            .total_staked_amount
            .checked_sub(removed)
            .ok_or(ShortStakeError::MathOverflow)?;
    } else {
        lender_offer.available_to_borrow = lender_offer
            .available_to_borrow
            .checked_add(amount_borrowed)
            .ok_or(ShortStakeError::MathOverflow)?;
    }

    let offer_available_to_borrow = lender_offer.available_to_borrow;
    let offer_active_borrows = lender_offer.current_active_borrows;
    let offer_total_offered = lender_offer.total_offered;
    let offer_is_closed = lender_offer.is_closed;
    let lender_offer_key = lender_offer.key();

    // Close the offer outright once nothing is lent out from it (rent back to the staker). If other
    // loans are still open it stays, closed to new borrowing; the staker withdraws what comes
    // back with early_unstake, which closes it once the last loan has settled.
    let offer_account_closed = is_written_off && offer_active_borrows == 0;
    if offer_account_closed {
        lender_offer.close(ctx.accounts.staker.to_account_info())?;
    }

    emit!(LiquidateEvent {
        liquidator: ctx.accounts.liquidator.key(),
        borrower: borrower_pubkey,
        staker: staker_pubkey,
        token_mint: token_mint_key,
        lender_offer: lender_offer_key,
        borrow_position: ctx.accounts.borrow_position.key(),
        position_id,
        offer_id,
        amount_liquidated: amount_borrowed,
        exit_price_usd: current_price_usdc,
        interest_paid_to_staker: staker_interest_usdc,
        liquidator_bounty: liquidator_bounty_paid,
        liquidator_reimbursement,
        protocol_fee: pay_protocol,
        collateral_returned: remaining_borrower_usdc,
        is_expired,
        is_bad_debt,
        offer_available_to_borrow,
        offer_active_borrows,
        market_total_borrowed: market_vault.total_borrowed_amount,
        timestamp: current_time,
        staker_fee: staker_fee_usdc,
        is_written_off,
        staker_compensation,
        tokens_returned_to_staker,
        offer_total_offered,
        offer_account_closed,
        market_total_staked: market_vault.total_staked_amount,
        offer_is_closed,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct LiquidatePosition<'info> {
    #[account(mut)]
    pub liquidator: Signer<'info>,


    /// CHECK: Validated explicitly via the borrow_position data bridge match below
    #[account(mut, address = borrow_position.borrower_pubkey)]
    pub borrower: UncheckedAccount<'info>,

    /// CHECK: Validated explicitly via the lender_offer data bridge match below
    #[account(mut, address = lender_offer.staker_pubkey)]
    pub staker: UncheckedAccount<'info>,

    pub token_mint: Box<Account<'info, Mint>>,

    #[account(address = usdc::ID)]
    pub usdc_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = token_mint,
        associated_token::authority = liquidator,
    )]
    pub liquidator_token_account: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        seeds = [b"market_vault", token_mint.key().as_ref()],
        bump = market_vault.bump,
        has_one = token_mint
    )]
    pub market_vault: Box<Account<'info, MarketVault>>,

    #[account(
        mut,
        seeds = [b"vault_token_account", token_mint.key().as_ref()],
        bump,
    )]
    pub vault_token_account: Box<Account<'info, TokenAccount>>,

    // FIXED: Secured with your hardcoded global configuration seed string
    #[account(
        mut,
        seeds = [b"global_usdc_vault"],
        bump,
        token::mint = usdc_mint,
        token::authority = global_config,
    )]
    pub global_usdc_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        seeds = [b"global_config"],
        bump=global_config.bump,
    )]
    pub global_config: Box<Account<'info, GlobalConfig>>,



    // Payouts go to accounts the staker / borrower OWN; any such account is accepted (see the
    // handler's comment on Slot Zero H-01).
    #[account(mut, token::mint = usdc_mint, token::authority = staker.key())]
    pub staker_usdc_account: Box<Account<'info, TokenAccount>>,

    // Write-off only: receives the offer's unborrowed tokens. Optional (Slot Zero H-01): a buyback
    // never uses it, so it isn't required there; a write-off with unborrowed tokens needs it.
    // When passed, it must be a token account of this mint that the staker owns.
    #[account(mut, token::mint = token_mint, token::authority = staker.key())]
    pub staker_token_account: Option<Box<Account<'info, TokenAccount>>>,
    
    #[account(mut, token::mint = usdc_mint, token::authority = liquidator.key())]
    pub liquidator_usdc_account: Box<Account<'info, TokenAccount>>,
    
    // FIXED: Hardcoded to your cold-wallet address to protect protocol profits permanently
    #[account(
        mut, 
        token::mint = usdc_mint, 
        token::authority = crate::constants::FEE_COLLECTOR_PUBKEY
    )]
    pub fee_collector_usdc_account: Box<Account<'info, TokenAccount>>,
    
    #[account(mut, token::mint = usdc_mint, token::authority = borrower.key())]
    pub borrower_usdc_account: Box<Account<'info, TokenAccount>>,

    // FIXED: Enforced strict historical tracking derivation boundaries
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

    // FIXED: Closed upon completion and rent lamports rewarded right to the executing liquidator bot
    #[account(
        mut,
        close = liquidator, 
        seeds = [b"position", borrower.key().as_ref(), borrow_position.position_id.to_le_bytes().as_ref()],
        bump = borrow_position.bump,
        constraint = borrow_position.lender_offer_ref == lender_offer.key(),
    )]
        
    pub borrow_position: Box<Account<'info, BorrowPosition>>,
    
    pub pyth_price_feed: Option<Box<Account<'info, PriceUpdateV2>>>,
    //pub pyth_price_feed: Option<UncheckedAccount<'info>>,

    /// CHECK: address-constrained to the instructions sysvar; only read through
    /// solana_instructions_sysvar helpers to find the signed custom-oracle price.
    #[account(address = INSTRUCTIONS_SYSVAR_ID)]
    pub instructions_sysvar: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}

#[event]
pub struct LiquidateEvent {
    pub liquidator: Pubkey,
    pub borrower: Pubkey,
    pub staker: Pubkey,
    pub token_mint: Pubkey,
    pub lender_offer: Pubkey,
    pub borrow_position: Pubkey,
    pub position_id: u64,
    pub offer_id: u64,
    pub amount_liquidated: u64,
    pub exit_price_usd: u64,
    /// Interest received by the staker, net of the protocol fee.
    pub interest_paid_to_staker: u64,
    pub liquidator_bounty: u64,
    pub liquidator_reimbursement: u64,
    /// Protocol's share of the liquidation penalty (excludes staker_fee).
    pub protocol_fee: u64,
    pub collateral_returned: u64,
    pub is_expired: bool,
    pub is_bad_debt: bool,
    pub offer_available_to_borrow: u64,
    pub offer_active_borrows: u16,
    pub market_total_borrowed: u64,
    pub timestamp: i64,
    /// Protocol's cut of the staker's interest (6-decimal USDC); also sent to the fee collector.
    pub staker_fee: u64,
    /// Bad-debt write-off: no buyback, the token debt is forgiven and the offer is closed.
    pub is_written_off: bool,
    /// Write-off only: all collateral (6-decimal USDC) paid to the staker for the lost tokens.
    pub staker_compensation: u64,
    /// Write-off only: the offer's unborrowed tokens returned to the staker.
    pub tokens_returned_to_staker: u64,
    /// Post-state lender_offer.total_offered.
    pub offer_total_offered: u64,
    /// True if the lender_offer account was closed by this instruction.
    pub offer_account_closed: bool,
    /// Post-state market_vault.total_staked_amount.
    pub market_total_staked: u64,
    /// Post-state lender_offer.is_closed (no new borrows; set by a write-off).
    pub offer_is_closed: bool,
}


