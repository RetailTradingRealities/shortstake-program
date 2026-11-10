use anchor_lang::prelude::*;

// Mainnet/devnet address of the Pyth Solana Receiver program, per
// https://docs.pyth.network/price-feeds/core/contract-addresses/solana
pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey = pubkey!("rec5EKMGg6MxZYaMdyBfgwp4d5rB9T1VQH5pJv5LtFJ");

// sha256("account:PriceUpdateV2")[..8], Anchor's standard account discriminator.
const PRICE_UPDATE_V2_DISCRIMINATOR: [u8; 8] = [34, 241, 35, 99, 157, 126, 244, 205];

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub enum VerificationLevel {
    Partial { num_signatures: u8 },
    Full,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct PriceFeedMessage {
    pub feed_id: [u8; 32],
    pub price: i64,
    pub conf: u64,
    pub exponent: i32,
    pub publish_time: i64,
    pub prev_publish_time: i64,
    pub ema_price: i64,
    pub ema_conf: u64,
}

pub struct Price {
    pub price: i64,
    pub conf: u64,
    pub exponent: i32,
    pub publish_time: i64,
}

// Mirrors the on-chain account layout written by the Pyth Receiver program's
// own `PriceUpdateV2` (same field order/types), so it deserializes real posted
// price accounts without depending on the pyth-solana-receiver-sdk crate.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Debug)]
pub struct PriceUpdateV2 {
    pub write_authority: Pubkey,
    pub verification_level: VerificationLevel,
    pub price_message: PriceFeedMessage,
    pub posted_slot: u64,
}

/// How far a Pyth publish_time may be ahead of the cluster clock.
pub const MAX_FUTURE_SKEW_SECONDS: i64 = 10;

impl PriceUpdateV2 {
    pub fn get_price_no_older_than(
        &self,
        clock: &Clock,
        maximum_age: u64,
        feed_id: &[u8; 32],
    ) -> std::result::Result<Price, ()> {
        if self.verification_level != VerificationLevel::Full {
            return Err(());
        }
        if self.price_message.feed_id != *feed_id {
            return Err(());
        }

        // A publish_time slightly AHEAD of the cluster clock is normal (Pyth time vs the bank
        // clock); refusing any future skew could stall every Pyth market (Slot Zero I-02).
        // Same tolerance as the signed-price path.
        let age = clock.unix_timestamp - self.price_message.publish_time;
        if age < -MAX_FUTURE_SKEW_SECONDS || (age > 0 && age as u64 > maximum_age) {
            return Err(());
        }

        Ok(Price {
            price: self.price_message.price,
            conf: self.price_message.conf,
            exponent: self.price_message.exponent,
            publish_time: self.price_message.publish_time,
        })
    }
}

impl anchor_lang::Discriminator for PriceUpdateV2 {
    const DISCRIMINATOR: &'static [u8] = &PRICE_UPDATE_V2_DISCRIMINATOR;
}

impl anchor_lang::Owner for PriceUpdateV2 {
    fn owner() -> Pubkey {
        PYTH_RECEIVER_PROGRAM_ID
    }
}

impl anchor_lang::AccountSerialize for PriceUpdateV2 {
    fn try_serialize<W: std::io::Write>(&self, writer: &mut W) -> Result<()> {
        writer
            .write_all(&PRICE_UPDATE_V2_DISCRIMINATOR)
            .map_err(|_| error!(anchor_lang::error::ErrorCode::AccountDidNotSerialize))?;
        AnchorSerialize::serialize(self, writer)
            .map_err(|_| error!(anchor_lang::error::ErrorCode::AccountDidNotSerialize))?;
        Ok(())
    }
}

impl anchor_lang::AccountDeserialize for PriceUpdateV2 {
    fn try_deserialize(buf: &mut &[u8]) -> Result<Self> {
        if buf.len() < 8 {
            return Err(anchor_lang::error::ErrorCode::AccountDiscriminatorNotFound.into());
        }
        if buf[..8] != PRICE_UPDATE_V2_DISCRIMINATOR {
            return Err(anchor_lang::error::ErrorCode::AccountDiscriminatorMismatch.into());
        }
        Self::try_deserialize_unchecked(buf)
    }

    fn try_deserialize_unchecked(buf: &mut &[u8]) -> Result<Self> {
        let mut data = &buf[8..];
        AnchorDeserialize::deserialize(&mut data)
            .map_err(|_| error!(anchor_lang::error::ErrorCode::AccountDidNotDeserialize))
    }
}
