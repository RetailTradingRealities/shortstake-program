//! Shared money math. Rounding rule: amounts users owe the protocol or stakers (interest, fees,
//! required margin, maintenance margin) round UP; amounts the protocol pays out round DOWN - so
//! no rounding remainder can be farmed with many tiny positions.
//! Mirrored in app/src/instructions/liquidationPlan.ts (the keeper) - keep both in step.

use anchor_lang::prelude::*;
use crate::ShortStakeError;

/// 10^decimals - the base units in one whole token.
pub fn token_divisor(decimals: u8) -> Result<u64> {
    10_u64
        .checked_pow(decimals as u32)
        .ok_or_else(|| error!(ShortStakeError::MathOverflow))
}

/// ceil(n / d) for a non-zero constant or validated divisor.
pub fn div_ceil(n: u128, d: u128) -> Result<u128> {
    require!(d > 0, ShortStakeError::MathOverflow);
    Ok(n.div_ceil(d))
}

/// `value * bps / 10_000`, rounded up.
pub fn bps_of_ceil(value: u64, bps: u16) -> Result<u64> {
    let product = (value as u128)
        .checked_mul(bps as u128)
        .ok_or(ShortStakeError::MathOverflow)?;
    div_ceil(product, 10_000)?
        .try_into()
        .map_err(|_| error!(ShortStakeError::MathOverflow))
}

/// Hours billed for a loan running `seconds_elapsed`: every started hour, at least one.
pub fn billable_hours(seconds_elapsed: i64) -> Result<u64> {
    require!(seconds_elapsed >= 0, ShortStakeError::MathOverflow);
    let hours = div_ceil(seconds_elapsed as u128, 3_600)?.max(1);
    hours.try_into().map_err(|_| error!(ShortStakeError::MathOverflow))
}

/// Interest owed on `notional_usdc` at `rate_bps` per day for `hours`:
/// notional * rate_bps * hours / (10_000 * 24), one division, rounded up.
pub fn accrued_interest_usdc(notional_usdc: u64, rate_bps: u16, hours: u64) -> Result<u64> {
    let product = (notional_usdc as u128)
        .checked_mul(rate_bps as u128)
        .ok_or(ShortStakeError::MathOverflow)?
        .checked_mul(hours as u128)
        .ok_or(ShortStakeError::MathOverflow)?;
    div_ceil(product, 240_000)?
        .try_into()
        .map_err(|_| error!(ShortStakeError::MathOverflow))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interest_rounds_up_with_one_division() {
        // 1 micro-USDC at 1 bps for 1 hour is 1/240_000 of a micro-USDC: owed as 1, not 0.
        assert_eq!(accrued_interest_usdc(1, 1, 1).unwrap(), 1);
        // Exact amounts are unchanged: $10 at 1200 bps/day for 24 h = $1.20.
        assert_eq!(accrued_interest_usdc(10_000_000, 1_200, 24).unwrap(), 1_200_000);
        // One division: 7 * 10 * 1 / 240_000 -> 1 (two floor divisions gave 0).
        assert_eq!(accrued_interest_usdc(7, 10, 1).unwrap(), 1);
        assert_eq!(accrued_interest_usdc(0, 1_200, 5).unwrap(), 0);
    }

    #[test]
    fn hours_and_bps() {
        assert_eq!(billable_hours(0).unwrap(), 1);
        assert_eq!(billable_hours(1).unwrap(), 1);
        assert_eq!(billable_hours(3_600).unwrap(), 1);
        assert_eq!(billable_hours(3_601).unwrap(), 2);
        assert!(billable_hours(-1).is_err());
        assert_eq!(bps_of_ceil(1, 25).unwrap(), 1);
        assert_eq!(bps_of_ceil(10_000_000, 25).unwrap(), 25_000);
        assert_eq!(bps_of_ceil(10_000_001, 15_000).unwrap(), 15_000_002);
    }

    #[test]
    fn token_divisor_is_checked() {
        assert_eq!(token_divisor(6).unwrap(), 1_000_000);
        assert_eq!(token_divisor(19).unwrap(), 10_000_000_000_000_000_000);
        assert!(token_divisor(20).is_err());
    }
}
