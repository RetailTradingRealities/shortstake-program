use anchor_lang::prelude::*;
use crate::pyth_feeds_list::OracleFlag;

// The oracle mode a new market_vault records. Every market now uses the signed oracle
// (2026-11-21, see get_current_price.rs); the Pyth allow-list is no longer consulted.
pub fn is_pyth_supported(_token_mint: Pubkey) -> OracleFlag {
    OracleFlag::Custom
}
