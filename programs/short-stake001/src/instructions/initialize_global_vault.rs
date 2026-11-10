use anchor_lang::prelude::*;
use anchor_spl::token::{Mint, Token, TokenAccount};
use crate::constants::{ADMIN_PUBKEY, usdc};
use crate::GlobalConfig;

// Deployer Wallet Pubkey imported from constants.rs


pub fn initialize_global_vault(_ctx: Context<InitializeGlobalVault>) -> Result<()> {
    // The `init` macro in your struct already handled the account creation, 
    // rent payment, and token account formatting.
    
    msg!("Global USDC Vault initialized successfully!");
    
    Ok(())
}


#[derive(Accounts)]
pub struct InitializeGlobalVault<'info> {
    #[account(mut, address = ADMIN_PUBKEY)]
    pub admin: Signer<'info>,

    /// CHECK: The PDA that will be set as the authority over the global_usdc_vault.
    /// Update the seeds to match how your global_config is derived.
    #[account(
        seeds = [b"global_config"], 
        bump = global_config.bump,
    )]
    pub global_config: Account<'info, GlobalConfig>,

     #[account(address = usdc::ID)]
    pub usdc_mint: Account<'info, Mint>,

    #[account(
        init,
        payer = admin,
        seeds = [b"global_usdc_vault"],
        bump,
        token::mint = usdc_mint,
        token::authority = global_config,
    )]
    pub global_usdc_vault: Account<'info, TokenAccount>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
}