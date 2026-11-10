use anchor_lang::prelude::*;
use crate::instructions::GlobalConfig;

/// Emergency pause. While paused, deposit_stake and open_borrow_position are refused (no new
/// exposure). close_position, early_unstake and liquidate_position never read this account, so a
/// pause can't trap funds or stop liquidations.
///
/// A separate PDA rather than a field on GlobalConfig: growing GlobalConfig would make every
/// deployed config account undecodable until migrated.
#[account]
#[derive(InitSpace)]
pub struct ProtocolState {
    pub paused: bool,
    pub bump: u8,
}

pub const PROTOCOL_STATE_SEED: &[u8] = b"protocol_state";

/// Pause or unpause new stakes and borrows. Config authority only (the multisig on mainnet).
/// Also creates ProtocolState on deployments initialized before it existed (devnet): a first
/// `set_paused(false)` there. Both init_if_needed paths leave it fully written (both fields).
pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()> {
    let state = &mut ctx.accounts.protocol_state;
    state.paused = paused;
    state.bump = ctx.bumps.protocol_state;
    emit!(PauseChangedEvent {
        paused,
        authority: ctx.accounts.authority.key(),
        timestamp: Clock::get()?.unix_timestamp,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct SetPaused<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        seeds = [b"global_config"],
        bump = global_config.bump,
        has_one = authority,
    )]
    pub global_config: Account<'info, GlobalConfig>,

    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + ProtocolState::INIT_SPACE,
        seeds = [PROTOCOL_STATE_SEED],
        bump
    )]
    pub protocol_state: Account<'info, ProtocolState>,

    pub system_program: Program<'info, System>,
}

#[event]
pub struct PauseChangedEvent {
    pub paused: bool,
    pub authority: Pubkey,
    pub timestamp: i64,
}
