#![allow(dead_code)]

//! Shared litesvm test harness.
//!
//! `anchor-lang` pins `solana-pubkey` v3 (aliased here as [`AnchorPubkey`]) while `litesvm`
//! and its transaction/keypair stack use `solana-pubkey` v4 (aka `solana-address`, aliased
//! here as [`SvmPubkey`]). These are distinct Rust types with identical 32-byte layout, so
//! [`to_svm`]/[`to_anchor`] bridge them by round-tripping through raw bytes.
//! `spl-token-interface` resolves the same v3 pubkey as anchor, so mint/token-account state
//! can be built directly with anchor pubkeys.

use anchor_lang::{prelude::Pubkey as AnchorPubkey, Discriminator, InstructionData};
use litesvm::LiteSVM;
use short_stake001::signed_price::{PriceAction, INSTRUCTIONS_SYSVAR_ID};
use std::collections::HashMap;
use solana_account::Account as SvmAccount;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_program_option::COption;
use solana_program_pack::Pack;
use solana_pubkey::Pubkey as SvmPubkey;
pub use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_associated_token_account_interface::address::get_associated_token_address;
use spl_token_interface::state::{Account as SplTokenAccount, AccountState, Mint as SplMint};

pub mod market;

pub const TOKEN_PROGRAM_ID: AnchorPubkey = spl_token_interface::ID;

pub fn to_svm(pk: AnchorPubkey) -> SvmPubkey {
    SvmPubkey::from(pk.to_bytes())
}

pub fn to_anchor(pk: SvmPubkey) -> AnchorPubkey {
    AnchorPubkey::from(pk.to_bytes())
}

pub fn program_id() -> AnchorPubkey {
    short_stake001::ID
}

/// AccountMeta placeholder anchor-lang expects for an absent `Option<Account<..>>`.
pub fn none_account_meta() -> AccountMeta {
    AccountMeta {
        pubkey: to_svm(program_id()),
        is_signer: false,
        is_writable: false,
    }
}

/// Fills an instruction's absent `pyth_price_feed` slot (the `none_account_meta()` placeholder
/// the ix_* builders pass) with `feed`, for Pyth-priced markets.
pub fn with_pyth_feed(mut ix: Instruction, feed: AnchorPubkey) -> Instruction {
    let placeholder = none_account_meta();
    let slot = ix
        .accounts
        .iter_mut()
        .find(|m| m.pubkey == placeholder.pubkey && !m.is_writable && !m.is_signer)
        .expect("instruction has no pyth_price_feed placeholder");
    *slot = meta(feed, false, false);
    ix
}

pub fn meta(pk: AnchorPubkey, is_signer: bool, is_writable: bool) -> AccountMeta {
    AccountMeta {
        pubkey: to_svm(pk),
        is_signer,
        is_writable,
    }
}

pub fn build_ix(accounts: Vec<AccountMeta>, data: Vec<u8>) -> Instruction {
    Instruction {
        program_id: to_svm(program_id()),
        accounts,
        data,
    }
}

/// Loads the repo-root `devnet-keypair.json`, whose pubkey matches the hardcoded
/// `ADMIN_PUBKEY` constant used throughout the program's admin-gated instructions.
pub fn load_admin_keypair() -> Keypair {
    load_keypair_json(concat!(env!("CARGO_MANIFEST_DIR"), "/../../devnet-keypair.json"))
}

/// Loads the repo-root `devnet-fee-collector-keypair.json`, whose pubkey matches
/// `FEE_COLLECTOR_PUBKEY` (the required signer for `refuel_backend_wallet`).
pub fn load_fee_collector_keypair() -> Keypair {
    load_keypair_json(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../devnet-fee-collector-keypair.json"
    ))
}

fn load_keypair_json(path: &str) -> Keypair {
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    let bytes: Vec<u8> = content
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().parse::<u8>().expect("parse keypair byte"))
        .collect();
    Keypair::try_from(bytes.as_slice()).expect("build keypair from bytes")
}

pub fn random_pubkey() -> AnchorPubkey {
    to_anchor(Keypair::new().pubkey())
}

pub struct TestCtx {
    pub svm: LiteSVM,
    pub admin: Keypair,
    /// Stands in for the protocol backend that signs custom-oracle prices off-chain.
    pub oracle_signer: Keypair,
    /// Price (10^-12 USD per whole token) that `send_with_price` signs for each mint.
    pub custom_prices: HashMap<AnchorPubkey, u64>,
}

pub fn setup() -> TestCtx {
    let mut svm = LiteSVM::new();

    let so_path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/deploy/short_stake001.so"
    );
    svm.add_program_from_file(to_svm(program_id()), so_path)
        .expect("failed to load short_stake001.so - run `anchor build -- --features devnet` first (tests use the devnet keys), then `cargo test --features devnet`");

    let admin = load_admin_keypair();
    assert_eq!(
        to_anchor(admin.pubkey()),
        short_stake001::constants::ADMIN_PUBKEY,
        "devnet-keypair.json no longer matches ADMIN_PUBKEY"
    );
    svm.airdrop(&admin.pubkey(), 100_000_000_000).unwrap();

    let mut ctx = TestCtx {
        svm,
        admin,
        oracle_signer: Keypair::new(),
        custom_prices: HashMap::new(),
    };
    // open/close/liquidate all pay protocol fees here, so it must always exist.
    ctx.create_ata(
        short_stake001::constants::FEE_COLLECTOR_PUBKEY,
        short_stake001::constants::usdc::ID,
        0,
    );
    ctx
}

/// USDC ATA of `FEE_COLLECTOR_PUBKEY` (pre-created by `setup`), where protocol fees land.
pub fn fee_collector_usdc_ata(usdc_mint: AnchorPubkey) -> AnchorPubkey {
    get_associated_token_address(&short_stake001::constants::FEE_COLLECTOR_PUBKEY, &usdc_mint)
}

/// Bytes the oracle signer signs:
/// program_id || token_mint || price u64 LE || publish_time i64 LE || action u8
/// (must match `short_stake001::signed_price`).
pub fn signed_price_message(
    token_mint: AnchorPubkey,
    price: u64,
    publish_time: i64,
    action: PriceAction,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(short_stake001::signed_price::SIGNED_PRICE_MESSAGE_LEN);
    message.extend_from_slice(program_id().as_ref());
    message.extend_from_slice(token_mint.as_ref());
    message.extend_from_slice(&price.to_le_bytes());
    message.extend_from_slice(&publish_time.to_le_bytes());
    message.push(action as u8);
    message
}

/// The action a signed price must carry to be accepted by `ix`.
pub fn price_action_for(ix: &Instruction) -> PriceAction {
    use short_stake001::instruction as sx;
    let disc = &ix.data[..8];
    if disc == sx::DepositStake::DISCRIMINATOR {
        PriceAction::DepositStake
    } else if disc == sx::OpenBorrowPosition::DISCRIMINATOR {
        PriceAction::OpenBorrowPosition
    } else if disc == sx::ClosePosition::DISCRIMINATOR {
        PriceAction::ClosePosition
    } else if disc == sx::LiquidatePosition::DISCRIMINATOR {
        PriceAction::LiquidatePosition
    } else {
        panic!("instruction does not consume a signed price")
    }
}

/// Ed25519 precompile instruction carrying `signer`'s signature over `message`.
pub fn ed25519_ix(signer: &Keypair, message: &[u8]) -> Instruction {
    let signature = signer.sign_message(message);
    solana_ed25519_program::new_ed25519_instruction_with_signature(
        message,
        signature.as_array(),
        signer.pubkey().as_array(),
    )
}

impl TestCtx {
    pub fn oracle_signer_pk(&self) -> AnchorPubkey {
        to_anchor(self.oracle_signer.pubkey())
    }

    /// Sets the price `send_with_price` signs for `token_mint` (replaces the old
    /// on-chain `create_custom_oracle` account).
    pub fn set_custom_price(&mut self, token_mint: AnchorPubkey, price: u64) {
        self.custom_prices.insert(token_mint, price);
    }

    pub fn unix_timestamp(&self) -> i64 {
        self.svm.get_sysvar::<anchor_lang::prelude::Clock>().unix_timestamp
    }

    /// Signed-price instruction for `token_mint` and `action` at the current on-chain time.
    pub fn signed_price_ix(&self, token_mint: AnchorPubkey, action: PriceAction) -> Instruction {
        self.signed_price_ix_at(token_mint, action, self.unix_timestamp())
    }

    /// Like `signed_price_ix`, but signed as of `publish_time` (to test price age limits).
    pub fn signed_price_ix_at(
        &self,
        token_mint: AnchorPubkey,
        action: PriceAction,
        publish_time: i64,
    ) -> Instruction {
        let price = *self
            .custom_prices
            .get(&token_mint)
            .expect("call set_custom_price for this mint first");
        let message = signed_price_message(token_mint, price, publish_time, action);
        ed25519_ix(&self.oracle_signer, &message)
    }

    pub fn new_funded_keypair(&mut self, lamports: u64) -> Keypair {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), lamports).unwrap();
        kp
    }

    /// Sends a transaction paid for and signed by `payer`, plus any `extra_signers`.
    pub fn send(
        &mut self,
        payer: &Keypair,
        ix: Instruction,
        extra_signers: &[&Keypair],
    ) -> Result<(), String> {
        self.send_ixs(payer, vec![ix], extra_signers)
    }

    /// Like `send`, but first places a freshly signed custom-oracle price for `token_mint`
    /// directly before `ix`, as the program requires.
    pub fn send_with_price(
        &mut self,
        payer: &Keypair,
        token_mint: AnchorPubkey,
        ix: Instruction,
        extra_signers: &[&Keypair],
    ) -> Result<(), String> {
        let price_ix = self.signed_price_ix(token_mint, price_action_for(&ix));
        self.send_ixs(payer, vec![price_ix, ix], extra_signers)
    }

    pub fn send_ixs(
        &mut self,
        payer: &Keypair,
        ixs: Vec<Instruction>,
        extra_signers: &[&Keypair],
    ) -> Result<(), String> {
        let mut signers: Vec<&Keypair> = vec![payer];
        signers.extend_from_slice(extra_signers);
        let blockhash = self.svm.latest_blockhash();
        let message = Message::new(&ixs, Some(&payer.pubkey()));
        let tx = Transaction::new(&signers, message, blockhash);
        self.svm
            .send_transaction(tx)
            .map(|_| ())
            // Program logs are appended so a failing test shows *why* (CU exhaustion, panics, ...).
            .map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")))
    }

    /// Directly injects an initialized SPL mint account (bypasses `InitializeMint`).
    pub fn create_mint(&mut self, mint: AnchorPubkey, decimals: u8, mint_authority: AnchorPubkey) {
        self.create_mint_with_freeze_authority(mint, decimals, mint_authority, None);
    }

    /// `create_mint` with an optional freeze authority.
    pub fn create_mint_with_freeze_authority(
        &mut self,
        mint: AnchorPubkey,
        decimals: u8,
        mint_authority: AnchorPubkey,
        freeze_authority: Option<AnchorPubkey>,
    ) {
        let mint_state = SplMint {
            mint_authority: COption::Some(mint_authority),
            supply: 0,
            decimals,
            is_initialized: true,
            freeze_authority: freeze_authority.map_or(COption::None, COption::Some),
        };
        let mut data = vec![0u8; SplMint::LEN];
        SplMint::pack(mint_state, &mut data).unwrap();
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(
                to_svm(mint),
                SvmAccount {
                    lamports,
                    data,
                    owner: to_svm(TOKEN_PROGRAM_ID),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
    }

    /// Directly injects an initialized SPL token account at the associated-token-account
    /// address for (`owner`, `mint`), pre-funded with `amount` base units.
    pub fn create_ata(&mut self, owner: AnchorPubkey, mint: AnchorPubkey, amount: u64) -> AnchorPubkey {
        let ata = get_associated_token_address(&owner, &mint);
        self.create_token_account(ata, owner, mint, amount)
    }

    /// Directly injects an initialized SPL token account at an arbitrary `address`
    /// (e.g. a non-ATA account, to test ATA enforcement).
    pub fn create_token_account(
        &mut self,
        address: AnchorPubkey,
        owner: AnchorPubkey,
        mint: AnchorPubkey,
        amount: u64,
    ) -> AnchorPubkey {
        let acc_state = SplTokenAccount {
            mint,
            owner,
            amount,
            delegate: COption::None,
            state: AccountState::Initialized,
            is_native: COption::None,
            delegated_amount: 0,
            close_authority: COption::None,
        };
        let mut data = vec![0u8; SplTokenAccount::LEN];
        SplTokenAccount::pack(acc_state, &mut data).unwrap();
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(
                to_svm(address),
                SvmAccount {
                    lamports,
                    data,
                    owner: to_svm(TOKEN_PROGRAM_ID),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        address
    }

    pub fn token_balance(&self, token_account: AnchorPubkey) -> u64 {
        let acc = self
            .svm
            .get_account(&to_svm(token_account))
            .expect("token account not found");
        SplTokenAccount::unpack(&acc.data).unwrap().amount
    }

    pub fn lamports(&self, pubkey: AnchorPubkey) -> u64 {
        self.svm.get_balance(&to_svm(pubkey)).unwrap_or(0)
    }

    pub fn account_exists(&self, pubkey: AnchorPubkey) -> bool {
        self.svm.get_account(&to_svm(pubkey)).is_some()
    }

    /// Writes a Pyth Receiver `PriceUpdateV2` account at `at`, owned by `owner` (the Pyth
    /// Receiver for a genuine-looking account), in the exact layout the program deserializes.
    #[allow(clippy::too_many_arguments)]
    pub fn set_pyth_price(
        &mut self,
        at: AnchorPubkey,
        owner: AnchorPubkey,
        feed_id: [u8; 32],
        price: i64,
        exponent: i32,
        publish_time: i64,
    ) {
        self.set_pyth_price_with_conf(at, owner, feed_id, price, 0, exponent, publish_time);
    }

    /// `set_pyth_price` with a confidence interval (same exponent as the price).
    #[allow(clippy::too_many_arguments)]
    pub fn set_pyth_price_with_conf(
        &mut self,
        at: AnchorPubkey,
        owner: AnchorPubkey,
        feed_id: [u8; 32],
        price: i64,
        conf: u64,
        exponent: i32,
        publish_time: i64,
    ) {
        use anchor_lang::AccountSerialize;
        use short_stake001::pyth_price_update::{PriceFeedMessage, PriceUpdateV2, VerificationLevel};
        let update = PriceUpdateV2 {
            write_authority: random_pubkey(),
            verification_level: VerificationLevel::Full,
            price_message: PriceFeedMessage {
                feed_id,
                price,
                conf,
                exponent,
                publish_time,
                prev_publish_time: publish_time,
                ema_price: price,
                ema_conf: 0,
            },
            posted_slot: 0,
        };
        let mut data = Vec::new();
        update.try_serialize(&mut data).unwrap();
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm
            .set_account(
                to_svm(at),
                SvmAccount { lamports, data, owner: to_svm(owner), executable: false, rent_epoch: 0 },
            )
            .unwrap();
    }

    /// Deserializes an Anchor `#[account]` struct directly from the ledger.
    pub fn fetch<T: anchor_lang::AccountDeserialize>(&self, pubkey: AnchorPubkey) -> T {
        let acc = self
            .svm
            .get_account(&to_svm(pubkey))
            .expect("account not found");
        T::try_deserialize(&mut acc.data.as_slice()).expect("failed to deserialize account")
    }

    /// Overwrites the on-chain Unix timestamp, e.g. to simulate a loan expiring.
    pub fn warp_unix_timestamp(&mut self, unix_timestamp: i64) {
        let mut clock = self.svm.get_sysvar::<anchor_lang::prelude::Clock>();
        clock.unix_timestamp = unix_timestamp;
        self.svm.set_sysvar(&clock);
    }
}

// ---- PDA helpers (seeds copied from the program's `#[account(seeds = ...)]` constraints) ----

pub fn global_config_pda() -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"global_config"], &program_id())
}

pub fn protocol_state_pda() -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"protocol_state"], &program_id())
}

pub fn global_usdc_vault_pda() -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"global_usdc_vault"], &program_id())
}

pub fn market_vault_pda(token_mint: AnchorPubkey) -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"market_vault", token_mint.as_ref()], &program_id())
}

pub fn vault_token_account_pda(token_mint: AnchorPubkey) -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"vault_token_account", token_mint.as_ref()], &program_id())
}

pub fn lender_offer_pda(staker: AnchorPubkey, token_mint: AnchorPubkey, offer_id: u64) -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(
        &[
            b"offer",
            staker.as_ref(),
            token_mint.as_ref(),
            &offer_id.to_le_bytes(),
        ],
        &program_id(),
    )
}

pub fn borrow_position_pda(borrower: AnchorPubkey, position_id: u64) -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(
        &[b"position", borrower.as_ref(), &position_id.to_le_bytes()],
        &program_id(),
    )
}
/*
pub fn protocol_treasury_pda() -> (AnchorPubkey, u8) {
    AnchorPubkey::find_program_address(&[b"protocol_treasury"], &program_id())
}
    */

// ---- Instruction builders (account order/mutability mirrors each `#[derive(Accounts)]`) ----

pub fn ix_initialize(
    admin: AnchorPubkey,
    global_config: AnchorPubkey,
    fee_rate_stakers_bps: u16,
    fee_rate_borrowers_bps: u16,
    liquidation_thresh_bps: u16,
    min_expiry_days: u8,
    max_expiry_days: u8,
    oracle_signer: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![
            meta(admin, true, true),
            meta(global_config, false, true),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(protocol_state_pda().0, false, true),
        ],
        short_stake001::instruction::Initialize {
            fee_rate_stakers_bps,
            fee_rate_borrowers_bps,
            liquidation_thresh_bps,
            min_expiry_days,
            max_expiry_days,
            oracle_signer,
        }
        .data(),
    )
}

pub fn ix_initialize_global_vault(
    admin: AnchorPubkey,
    global_config: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![
            meta(admin, true, true),
            meta(global_config, false, false),
            meta(usdc_mint, false, false),
            meta(global_usdc_vault, false, true),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
        ],
        short_stake001::instruction::InitializeGlobalVault {}.data(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn ix_deposit_stake(
    staker: AnchorPubkey,
    token_mint: AnchorPubkey,
    staker_token_account: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    lender_offer: AnchorPubkey,
    global_config: AnchorPubkey,
    amount_to_stake: u64,
    interest_rate_bps: u16,
    max_expiry_days: u8,
    offer_id: u64,
) -> Instruction {
    build_ix(
        vec![
            meta(staker, true, true),
            meta(token_mint, false, false),
            meta(staker_token_account, false, true),
            meta(market_vault, false, true),
            meta(vault_token_account, false, true),
            meta(lender_offer, false, true),
            meta(global_config, false, false),
            none_account_meta(), // pyth_price_feed: None
            meta(INSTRUCTIONS_SYSVAR_ID, false, false),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
            meta(anchor_spl::associated_token::ID, false, false),
            meta(protocol_state_pda().0, false, false),
        ],
        short_stake001::instruction::DepositStake {
            amount_to_stake,
            interest_rate_bps,
            max_expiry_days,
            offer_id,
        }
        .data(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn ix_open_borrow_position(
    borrower: AnchorPubkey,
    token_mint: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
    global_config: AnchorPubkey,
    lender_offer: AnchorPubkey,
    borrower_token_account: AnchorPubkey,
    borrower_usdc_account: AnchorPubkey,
    borrow_position: AnchorPubkey,
    borrow_amount: u64,
    position_id: u64,
    provided_collateral_usdc: u64,
) -> Instruction {
    // Accepts any offer terms; see ix_open_borrow_position_with_terms for the M-02 bounds.
    ix_open_borrow_position_with_terms(
        borrower, token_mint, usdc_mint, market_vault, vault_token_account, global_usdc_vault,
        global_config, lender_offer, borrower_token_account, borrower_usdc_account,
        borrow_position, borrow_amount, position_id, provided_collateral_usdc, u16::MAX, 0,
    )
}

/// open_borrow_position with the borrower's accepted terms: the offer's rate must be at most
/// `max_interest_rate_bps` and its expiry at least `min_expiry_days` (Slot Zero M-02).
#[allow(clippy::too_many_arguments)]
pub fn ix_open_borrow_position_with_terms(
    borrower: AnchorPubkey,
    token_mint: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
    global_config: AnchorPubkey,
    lender_offer: AnchorPubkey,
    borrower_token_account: AnchorPubkey,
    borrower_usdc_account: AnchorPubkey,
    borrow_position: AnchorPubkey,
    borrow_amount: u64,
    position_id: u64,
    provided_collateral_usdc: u64,
    max_interest_rate_bps: u16,
    min_expiry_days: u8,
) -> Instruction {
    build_ix(
        vec![
            meta(borrower, true, true),
            meta(token_mint, false, false),
            meta(usdc_mint, false, false),
            meta(market_vault, false, true),
            meta(vault_token_account, false, true),
            meta(global_usdc_vault, false, true),
            meta(global_config, false, false),
            meta(lender_offer, false, true),
            meta(borrower_token_account, false, true),
            meta(borrower_usdc_account, false, true),
            meta(fee_collector_usdc_ata(usdc_mint), false, true),
            meta(borrow_position, false, true),
            none_account_meta(), // pyth_price_feed: None
            meta(INSTRUCTIONS_SYSVAR_ID, false, false),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
            meta(protocol_state_pda().0, false, false),
        ],
        short_stake001::instruction::OpenBorrowPosition {
            borrow_amount,
            position_id,
            provided_collateral_usdc,
            max_interest_rate_bps,
            min_expiry_days,
        }
        .data(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn ix_close_position(
    borrower: AnchorPubkey,
    token_mint: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    borrower_token_account: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
    global_config: AnchorPubkey,
    staker_usdc_account: AnchorPubkey,
    borrower_usdc_account: AnchorPubkey,
    lender_offer: AnchorPubkey,
    borrow_position: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![
            meta(borrower, true, true),
            meta(token_mint, false, false),
            meta(usdc_mint, false, false),
            meta(borrower_token_account, false, true),
            meta(market_vault, false, true),
            meta(vault_token_account, false, true),
            meta(global_usdc_vault, false, true),
            meta(global_config, false, false),
            meta(staker_usdc_account, false, true),
            meta(borrower_usdc_account, false, true),
            meta(fee_collector_usdc_ata(usdc_mint), false, true),
            meta(lender_offer, false, true),
            meta(borrow_position, false, true),
            none_account_meta(), // pyth_price_feed: None
            meta(INSTRUCTIONS_SYSVAR_ID, false, false),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
        ],
        short_stake001::instruction::ClosePosition {}.data(),
    )
}

pub fn ix_early_unstake(
    staker: AnchorPubkey,
    payer_wallet: AnchorPubkey,
    token_mint: AnchorPubkey,
    staker_token_account: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    lender_offer: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![
            meta(staker, true, true),
            meta(payer_wallet, false, true),
            meta(token_mint, false, false),
            meta(staker_token_account, false, true),
            meta(market_vault, false, true),
            meta(vault_token_account, false, true),
            meta(lender_offer, false, true),
            meta(TOKEN_PROGRAM_ID, false, false),
        ],
        short_stake001::instruction::EarlyUnstake {}.data(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn ix_liquidate_position(
    liquidator: AnchorPubkey,
    borrower: AnchorPubkey,
    staker: AnchorPubkey,
    token_mint: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    liquidator_token_account: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
    global_config: AnchorPubkey,
    staker_usdc_account: AnchorPubkey,
    liquidator_usdc_account: AnchorPubkey,
    fee_collector_usdc_account: AnchorPubkey,
    borrower_usdc_account: AnchorPubkey,
    lender_offer: AnchorPubkey,
    borrow_position: AnchorPubkey,
) -> Instruction {
    // staker_token_account defaults to the staker's ATA for the borrowed mint.
    ix_liquidate_position_with_staker_token(
        liquidator, borrower, staker, token_mint, usdc_mint, liquidator_token_account, market_vault,
        vault_token_account, global_usdc_vault, global_config, staker_usdc_account,
        Some(get_associated_token_address(&staker, &token_mint)), liquidator_usdc_account,
        fee_collector_usdc_account, borrower_usdc_account, lender_offer, borrow_position,
    )
}

/// liquidate_position with an explicit optional staker_token_account (None = not passed).
#[allow(clippy::too_many_arguments)]
pub fn ix_liquidate_position_with_staker_token(
    liquidator: AnchorPubkey,
    borrower: AnchorPubkey,
    staker: AnchorPubkey,
    token_mint: AnchorPubkey,
    usdc_mint: AnchorPubkey,
    liquidator_token_account: AnchorPubkey,
    market_vault: AnchorPubkey,
    vault_token_account: AnchorPubkey,
    global_usdc_vault: AnchorPubkey,
    global_config: AnchorPubkey,
    staker_usdc_account: AnchorPubkey,
    staker_token_account: Option<AnchorPubkey>,
    liquidator_usdc_account: AnchorPubkey,
    fee_collector_usdc_account: AnchorPubkey,
    borrower_usdc_account: AnchorPubkey,
    lender_offer: AnchorPubkey,
    borrow_position: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![
            meta(liquidator, true, true),
            meta(borrower, false, true),
            meta(staker, false, true),
            meta(token_mint, false, false),
            meta(usdc_mint, false, false),
            meta(liquidator_token_account, false, true),
            meta(market_vault, false, true),
            meta(vault_token_account, false, true),
            meta(global_usdc_vault, false, true),
            meta(global_config, false, false),
            meta(staker_usdc_account, false, true),
            // staker_token_account (optional): write-off token refunds
            staker_token_account.map_or_else(none_account_meta, |a| meta(a, false, true)),
            meta(liquidator_usdc_account, false, true),
            meta(fee_collector_usdc_account, false, true),
            meta(borrower_usdc_account, false, true),
            meta(lender_offer, false, true),
            meta(borrow_position, false, true),
            none_account_meta(), // pyth_price_feed: None
            meta(INSTRUCTIONS_SYSVAR_ID, false, false),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
            meta(TOKEN_PROGRAM_ID, false, false),
        ],
        short_stake001::instruction::LiquidatePosition {}.data(),
    )
}

/*
pub fn ix_refuel_backend_wallet(
    protocol_backend_wallet: AnchorPubkey,
    protocol_treasury_pda: AnchorPubkey,
    amount: u64,
) -> Instruction {
    build_ix(
        vec![
            meta(protocol_backend_wallet, true, true),
            meta(protocol_treasury_pda, false, true),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
        ],
        short_stake001::instruction::RefuelBackendWallet { amount }.data(),
    )
}
    */

pub fn ix_update_config(
    authority: AnchorPubkey,
    global_config: AnchorPubkey,
    new_fee_stakers_bps: Option<u16>,
    new_fee_borrowers_bps: Option<u16>,
    new_liquidation_thresh_bps: Option<u16>,
    new_min_expiry_days: Option<u8>,
    new_max_expiry_days: Option<u8>,
    new_oracle_signer: Option<AnchorPubkey>,
) -> Instruction {
    build_ix(
        vec![meta(authority, true, true), meta(global_config, false, true)],
        short_stake001::instruction::UpdateConfig {
            new_fee_stakers_bps,
            new_fee_borrowers_bps,
            new_liquidation_thresh_bps,
            new_min_expiry_days,
            new_max_expiry_days,
            new_oracle_signer,
        }
        .data(),
    )
}

pub fn ix_set_paused(authority: AnchorPubkey, global_config: AnchorPubkey, paused: bool) -> Instruction {
    build_ix(
        vec![
            meta(authority, true, true),
            meta(global_config, false, false),
            meta(protocol_state_pda().0, false, true),
            meta(anchor_lang::solana_program::system_program::ID, false, false),
        ],
        short_stake001::instruction::SetPaused { paused }.data(),
    )
}

pub fn ix_accept_authority(new_authority: AnchorPubkey, global_config: AnchorPubkey) -> Instruction {
    build_ix(
        vec![meta(new_authority, true, false), meta(global_config, false, true)],
        short_stake001::instruction::AcceptAuthority {}.data(),
    )
}

pub fn ix_propose_authority(
    authority: AnchorPubkey,
    global_config: AnchorPubkey,
    new_authority: AnchorPubkey,
) -> Instruction {
    build_ix(
        vec![meta(authority, true, true), meta(global_config, false, true)],
        short_stake001::instruction::ProposeAuthority { new_authority }.data(),
    )
}
