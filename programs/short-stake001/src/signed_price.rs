use anchor_lang::prelude::*;
use solana_instructions_sysvar::{load_current_index_checked, load_instruction_at_checked};
use crate::ShortStakeError;

// Custom-oracle prices are not stored on-chain. The protocol backend signs
// (program_id, token_mint, price, publish_time, action) off-chain with global_config.oracle_signer,
// and the client places an Ed25519 signature-verify instruction carrying that message
// IMMEDIATELY BEFORE the instruction that consumes the price. The Ed25519 precompile
// rejects the whole transaction if the signature is invalid; this module then checks that
// the verified message is ours, for the right mint and action, signed by the right key, and fresh.

pub const ED25519_PROGRAM_ID: Pubkey = pubkey!("Ed25519SigVerify111111111111111111111111111");
pub const INSTRUCTIONS_SYSVAR_ID: Pubkey = pubkey!("Sysvar1nstructions1111111111111111111111111");

pub const MAX_SIGNED_PRICE_AGE_SECONDS: i64 = 60;
// A liquidate price names no position, so while it is valid a liquidator can hold the highest
// one of a falling market and use it after the drop (KupiaSec, 2026-11-23). Keep that window short.
pub const MAX_LIQUIDATION_PRICE_AGE_SECONDS: i64 = 15;
// Tolerates the backend's wall clock running slightly ahead of the cluster's unix_timestamp.
pub const MAX_SIGNED_PRICE_FUTURE_SKEW_SECONDS: i64 = 10;

// program_id (32) || token_mint (32) || price u64 LE (8) || publish_time i64 LE (8) || action (1)
pub const SIGNED_PRICE_MESSAGE_LEN: usize = 32 + 32 + 8 + 8 + 1;

/// Which instruction a signed price was issued for. The backend applies different sourcing
/// rules per action (e.g. how many data feeds must agree), so a price signed for one action
/// must not be replayable in another. Mirrored in app/src/oracle/signedPrice.ts.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PriceAction {
    DepositStake = 1,
    OpenBorrowPosition = 2,
    ClosePosition = 3,
    LiquidatePosition = 4,
}

impl PriceAction {
    /// How old (in seconds, by the cluster clock) a price signed for this action may be.
    pub const fn max_age_seconds(self) -> i64 {
        match self {
            PriceAction::LiquidatePosition => MAX_LIQUIDATION_PRICE_AGE_SECONDS,
            _ => MAX_SIGNED_PRICE_AGE_SECONDS,
        }
    }
}

// Ed25519 precompile instruction layout: [num_signatures u8, padding u8, offsets (14 bytes) ...]
const ED25519_OFFSETS_START: usize = 2;
const ED25519_OFFSETS_LEN: usize = 14;
const ED25519_PUBKEY_LEN: usize = 32;
// Instruction index meaning "data lives in this same Ed25519 instruction".
const ED25519_CURRENT_IX: u16 = u16::MAX;

/// Returns the oracle-signed price (10^-12 USD per whole token, see PRICE_DECIMALS) for
/// `token_mint`, provided it was signed for `action`.
pub fn verify_signed_price(
    instructions_sysvar: &AccountInfo,
    oracle_signer: Pubkey,
    token_mint: Pubkey,
    action: PriceAction,
) -> Result<u64> {
    require_keys_eq!(
        instructions_sysvar.key(),
        INSTRUCTIONS_SYSVAR_ID,
        ShortStakeError::InvalidSignedPrice
    );

    // The signed price must sit directly before this instruction - a fixed position means
    // an attacker can't point us at some other Ed25519 instruction elsewhere in the tx.
    let current_index = load_current_index_checked(instructions_sysvar)?;
    let ed25519_index = current_index
        .checked_sub(1)
        .ok_or(ShortStakeError::MissingCustomOracle)?;
    let ed25519_ix = load_instruction_at_checked(ed25519_index as usize, instructions_sysvar)?;
    require!(
        ed25519_ix.program_id.to_bytes() == ED25519_PROGRAM_ID.to_bytes(),
        ShortStakeError::MissingCustomOracle
    );

    let data = &ed25519_ix.data;
    require!(
        data.len() >= ED25519_OFFSETS_START + ED25519_OFFSETS_LEN && data[0] == 1,
        ShortStakeError::InvalidSignedPrice
    );

    let read_u16 = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]);
    // The precompile itself bounds-checks signature_offset, so it isn't needed here.
    let o = ED25519_OFFSETS_START;
    let signature_ix_index = read_u16(o + 2);
    let public_key_offset = read_u16(o + 4) as usize;
    let public_key_ix_index = read_u16(o + 6);
    let message_offset = read_u16(o + 8) as usize;
    let message_size = read_u16(o + 10) as usize;
    let message_ix_index = read_u16(o + 12);

    // All three must reference this Ed25519 instruction's own data, otherwise the pubkey or
    // message the precompile verified could differ from the bytes we read below.
    require!(
        signature_ix_index == ED25519_CURRENT_IX
            && public_key_ix_index == ED25519_CURRENT_IX
            && message_ix_index == ED25519_CURRENT_IX,
        ShortStakeError::InvalidSignedPrice
    );
    require!(
        message_size == SIGNED_PRICE_MESSAGE_LEN,
        ShortStakeError::InvalidSignedPrice
    );

    let signer_bytes = data
        .get(public_key_offset..public_key_offset + ED25519_PUBKEY_LEN)
        .ok_or(ShortStakeError::InvalidSignedPrice)?;
    require!(
        signer_bytes == oracle_signer.as_ref(),
        ShortStakeError::InvalidSignedPrice
    );

    let message = data
        .get(message_offset..message_offset + SIGNED_PRICE_MESSAGE_LEN)
        .ok_or(ShortStakeError::InvalidSignedPrice)?;
    require!(
        &message[0..32] == crate::ID.as_ref() && &message[32..64] == token_mint.as_ref(),
        ShortStakeError::InvalidSignedPrice
    );

    let price = u64::from_le_bytes(
        message[64..72]
            .try_into()
            .map_err(|_| ShortStakeError::InvalidSignedPrice)?,
    );
    let publish_time = i64::from_le_bytes(
        message[72..80]
            .try_into()
            .map_err(|_| ShortStakeError::InvalidSignedPrice)?,
    );
    require!(message[80] == action as u8, ShortStakeError::InvalidSignedPrice);
    require!(price > 0, ShortStakeError::InvalidPrice);

    let now = Clock::get()?.unix_timestamp;
    let age = now
        .checked_sub(publish_time)
        .ok_or(ShortStakeError::StaleOraclePrice)?;
    require!(
        (-MAX_SIGNED_PRICE_FUTURE_SKEW_SECONDS..=action.max_age_seconds()).contains(&age),
        ShortStakeError::StaleOraclePrice
    );

    Ok(price)
}
