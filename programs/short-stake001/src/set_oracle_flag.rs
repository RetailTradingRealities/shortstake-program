use anchor_lang::prelude::*;
use crate::pyth_feeds_list::{OracleFlag, PYTH_ALLOWED_FEEDS};

// Determines which oracle a market_vault should use, based on the allowed Pyth feed list.
pub fn is_pyth_supported(token_mint: Pubkey) -> OracleFlag {
    for (mint, _feed_id, _price_account) in PYTH_ALLOWED_FEEDS.iter() {
        if *mint == token_mint {
            return OracleFlag::Pyth;
        }
    }

    OracleFlag::Custom
}
