use anchor_lang::prelude::*;
use crate::pyth_feeds_list::OracleFlag;
use crate::signed_price::{verify_signed_price, PriceAction};
use crate::pyth_price_update::PriceUpdateV2;
// Returns the current price of a token in 10^-12 USD per whole token (constants::PRICE_DECIMALS),
// from an oracle_signer-signed price (see signed_price.rs).
//
// Every market is priced by the signed oracle, whatever market_vault.expected_oracle_feed says
// (2026-11-21). Pyth stopped pushing its sponsored meme-coin feeds (WIF, BONK, ...) around
// 2026-11-17, and a Pyth market can't be closed or liquidated on a frozen price. The stored flag
// and the optional pyth_price_feed account are kept so account layouts and clients don't change;
// both are ignored. The Pyth version is preserved at git tag v1-pyth.
pub fn get_current_price<'info>(
    token_mint: Pubkey,
    _oracle_flag: &OracleFlag,
    _pyth_price_feed: &Option<Box<Account<'info, PriceUpdateV2>>>,
    instructions_sysvar: &AccountInfo<'info>,
    oracle_signer: Pubkey,
    action: PriceAction,
) -> Result<u64> {
    // Price comes from the Ed25519 instruction placed just before this one
    verify_signed_price(instructions_sysvar, oracle_signer, token_mint, action)
}
