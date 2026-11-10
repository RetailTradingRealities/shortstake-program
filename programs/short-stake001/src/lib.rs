pub mod constants;
pub mod error;
pub mod get_current_price;
pub mod get_pyth_price_in_usdc;
pub mod instructions;
pub mod math;
pub mod pyth_feeds_list;
pub mod pyth_price_update;
pub mod set_oracle_flag;
pub mod signed_price;

use anchor_lang::prelude::*;

pub use constants::*;
pub use get_current_price::get_current_price;
pub use get_pyth_price_in_usdc::get_pyth_price_in_usdc;
pub use pyth_price_update::PriceUpdateV2;
pub use set_oracle_flag::is_pyth_supported;
#[allow(ambiguous_glob_reexports)]
pub use instructions::*;
pub use error::*;
//use crate::ShortStakeError;

// Program ID per build, like the USDC mint in constants.rs: `anchor build -- --features devnet`
// keeps the devnet deployment's ID; the default (mainnet) build uses the mainnet vanity ID
// (keypair keys/sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3.json, gitignored).
#[cfg(feature = "devnet")]
declare_id!("G6zjYNx2LurrRnYWJZmvScUYQ1oavBfRkGWoDrcCWWVQ");
#[cfg(not(feature = "devnet"))]
declare_id!("sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3");

// Security contact embedded in the program binary (shown by explorers such as Solscan), so
// whoever finds a bug can reach us. Mirrors app/frontend/public/.well-known/security.txt.
#[cfg(not(feature = "no-entrypoint"))]
solana_security_txt::security_txt! {
    name: "ShortStake",
    project_url: "https://shortstake.io",
    contacts: "email:security@shortstake.io,link:https://shortstake.io/security",
    policy: "https://shortstake.io/security",
    preferred_languages: "en"
}

#[program]
pub mod short_stake001 {
    use super::*;

    pub fn initialize(
        ctx: Context<InitializeConfig>,
        fee_rate_stakers_bps: u16,
        fee_rate_borrowers_bps: u16,
        liquidation_thresh_bps: u16,
        min_expiry_days: u8,
        max_expiry_days: u8,
        oracle_signer: Pubkey,
    ) -> Result<()> {
        crate::instructions::global_configuration::initialize_config(
            ctx,
            fee_rate_stakers_bps,
            fee_rate_borrowers_bps,
            liquidation_thresh_bps,
            min_expiry_days,
            max_expiry_days,
            oracle_signer,
        )
    }



    pub fn update_config(
        ctx: Context<UpdateConfig>,
        new_fee_stakers_bps: Option<u16>,
        new_fee_borrowers_bps: Option<u16>,
        new_liquidation_thresh_bps: Option<u16>,
        new_min_expiry_days: Option<u8>,
        new_max_expiry_days: Option<u8>,
        new_oracle_signer: Option<Pubkey>,
    ) -> Result<()> {
        crate::instructions::global_configuration::update_config(
            ctx,
            new_fee_stakers_bps,
            new_fee_borrowers_bps,
            new_liquidation_thresh_bps,
            new_min_expiry_days,
            new_max_expiry_days,
            new_oracle_signer,
        )
    }

    pub fn propose_authority(ctx: Context<ProposeAuthority>, new_authority: Pubkey) -> Result<()> {
        crate::instructions::global_configuration::propose_authority(ctx, new_authority)
    }

    pub fn accept_authority(ctx: Context<AcceptAuthority>) -> Result<()> {
        crate::instructions::global_configuration::accept_authority(ctx)
    }

    pub fn set_paused(ctx: Context<SetPaused>, paused: bool) -> Result<()> {
        crate::instructions::protocol_state::set_paused(ctx, paused)
    }

    pub fn initialize_global_vault(ctx: Context<InitializeGlobalVault>) -> Result<()> {
        crate::instructions::initialize_global_vault::initialize_global_vault(ctx)
    }

    pub fn deposit_stake(
        ctx: Context<DepositStake>,
        amount_to_stake: u64,
        interest_rate_bps: u16,
        max_expiry_days: u8,
        offer_id: u64,
    ) -> Result<()> {
        crate::instructions::deposit_stake::deposit_stake(
            ctx,
            amount_to_stake,
            interest_rate_bps,
            max_expiry_days,
            offer_id,
        )
    }

    pub fn early_unstake(ctx: Context<EarlyUnstake>) -> Result<()> {
        crate::instructions::early_unstake::early_unstake(ctx)
    }

    pub fn open_borrow_position(
        ctx: Context<OpenBorrowPosition>,
        borrow_amount: u64,
        position_id: u64,
        provided_collateral_usdc: u64,
        max_interest_rate_bps: u16,
        min_expiry_days: u8,
    ) -> Result<()> {
        crate::instructions::open_borrow_position::open_borrow_position(
            ctx,
            borrow_amount,
            position_id,
            provided_collateral_usdc,
            max_interest_rate_bps,
            min_expiry_days,
        )
    }

    pub fn close_position(ctx: Context<ClosePosition>) -> Result<()> {
        crate::instructions::close_position::close_position(ctx)
    }

    pub fn liquidate_position(ctx: Context<LiquidatePosition>) -> Result<()> {
        crate::instructions::liquidate_position::liquidate_position(ctx)
    }

    // pub fn refuel_backend_wallet(ctx: Context<RefuelCrank>, amount: u64) -> Result<()> {
    //     crate::instructions::protocol_staking::refuel_backend_wallet(ctx, amount)
    // }
    
}
