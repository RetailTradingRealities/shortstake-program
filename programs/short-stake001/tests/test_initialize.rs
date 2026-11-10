use anchor_lang::{prelude::Pubkey, InstructionData, ToAccountMetas};

#[test]
fn test_initialize() {
    let program_id = short_stake001::id();
    let admin = short_stake001::constants::ADMIN_PUBKEY;
    let global_config = Pubkey::find_program_address(&[b"global_config"], &program_id).0;

    let data = short_stake001::instruction::Initialize {
        fee_rate_stakers_bps: 100,
        fee_rate_borrowers_bps: 200,
        liquidation_thresh_bps: 11000,
        min_expiry_days: 1,
        max_expiry_days: 30,
        oracle_signer: Pubkey::new_unique(),
    }
    .data();

    let accounts = short_stake001::accounts::InitializeConfig {
        admin,
        global_config,
        system_program: anchor_lang::solana_program::system_program::ID,
        protocol_state: Pubkey::new_unique(),
    }
    .to_account_metas(None);

    assert_eq!(data.len(), 8 + 2 + 2 + 2 + 1 + 1 + 32);
    assert_eq!(accounts.len(), 4);
}
