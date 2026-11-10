use anchor_lang::prelude::*;

#[error_code]
pub enum ShortStakeError {
    #[msg("Stake amount must be greater than zero.")]
    InvalidStakeAmount,
    #[msg("Borrow amount must be greater than zero.")]
    InvalidBorrowAmount,
    #[msg("Collateral does not satisfy the required initial margin.")]
    InsufficientCollateral,
    #[msg("The lender offer does not have enough tokens available to borrow.")]
    InsufficientAvailableLiquidity,
    #[msg("Mathematical overflow occurred.")]
    MathOverflow,
    #[msg("max_expiry_days is outside the global config's allowed range.")]
    InvalidExpiryDays,
    // No longer returned (early_unstake now withdraws the unborrowed part); kept so the codes
    // after it don't shift.
    #[msg("The lender offer has active borrows and cannot be unstaked.")]
    OfferHasActiveBorrows,
    #[msg("The collateral cannot cover the accrued interest.")]
    BadDebtDetected,
    #[msg("The position is not eligible for liquidation.")]
    PositionHealthy,
    #[msg("A Pyth price feed is required for this token.")]
    MissingPythFeed,
    #[msg("The supplied Pyth price feed is not approved for this token.")]
    InvalidPythFeed,
    #[msg("A signed custom oracle price (Ed25519 instruction) must directly precede this instruction.")]
    MissingCustomOracle,
    #[msg("The requested oracle type is not implemented.")]
    OracleNotYetImplemented,
    #[msg("The Pyth price account is invalid.")]
    InvalidPriceAccount,
    #[msg("The oracle price is invalid.")]
    InvalidPrice,
    #[msg("The oracle price is stale.")]
    StaleOraclePrice,
    #[msg("The signed custom oracle price is malformed, for another mint or action, or not signed by the oracle signer.")]
    InvalidSignedPrice,
    #[msg("Fee rate exceeds the protocol's maximum allowed fee.")]
    InvalidFeeRate,
    #[msg("This lender offer is closed to new borrowing.")]
    OfferClosed,
    // Unused since the Slot Zero H-01 fix (payouts accept any account the owner holds); kept so
    // the error codes after it don't shift.
    #[msg("Token account must be the owner's associated token account.")]
    NotAssociatedTokenAccount,
    #[msg("Nothing to withdraw: every token in this offer is lent out. Try again after a loan closes.")]
    NothingToWithdraw,
    #[msg("Liquidation threshold is outside the protocol's allowed range.")]
    InvalidLiquidationThreshold,
    #[msg("Position is below the minimum size of $10 borrowed value.")]
    PositionTooSmall,
    #[msg("Only the proposed new authority can accept the authority transfer.")]
    NotPendingAuthority,
    #[msg("New stakes and borrows are paused. Closing, unstaking and liquidations still work.")]
    ProtocolPaused,
    #[msg("This token can't form a market (USDC itself, a freezable mint, or more decimals than the borrowing math supports).")]
    UnsupportedMarketToken,
    #[msg("The offer's terms changed after you chose it (higher rate or shorter expiry). Please review the offer and try again.")]
    OfferTermsChanged,
    #[msg("Expiry bounds must satisfy 1 <= min_expiry_days <= max_expiry_days.")]
    InvalidExpiryBounds,
    #[msg("Pyth price confidence interval is too wide to use.")]
    PythConfidenceTooWide,
    #[msg("A write-off that returns unborrowed tokens needs a token account the staker owns for this mint.")]
    MissingStakerTokenAccount,
    /* 
    #[msg("Only the designated backend wallet can call this crank instruction.")]
    UnauthorizedCrank,
    #[msg("Requested refuel amount exceeds the safety cap.")]
    ExceedsRefuelCap,
*/

}
