use anchor_lang::prelude::*;
use crate::pyth_feeds_list::OracleFlag;
use crate::constants::PRICE_DECIMALS;
use crate::ShortStakeError;
use crate::pyth_price_update::{
    PriceUpdateV2,
};

// Pyth's sponsored push feeds update at least every 55s (or on a 0.5% move), so a 60s limit
// left as little as 5s for a transaction to land in a quiet market.
// https://docs.pyth.network/price-feeds/core/push-feeds/solana
pub const MAXIMUM_PRICE_AGE_SECONDS: u64 = 90;

/// Widest Pyth confidence interval accepted, as a share of the price (500 = 5%).
pub const MAX_PYTH_CONF_BPS: u128 = 500;

/// Returns the Pyth price normalized to PRICE_DECIMALS (10^-12 USD per whole token).
pub fn get_pyth_price_in_usdc(
    oracle_flag: &OracleFlag,
    price_update: &Account<PriceUpdateV2>,
    feed_id: &[u8; 32],
) -> Result<u64> {
    require!(
        *oracle_flag == OracleFlag::Pyth,
        ShortStakeError::OracleNotYetImplemented
    );

    let price = price_update
        .get_price_no_older_than(&Clock::get()?, MAXIMUM_PRICE_AGE_SECONDS, feed_id)
        .map_err(|_| ShortStakeError::InvalidPrice)?;

    require!(price.price > 0, ShortStakeError::InvalidPrice);
    require!(price.exponent <= 0, ShortStakeError::InvalidPrice);
    // Refuse a price whose confidence band is too wide to trust (Slot Zero I-06). conf shares
    // price's exponent, so the ratio needs no scaling.
    require!(
        (price.conf as u128) * 10_000 <= (price.price as u128) * MAX_PYTH_CONF_BPS,
        ShortStakeError::PythConfidenceTooWide
    );

    let raw_price = price.price as u64;
    let pyth_decimals = price.exponent.unsigned_abs();

    let normalized_price = if pyth_decimals > PRICE_DECIMALS {
        let diff = pyth_decimals - PRICE_DECIMALS;
        raw_price
            .checked_div(10_u64.checked_pow(diff).ok_or(ShortStakeError::MathOverflow)?)
            .ok_or(ShortStakeError::MathOverflow)?
    } else {
        let diff = PRICE_DECIMALS - pyth_decimals;
        raw_price
            .checked_mul(10_u64.checked_pow(diff).ok_or(ShortStakeError::MathOverflow)?)
            .ok_or(ShortStakeError::MathOverflow)?
    };

    // A tiny price with more than PRICE_DECIMALS decimals can round to 0 - the signed-price path
    // refuses 0 too, and a 0 price would make a position's notional (and required margin) 0.
    require!(normalized_price > 0, ShortStakeError::InvalidPrice);

    Ok(normalized_price)
}
