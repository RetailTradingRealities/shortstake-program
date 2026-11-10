use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer, CloseAccount};
use crate::{LenderOffer, MarketVault, ShortStakeError};


/// Withdraws every token of the offer that isn't lent out. With loans still open the offer
/// account must stay (close_position / liquidate_position settle against it), so it's closed to
/// new borrowing instead: each loan's tokens return to it as the loan settles, and the staker
/// collects them with another early_unstake. The call that finds no loans open closes it.
pub fn early_unstake(ctx: Context<EarlyUnstake>) -> Result<()> {
    let offer = &ctx.accounts.lender_offer;

    // total_offered = available_to_borrow + tokens out on loan, so with no loans open this is
    // the whole offer.
    let amount_to_return = offer.available_to_borrow;
    let has_open_loans = offer.current_active_borrows > 0;
    // A fully lent offer can still be closed to new borrowing (nothing to withdraw yet; the
    // loans' tokens are collected later). Without this, a borrower rolling its loan could keep
    // the offer lent forever (Slot Zero L-04). Only a repeat call with nothing to do fails.
    require!(
        amount_to_return > 0 || !has_open_loans || !offer.is_closed,
        ShortStakeError::NothingToWithdraw
    );

    // 1. Construct PDA signer seeds for the Market Vault to authorize the token transfer
    let token_mint_key = ctx.accounts.token_mint.key();
    //let bumps = ctx.bumps.market_vault; // Safer to use the runtime bump from ctx.bumps
    let bump = ctx.accounts.market_vault.bump;
    let seeds = &[
        b"market_vault",
        token_mint_key.as_ref(),
        &[bump],
    ];
    let signer = &[&seeds[..]];

    // 2. Transfer tokens back from the global Market Vault token account to the staker's wallet
    let cpi_accounts = Transfer {
        from: ctx.accounts.vault_token_account.to_account_info(),
        to: ctx.accounts.staker_token_account.to_account_info(),
        authority: ctx.accounts.market_vault.to_account_info(),
    };
    let cpi_program = ctx.accounts.token_program.key();
    if amount_to_return > 0 {
        let cpi_ctx = CpiContext::new_with_signer(cpi_program, cpi_accounts, signer);
        token::transfer(cpi_ctx, amount_to_return)?;
    }

    let lender_offer = &mut ctx.accounts.lender_offer;
    lender_offer.total_offered = lender_offer
        .total_offered
        .checked_sub(amount_to_return)
        .ok_or(ShortStakeError::MathOverflow)?;
    lender_offer.available_to_borrow = 0;
    if has_open_loans {
        lender_offer.is_closed = true;
    }
    let offer_id = lender_offer.offer_id;
    let offer_total_offered = lender_offer.total_offered;
    let offer_active_borrows = lender_offer.current_active_borrows;
    let offer_is_closed = lender_offer.is_closed;
    let lender_offer_key = lender_offer.key();

    // 3. Update global market tracking state
    let market_vault = &mut ctx.accounts.market_vault;
    market_vault.total_staked_amount = market_vault
        .total_staked_amount
        .checked_sub(amount_to_return)
        .ok_or(ShortStakeError::MathOverflow)?;

    // Snapshot post-state for the event BEFORE the cleanup below may close the
    // market_vault account - don't read state through a closed account.
    let market_total_staked = market_vault.total_staked_amount;

    // 4. INFRASTRUCTURE CLEANUP: If this SPL token pool is now completely empty, close the accounts
    ctx.accounts.vault_token_account.reload()?; // Force refresh the local token balance state
    if ctx.accounts.vault_token_account.amount == 0
        && market_vault.total_staked_amount == 0
        && market_vault.total_borrowed_amount == 0
    {
        // close both accounts
        // A. Close the Token Vault via Token Program CPI (refunds the original PDA payer)
        let close_vault_accounts = CloseAccount {
            account: ctx.accounts.vault_token_account.to_account_info(),
            destination: ctx.accounts.payer_wallet.to_account_info(),
            authority: market_vault.to_account_info(),
        };
        let close_vault_ctx = CpiContext::new_with_signer(cpi_program, close_vault_accounts, signer);
        token::close_account(close_vault_ctx)?;

        // B. Close the Market Vault state account. Anchor's close also hands the account back to
        // the System Program and clears its data - draining only the lamports would leave the old
        // state revivable by a later instruction in the same transaction.
        market_vault.close(ctx.accounts.payer_wallet.to_account_info())?;
        
        msg!("Protocol vault empty. Closed token account and state PDA. Rent refunded to original payer.");
    }

    // 5. Close the offer account (rent back to the staker) once no loan needs it.
    let offer_account_closed = !has_open_loans;
    if offer_account_closed {
        ctx.accounts.lender_offer.close(ctx.accounts.staker.to_account_info())?;
    }

    emit!(EarlyUnstakeEvent {
        staker: ctx.accounts.staker.key(),
        token_mint: token_mint_key,
        lender_offer: lender_offer_key,
        offer_id,
        amount_returned: amount_to_return,
        market_total_staked,
        timestamp: Clock::get()?.unix_timestamp,
        offer_total_offered,
        offer_active_borrows,
        offer_is_closed,
        offer_account_closed,
    });

    Ok(())
}

#[derive(Accounts)]
pub struct EarlyUnstake<'info> {
    #[account(mut)]
    pub staker: Signer<'info>,
    /// CHECK: This is the public key of the staker, used for has_one constraint
    

    

    // Rent refund destination for the closed market_vault/vault_token_account - whoever
    // actually paid to create them (tracked in market_vault.payer), not the backend.
    /// CHECK: validated via the constraint below against market_vault.payer.
    #[account(mut, constraint = payer_wallet.key() == market_vault.payer)]
    pub payer_wallet: UncheckedAccount<'info>,

    pub token_mint: Account<'info, Mint>,

    #[account(
        mut,
        associated_token::mint = token_mint,
        associated_token::authority = staker,
    )]
    pub staker_token_account: Account<'info, TokenAccount>,

    #[account(
        mut,
        seeds = [b"market_vault", token_mint.key().as_ref()],
        bump = market_vault.bump,
        has_one = token_mint,
        has_one = vault_token_account
    )]
    pub market_vault: Account<'info, MarketVault>,

    // FIXED: Added seed constraints to prevent arbitrary account injection vulnerabilities
    #[account(
        mut,
        seeds = [b"vault_token_account", token_mint.key().as_ref()],
        bump,
    )]
    pub vault_token_account: Account<'info, TokenAccount>,

    // Not `close = staker`: the offer must outlive its open loans. Closed in the handler instead.
    #[account(
        mut,
        seeds = [
            b"offer",
            staker.key().as_ref(),
            token_mint.key().as_ref(),
            lender_offer.offer_id.to_le_bytes().as_ref()
        ],
        bump = lender_offer.bump,
        //has_one = staker,
        constraint = lender_offer.staker_pubkey == staker.key(),
        has_one = token_mint
    )]
    pub lender_offer: Account<'info, LenderOffer>,

    pub token_program: Program<'info, Token>,
}

#[event]
pub struct EarlyUnstakeEvent {
    pub staker: Pubkey,
    pub token_mint: Pubkey,
    pub lender_offer: Pubkey,
    pub offer_id: u64,
    pub amount_returned: u64,
    /// Post-state market_vault.total_staked_amount (0 if the market drained and closed).
    pub market_total_staked: u64,
    pub timestamp: i64,
    /// Post-state lender_offer fields. With loans still open the offer stays, closed to new
    /// borrowing (offer_is_closed), holding only the lent-out tokens (offer_total_offered).
    pub offer_total_offered: u64,
    pub offer_active_borrows: u16,
    pub offer_is_closed: bool,
    /// True once no loans were open: the offer account itself was closed.
    pub offer_account_closed: bool,
}
