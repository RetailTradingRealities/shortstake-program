use anchor_lang::prelude::*;

#[constant]
pub const COUNTER_SEED: &[u8] = b"counter";

#[constant]
pub const HELLO_WORLD_LAMPORTS: u64 = 1;

#[constant]
pub const MAX_COUNT: u64 = 10;

// Every oracle price (Pyth-normalized or oracle-signed) is an integer count of 10^-12 USD per
// whole token ($1.00 = 1_000_000_000_000), so sub-cent tokens keep enough significant digits:
// $0.00000001 is still 10_000 units. u64 tops out at ~$18.4M per token.
pub const PRICE_DECIMALS: u32 = 12;
pub const USDC_DECIMALS: u32 = 6;
// Divide (token amount in whole tokens * price) by this to get micro-USDC.
pub const PRICE_TO_USDC_DIVISOR: u128 = 10u128.pow(PRICE_DECIMALS - USDC_DECIMALS);

// The wallet allowed to call `initialize` and `initialize_global_vault`. After initialize, the
// config authority moves to the Config Squad (propose_authority / accept_authority), so this key
// only matters for the one-off setup.
// Devnet: the throwaway devnet-keypair.json (committed; the Rust tests sign with it).
#[cfg(feature = "devnet")]
pub const ADMIN_PUBKEY: Pubkey =
    pubkey!("J9oL7WZJehtHpokpH63nGJKhxCdAv5iyhZ9rAsdquncn");
// Mainnet: a fresh key whose keypair was never committed (keys/mainnet-admin.json, gitignored).
#[cfg(not(feature = "devnet"))]
pub const ADMIN_PUBKEY: Pubkey =
    pubkey!("HTyjZjAXej5vmWsMDQGGUXrDjgHseoTi1Vt3XVpx2XYe");

// Owner of the USDC account that receives protocol fees.
// Devnet: throwaway wallet, keypair at devnet-fee-collector-keypair.json (committed).
#[cfg(feature = "devnet")]
pub const FEE_COLLECTOR_PUBKEY: Pubkey =
    pubkey!("5THkMgTwPdkHoJqp82WMoSf8Wwcb2azqMh26ezwRwGJb");
// Mainnet: the Config Squad's vault (2-of-3 multisig, docs/incident-response.md section 2).
// Its USDC ATA must exist before the first open.
#[cfg(not(feature = "devnet"))]
pub const FEE_COLLECTOR_PUBKEY: Pubkey =
    pubkey!("52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr");

// Hard caps on the admin-set fee rates, so a compromised/mistyped admin call can't
// confiscate staker interest or overcharge borrowers. Production targets: 500 / 25.
/// Smallest position that can be opened: $10 of borrowed tokens at the entry price (micro-USDC).
/// Keeps every interest and fee amount well above rounding granularity.
pub const MIN_POSITION_NOTIONAL_USDC: u64 = 10_000_000;

pub const MAX_FEE_RATE_STAKERS_BPS: u16 = 2_000; // 20% of staker interest
pub const MAX_FEE_RATE_BORROWERS_BPS: u16 = 100; // 1% of borrowed notional

// Bounds on the admin-set liquidation threshold (equity <= notional * thresh liquidates).
// Below the floor the buffer before bad-debt write-off gets too thin for volatile tokens; the
// ceiling stays under the 150% initial margin so a new position can't open already liquidatable.
/// Most decimals a market token may have. The borrowing math overflows at 19 (open reverts
/// above ~$142) and token_divisor at 20+ (Slot Zero I-09); real SPL tokens use 0-9.
pub const MAX_MARKET_TOKEN_DECIMALS: u8 = 12;

pub const MIN_LIQUIDATION_THRESH_BPS: u16 = 10_500;
pub const MAX_LIQUIDATION_THRESH_BPS: u16 = 13_000;

pub mod usdc_mint {
    use super::*;
    pub const ID: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
}

// Define the official Solana Mainnet USDC Mint Address
pub mod usdc_mainnet {
    use super::*;
    pub const ID: Pubkey = pubkey!("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
}

// Define the official Solana Devnet USDC Mint Address (provided by Circle)
pub mod usdc_devnet {
    use super::*;
    pub const ID: Pubkey = pubkey!("4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU");
}

// Cluster-selected USDC mint used by every instruction. Default build = mainnet;
// build with `anchor build -- --features devnet` to target Circle's devnet USDC.
pub mod usdc {
    #[cfg(not(feature = "devnet"))]
    pub use super::usdc_mainnet::ID;
    #[cfg(feature = "devnet")]
    pub use super::usdc_devnet::ID;
}
