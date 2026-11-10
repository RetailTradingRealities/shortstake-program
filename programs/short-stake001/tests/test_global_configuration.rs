mod common;

use common::*;

#[test]
fn update_config_and_propose_authority() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_update_config(admin_pk, global_config, Some(150), None, Some(12_000), None, Some(45), None),
        &[],
    )
    .expect("update_config should succeed");

    let cfg: short_stake001::GlobalConfig = ctx.fetch(global_config);
    assert_eq!(cfg.fee_rate_stakers_bps, 150);
    assert_eq!(cfg.fee_rate_borrowers_bps, 25, "untouched field should be unchanged");
    assert_eq!(cfg.liquidation_thresh_bps, 12_000);
    assert_eq!(cfg.min_expiry_days, 1, "untouched field should be unchanged");
    assert_eq!(cfg.max_expiry_days, 45);

    let new_authority = random_pubkey();
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_propose_authority(admin_pk, global_config, new_authority),
        &[],
    )
    .expect("propose_authority should succeed");

    let cfg: short_stake001::GlobalConfig = ctx.fetch(global_config);
    assert_eq!(cfg.pending_authority, Some(new_authority));
    assert_eq!(cfg.authority, admin_pk, "active authority is unchanged until accepted");
}

#[test]
fn update_config_rejects_non_authority_signer() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize should succeed");

    let intruder = ctx.new_funded_keypair(10_000_000_000);
    let intruder_pk = to_anchor(intruder.pubkey());
    let result = ctx.send(
        &intruder,
        ix_update_config(intruder_pk, global_config, Some(999), None, None, None, None, None),
        &[],
    );
    assert!(result.is_err(), "update_config should reject a non-authority signer");
}

#[test]
fn fee_rates_above_caps_are_rejected() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();
    let max_stakers = short_stake001::constants::MAX_FEE_RATE_STAKERS_BPS;
    let max_borrowers = short_stake001::constants::MAX_FEE_RATE_BORROWERS_BPS;

    assert!(
        ctx.send(
            &ctx.admin.insecure_clone(),
            ix_initialize(admin_pk, global_config, max_stakers + 1, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
            &[],
        )
        .is_err(),
        "initialize should reject a staker fee above the cap"
    );
    assert!(
        ctx.send(
            &ctx.admin.insecure_clone(),
            ix_initialize(admin_pk, global_config, 500, max_borrowers + 1, 11_000, 1, 30, ctx.oracle_signer_pk()),
            &[],
        )
        .is_err(),
        "initialize should reject a borrower fee above the cap"
    );

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize at production fees should succeed");

    for (stakers, borrowers) in [(Some(max_stakers + 1), None), (None, Some(max_borrowers + 1))] {
        let result = ctx.send(
            &ctx.admin.insecure_clone(),
            ix_update_config(admin_pk, global_config, stakers, borrowers, None, None, None, None),
            &[],
        );
        assert!(result.is_err(), "update_config should reject fees above the cap");
    }

    // Exactly at the caps is allowed.
    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_update_config(admin_pk, global_config, Some(max_stakers), Some(max_borrowers), None, None, None, None),
        &[],
    )
    .expect("fees at the caps should be accepted");
}

#[test]
fn liquidation_threshold_is_bounded() {
    let mut ctx = setup();
    let admin_pk = to_anchor(ctx.admin.pubkey());
    let (global_config, _) = global_config_pda();
    let min = short_stake001::constants::MIN_LIQUIDATION_THRESH_BPS;
    let max = short_stake001::constants::MAX_LIQUIDATION_THRESH_BPS;

    for thresh in [min - 1, max + 1] {
        assert!(
            ctx.send(
                &ctx.admin.insecure_clone(),
                ix_initialize(admin_pk, global_config, 500, 25, thresh, 1, 30, ctx.oracle_signer_pk()),
                &[],
            )
            .is_err(),
            "initialize should reject a liquidation threshold outside the bounds"
        );
    }

    ctx.send(
        &ctx.admin.insecure_clone(),
        ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()),
        &[],
    )
    .expect("initialize at the production threshold should succeed");

    // A mistyped 900 (meaning 0.90) or 9000 must not slip through.
    for thresh in [900, 9_000, min - 1, max + 1] {
        let result = ctx.send(
            &ctx.admin.insecure_clone(),
            ix_update_config(admin_pk, global_config, None, None, Some(thresh), None, None, None),
            &[],
        );
        assert!(result.is_err(), "update_config should reject threshold {thresh}");
    }

    // Exactly at the bounds is allowed.
    for thresh in [min, max] {
        ctx.send(
            &ctx.admin.insecure_clone(),
            ix_update_config(admin_pk, global_config, None, None, Some(thresh), None, None, None),
            &[],
        )
        .expect("threshold at the bounds should be accepted");
    }
}

#[test]
fn authority_transfer_needs_the_proposed_key_to_accept() {
    let mut ctx = setup();
    let admin = ctx.admin.insecure_clone();
    let admin_pk = to_anchor(admin.pubkey());
    let (global_config, _) = global_config_pda();
    ctx.send(&admin, ix_initialize(admin_pk, global_config, 500, 25, 11_000, 1, 30, ctx.oracle_signer_pk()), &[])
        .expect("initialize should succeed");

    let successor = ctx.new_funded_keypair(1_000_000_000);
    let successor_pk = to_anchor(successor.pubkey());
    let intruder = ctx.new_funded_keypair(1_000_000_000);
    let intruder_pk = to_anchor(intruder.pubkey());

    // Nothing proposed yet: nobody can accept.
    assert!(ctx.send(&successor, ix_accept_authority(successor_pk, global_config), &[]).is_err());

    ctx.send(&admin, ix_propose_authority(admin_pk, global_config, successor_pk), &[])
        .expect("propose_authority should succeed");

    // Only the proposed key can accept.
    assert!(
        ctx.send(&intruder, ix_accept_authority(intruder_pk, global_config), &[]).is_err(),
        "a key that wasn't proposed must not be able to accept"
    );

    // Same bytes as the refused attempt above: a fresh blockhash keeps it from being AlreadyProcessed.
    ctx.svm.expire_blockhash();
    ctx.send(&successor, ix_accept_authority(successor_pk, global_config), &[])
        .expect("the proposed key should be able to accept");
    let cfg: short_stake001::GlobalConfig = ctx.fetch(global_config);
    assert_eq!(cfg.authority, successor_pk);
    assert_eq!(cfg.pending_authority, None, "the pending slot is cleared");

    // The old authority has lost control; the new one has it.
    assert!(
        ctx.send(&admin, ix_update_config(admin_pk, global_config, Some(600), None, None, None, None, None), &[])
            .is_err(),
        "the previous authority must no longer be able to update config"
    );
    ctx.send(&successor, ix_update_config(successor_pk, global_config, Some(600), None, None, None, None, None), &[])
        .expect("the new authority should be able to update config");

    // Accepting again is refused: the pending slot is empty.
    ctx.svm.expire_blockhash();
    assert!(ctx.send(&successor, ix_accept_authority(successor_pk, global_config), &[]).is_err());
}
