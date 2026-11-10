use anchor_lang::prelude::*;
use crate::pyth_feeds_list::OracleFlag;
use crate::pyth_feeds_list::PYTH_ALLOWED_FEEDS;
use crate::{get_pyth_price_in_usdc, ShortStakeError };
use crate::signed_price::{verify_signed_price, PriceAction};
use crate::pyth_price_update::PriceUpdateV2;
//This function retrieves the current price of a given token, 
//either from a Pyth price feed or an oracle_signer-signed custom price (see signed_price.rs).
//Both paths return 10^-12 USD per whole token (constants::PRICE_DECIMALS).

//OracleFlag imported at the top of this file from pyth_feeds_list.rs


pub fn get_current_price<'info>(
    token_mint: Pubkey,
    oracle_flag: &OracleFlag,
    pyth_price_feed: &Option<Box<Account<'info, PriceUpdateV2>>>,
    instructions_sysvar: &AccountInfo<'info>,
    oracle_signer: Pubkey,
    action: PriceAction,
) -> Result<u64> {
    // Route using the mode stored on market_vault, set once in deposit_stake
    match oracle_flag {
        OracleFlag::Pyth => {
            let (feed_id, price_account) = PYTH_ALLOWED_FEEDS
                .iter()
                .find(|(mint, _, _)| *mint == token_mint)
                .map(|(_, feed_id, price_account)| (*feed_id, *price_account))
                .ok_or(ShortStakeError::InvalidPythFeed)?;

            // Unpack the optional Pyth feed
            let pyth_acc = pyth_price_feed
                .as_ref()
                .ok_or(ShortStakeError::MissingPythFeed)?;

            // Only Pyth's sponsored push-feed account for this token. Anyone can post a verified
            // Pyth update to an account of their own, so without this a caller could pick
            // whichever recent price suits them. (Ownership by the Pyth Receiver is enforced by
            // the Account<PriceUpdateV2> type; the feed ID inside is checked when reading.)
            require_keys_eq!(
                pyth_acc.key(),
                price_account,
                ShortStakeError::InvalidPythFeed
            );

            // Call your existing Pyth function
            get_pyth_price_in_usdc(
                &OracleFlag::Pyth,
                pyth_acc,
                &feed_id,
            )
        }
        OracleFlag::Custom => {
            // Price comes from the Ed25519 instruction placed just before this one
            verify_signed_price(instructions_sysvar, oracle_signer, token_mint, action)
        }
    }
}

