use crate::constants::{
    ADMIN_PUBKEY, MAX_FEE_RATE_BORROWERS_BPS, MAX_FEE_RATE_STAKERS_BPS, MAX_LIQUIDATION_THRESH_BPS,
    MIN_LIQUIDATION_THRESH_BPS,
};
use crate::ShortStakeError;
use crate::instructions::{ProtocolState, PROTOCOL_STATE_SEED};
use anchor_lang::prelude::*;

#[account]
#[derive(InitSpace)]
pub struct GlobalConfig {
    pub authority: Pubkey,
    pub pending_authority: Option<Pubkey>, // <-- Add this field (takes 33 bytes)
    // Backend key that signs custom-oracle prices off-chain (see signed_price.rs).
    // Kept separate from `authority` so the hot backend key can't change protocol config.
    pub oracle_signer: Pubkey,
    // Protocol cut of the staker's USDC interest, taken at close/liquidation (500 = 5.00%).
    pub fee_rate_stakers_bps: u16,
    // Protocol fee on the USD notional borrowed, charged at open (25 = 0.25%).
    pub fee_rate_borrowers_bps: u16,
    pub liquidation_thresh_bps: u16,
    pub min_expiry_days: u8,
    pub max_expiry_days: u8,
    pub bump: u8,
}
//==============================================================================================
/// Instruction 0: Initialize Global Configuration
/// This instruction sets up the initial global configuration for the protocol.
/// The admin who calls this instruction becomes the initial authority.
pub fn initialize_config(
    ctx: Context<InitializeConfig>,
    fee_rate_stakers_bps: u16,
    fee_rate_borrowers_bps: u16,
    liquidation_thresh_bps: u16,
    min_expiry_days: u8,
    max_expiry_days: u8,
    oracle_signer: Pubkey,
) -> Result<()> {
    require!(
        fee_rate_stakers_bps <= MAX_FEE_RATE_STAKERS_BPS,
        ShortStakeError::InvalidFeeRate
    );
    require!(
        fee_rate_borrowers_bps <= MAX_FEE_RATE_BORROWERS_BPS,
        ShortStakeError::InvalidFeeRate
    );
    require!(
        (MIN_LIQUIDATION_THRESH_BPS..=MAX_LIQUIDATION_THRESH_BPS).contains(&liquidation_thresh_bps),
        ShortStakeError::InvalidLiquidationThreshold
    );
    require_valid_expiry_bounds(min_expiry_days, max_expiry_days)?;

    let global_config = &mut ctx.accounts.global_config;

    // Set the admin who initialized it as the authority
    global_config.authority = ctx.accounts.admin.key();

    // Assign the protocol-wide variables
    global_config.fee_rate_stakers_bps = fee_rate_stakers_bps;
    global_config.fee_rate_borrowers_bps = fee_rate_borrowers_bps;
    global_config.liquidation_thresh_bps = liquidation_thresh_bps;
    global_config.min_expiry_days = min_expiry_days;
    global_config.max_expiry_days = max_expiry_days;
    global_config.oracle_signer = oracle_signer;

    global_config.bump = ctx.bumps.global_config;

    let protocol_state = &mut ctx.accounts.protocol_state;
    protocol_state.paused = false;
    protocol_state.bump = ctx.bumps.protocol_state;

    msg!("Global config initialized successfully.");
    Ok(())
}

/// 1 <= min <= max (Slot Zero I-04): a 0-day minimum or an inverted range would let offers
/// expire at once or make every deposit fail.
fn require_valid_expiry_bounds(min_expiry_days: u8, max_expiry_days: u8) -> Result<()> {
    require!(
        min_expiry_days >= 1 && min_expiry_days <= max_expiry_days,
        ShortStakeError::InvalidExpiryBounds
    );
    Ok(())
}

/// Accounts context for initializing the global configuration
#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    // SECURITY: Hardcoded to prevent front-running the init of this singleton PDA
    #[account(mut, address = ADMIN_PUBKEY)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        // 8 bytes for the Anchor discriminator + the size of the struct
        space = 8 + GlobalConfig::INIT_SPACE,
        seeds = [b"global_config"], // Static seed guarantees a singleton
        bump
    )]
    pub global_config: Account<'info, GlobalConfig>,

    pub system_program: Program<'info, System>,

    // Created with the config, unpaused (see protocol_state.rs).
    #[account(
        init,
        payer = admin,
        space = 8 + ProtocolState::INIT_SPACE,
        seeds = [PROTOCOL_STATE_SEED],
        bump
    )]
    pub protocol_state: Account<'info, ProtocolState>,
}

//==============================================================================================
//New instruction to update the global configuration

pub fn update_config(
    ctx: Context<UpdateConfig>,
    new_fee_stakers_bps: Option<u16>,
    new_fee_borrowers_bps: Option<u16>,
    new_liquidation_thresh_bps: Option<u16>,
    new_min_expiry_days: Option<u8>,
    new_max_expiry_days: Option<u8>,
    new_oracle_signer: Option<Pubkey>,
) -> Result<()> {
    let global_config = &mut ctx.accounts.global_config;
    let old = ConfigValues::of(global_config);

    // We use Option<> for the parameters so the admin can choose to update
    // only one or two fields without having to pass in the old values for the rest.

    if let Some(fee) = new_fee_stakers_bps {
        require!(fee <= MAX_FEE_RATE_STAKERS_BPS, ShortStakeError::InvalidFeeRate);
        global_config.fee_rate_stakers_bps = fee;
    }

    if let Some(fee) = new_fee_borrowers_bps {
        require!(fee <= MAX_FEE_RATE_BORROWERS_BPS, ShortStakeError::InvalidFeeRate);
        global_config.fee_rate_borrowers_bps = fee;
    }

    if let Some(threshold) = new_liquidation_thresh_bps {
        require!(
            (MIN_LIQUIDATION_THRESH_BPS..=MAX_LIQUIDATION_THRESH_BPS).contains(&threshold),
            ShortStakeError::InvalidLiquidationThreshold
        );
        global_config.liquidation_thresh_bps = threshold;
    }

    if let Some(min_days) = new_min_expiry_days {
        global_config.min_expiry_days = min_days;
    }

    if let Some(max_days) = new_max_expiry_days {
        global_config.max_expiry_days = max_days;
    }

    if let Some(signer) = new_oracle_signer {
        global_config.oracle_signer = signer;
    }
    // Checked on the result, so min and max can be moved together in one call.
    require_valid_expiry_bounds(global_config.min_expiry_days, global_config.max_expiry_days)?;

    // Old and new values, so a fee change or an oracle-signer rotation is visible on chain
    // without decoding instruction arguments (Slot Zero I-05).
    emit!(ConfigUpdatedEvent {
        authority: ctx.accounts.authority.key(),
        old,
        new: ConfigValues::of(global_config),
        timestamp: Clock::get()?.unix_timestamp,
    });
    msg!("Global config updated successfully by admin.");
    Ok(())
}

/// The settings update_config can change.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct ConfigValues {
    pub fee_rate_stakers_bps: u16,
    pub fee_rate_borrowers_bps: u16,
    pub liquidation_thresh_bps: u16,
    pub min_expiry_days: u8,
    pub max_expiry_days: u8,
    pub oracle_signer: Pubkey,
}

impl ConfigValues {
    fn of(config: &GlobalConfig) -> Self {
        Self {
            fee_rate_stakers_bps: config.fee_rate_stakers_bps,
            fee_rate_borrowers_bps: config.fee_rate_borrowers_bps,
            liquidation_thresh_bps: config.liquidation_thresh_bps,
            min_expiry_days: config.min_expiry_days,
            max_expiry_days: config.max_expiry_days,
            oracle_signer: config.oracle_signer,
        }
    }
}

#[event]
pub struct ConfigUpdatedEvent {
    pub authority: Pubkey,
    pub old: ConfigValues,
    pub new: ConfigValues,
    pub timestamp: i64,
}

#[event]
pub struct AuthorityProposedEvent {
    pub authority: Pubkey,
    pub proposed_authority: Pubkey,
    pub timestamp: i64,
}

#[event]
pub struct AuthorityAcceptedEvent {
    pub old_authority: Pubkey,
    pub new_authority: Pubkey,
    pub timestamp: i64,
}

/// Accounts context for updating the global configuration
/// Notice the has_one = authority inside the global_config account macro.
/// For this to work, the name of the Signer (authority) must exactly match the name of the field inside your GlobalConfig struct.
#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    // This MUST be a signer, meaning the wallet must approve the transaction
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"global_config"],
        bump = global_config.bump,
        has_one = authority, // SECURITY: Enforces global_config.authority == authority.key()
    )]
    pub global_config: Account<'info, GlobalConfig>,
}

//================================================================================================

/// Instruction 1: Propose New Authority
/// This instruction can only be called by the current admin. It does not change the active authority;
/// it merely nominates a new address.

pub fn propose_authority(ctx: Context<ProposeAuthority>, new_authority: Pubkey) -> Result<()> {
    let global_config = &mut ctx.accounts.global_config;

    // Store the nominated address in the pending field
    global_config.pending_authority = Some(new_authority);
    emit!(AuthorityProposedEvent {
        authority: ctx.accounts.authority.key(),
        proposed_authority: new_authority,
        timestamp: Clock::get()?.unix_timestamp,
    });

    msg!("New authority proposed: {}", new_authority);
    Ok(())
}

/// Instruction 2: Accept Authority
/// Completes a transfer started by propose_authority: only the proposed key can call it (for a
/// Squads multisig, its vault signs through a multisig transaction). To cancel a pending transfer,
/// the current authority proposes itself.
pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
    let global_config = &mut ctx.accounts.global_config;
    let new_authority = ctx.accounts.new_authority.key();
    require!(
        global_config.pending_authority == Some(new_authority),
        ShortStakeError::NotPendingAuthority
    );
    let old_authority = global_config.authority;
    global_config.authority = new_authority;
    global_config.pending_authority = None;
    emit!(AuthorityAcceptedEvent {
        old_authority,
        new_authority,
        timestamp: Clock::get()?.unix_timestamp,
    });

    msg!("Authority transferred to: {}", new_authority);
    Ok(())
}

#[derive(Accounts)]
pub struct AcceptAuthority<'info> {
    pub new_authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"global_config"],
        bump = global_config.bump,
    )]
    pub global_config: Account<'info, GlobalConfig>,
}

/// Accounts context for proposing a new authority
/// The current admin must sign the transaction.
/// The global_config account must have the current admin as its authority.
#[derive(Accounts)]
pub struct ProposeAuthority<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(
        mut,
        seeds = [b"global_config"],
        bump = global_config.bump,
        has_one = authority, // Enforces the current admin is signing
    )]
    pub global_config: Account<'info, GlobalConfig>,
}
