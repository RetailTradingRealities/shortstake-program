# ShortStake

ShortStake is a market for shorting SPL tokens on Solana.

- **Stakers** lend tokens at a daily interest rate.
- **Borrowers** post USDC collateral (150% to open), borrow those tokens and sell them, then buy
  them back to close.
- **Anyone** can liquidate a position that is undercollateralized or past its expiry, for a 4%
  bounty.

Website: https://shortstake.io · Security contact: security@shortstake.io

This repository holds the on-chain program, its TypeScript client, the price-oracle service and
the liquidation keeper we run ourselves. It is published so that users, researchers and outside
liquidators can read exactly what runs.

## Deployments

| | |
| --- | --- |
| **Program (mainnet)** | `sstowRTr15wR623yz3cUTRXJf8YhjgPBJ2TKBvNyZA3` |
| Upgrade authority | Squads 2-of-3 multisig vault `6n5r7M2Ws6NQoYAtV8rsdLVpsiawEmtA9PMt1dDEa9K1`, 24 h time lock |
| Config authority (fees, oracle signer, pause) | Squads 2-of-3 multisig vault `52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr` |
| Protocol fee account (USDC) | `7gZUk1P8NHBPiriAXQsepizYYryP4pkneFVHxbHmqQVv` |
| Oracle signer | `gyNUZv6Au6ZPDPGEsZ8VDEy5YxxSuQ28zEjseqTW82h` |
| Program (devnet, testing) | `G6zjYNx2LurrRnYWJZmvScUYQ1oavBfRkGWoDrcCWWVQ` |

**Verified build:** not yet. The mainnet program was built with `anchor build`. A
[`solana-verify`](https://github.com/Ellipsis-Labs/solana-verifiable-build) build is planned as a
later upgrade. Once it lands, explorers will show that the on-chain program matches this source.

## What's here

| Path | What |
| --- | --- |
| `programs/short-stake001/` | The Anchor program (Rust) and its tests |
| `clients/js/` | TypeScript client generated from the program's IDL (Codama, `@solana/kit`) |
| `app/src/oracle/` | Signed-price helpers used by the client and the keeper |
| `app/src/instructions/` | Transaction builders, including the keeper's flash-loan liquidation |
| `app/src/keeper/`, `deploy/keeper/` | The liquidation keeper and how to run it on a Linux server |
| `supabase/functions/price-oracle/` | The oracle that signs every market's prices |

## The program

The main instructions:
- `deposit_stake`: a staker lends tokens.
- `early_unstake`: a staker withdraws unlent tokens.
- `open_borrow_position`: a borrower opens a short, posting collateral.
- `close_position`: a borrower closes a short, returning the tokens and paying interest.
- `liquidate_position`: anyone can liquidate an eligible position.

Admin instructions (`initialize`, `update_config`, `propose_authority` / `accept_authority`,
`set_paused`) are restricted to the config authority. A pause only stops new stakes and borrows;
closing, unstaking and liquidating always work.

**Prices.** Every market is priced by the oracle signer: a signed price, verified on chain with an
Ed25519 instruction placed directly before the program instruction. Each signed price is for one
program, mint and action. The program accepts a liquidation price for 15 seconds
(`MAX_LIQUIDATION_PRICE_AGE_SECONDS`) and the others for 60: a liquidation signature names no
position, so a long window would let a liquidator keep the highest price of a falling market.

Until 2026-11-22, tokens on a Pyth list were priced from Pyth's sponsored push feeds instead. Pyth
stopped updating its meme-token feeds, so the program now ignores the stored `expected_oracle_feed`
and the optional `pyth_price_feed` account (both kept so account layouts don't change).

**Liquidation.** A position is liquidatable when its equity is at or below the maintenance
margin (110% of its current value, by default) or when it is past its expiry.
- **Buyback:** the liquidator returns the borrowed tokens. It is reimbursed their value at the
  liquidation price plus a **4% bounty**, and 1% more goes to the protocol.
- **Write-off:** if the collateral can't cover the tokens' value plus the bounty, the position is
  written off instead. The staker receives all of the collateral plus its unlent tokens, and the
  liquidator receives the position account's rent.

## Outside liquidators

Every market needs a signed price from the public oracle. Fetch a liquidation price right before
sending it (it is usable for about 15 seconds):

```
POST https://euwmdcepgwcesoqntlnd.supabase.co/functions/v1/price-oracle
Content-Type: application/json
apikey: <public API key below>
Authorization: Bearer <public API key below>

{ "mint": "<token mint>", "action": "liquidate" }
```

Public API key (Supabase "anon" key, the same one the website uses; it grants no write access):

```
eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJpc3MiOiJzdXBhYmFzZSIsInJlZiI6ImV1d21kY2VwZ3djZXNvcW50bG5kIiwicm9sZSI6ImFub24iLCJpYXQiOjE3OTE0NTg0MjYsImV4cCI6MjEwNzAzNDQyNn0.2Z9Dawk5boKn0QkEpYeBmui-frAIYjR8xPSbc_wMX90
```

`app/src/oracle/signedPrice.ts` (`fetchSignedPrice`, `getSignedPriceInstruction`) turns the
response into the Ed25519 instruction. `app/src/instructions/LiquidatePosition.ts`
(`executeLiquidatePosition`) builds a complete liquidation:
- a Jupiter Lend flash loan, then a Jupiter swap into the borrowed tokens, then
  `liquidate_position`, then the loan repayment, all in one transaction;
- or a plain write-off.

The keeper in `app/src/keeper/` scans positions on chain and calls it. See
`deploy/keeper/README.md` to run it. The FAQ on shortstake.io has a minimal example.

## Building and testing

Requirements: Rust per `rust-toolchain.toml`, Anchor 1.1.2, the Solana CLI, and Node 24.

```
anchor build                      # mainnet build (default)
anchor build -- --features devnet # devnet build: devnet program ID, admin, fee wallet and USDC
npx -y npm@11.6.1 ci              # the lockfile was written by npm 11.6; newer npm may reject it
npm run test:keeper               # keeper liquidation math against the program's formulas
```

The Rust tests (`programs/short-stake001/tests`, LiteSVM) run against the **devnet** build, and
they sign with the devnet admin key (`devnet-keypair.json`). That key is the devnet program's
upgrade authority, so it is **not** published, and the tests can't run from this repository as
is. They run in our private repository: `cargo test --features devnet`.

## Security

Please report vulnerabilities to security@shortstake.io (see https://shortstake.io/security).
The program also embeds a `security.txt`.

The program was reviewed by an AI-assisted audit (Slot Zero Security, November 2026), and the
findings were fixed before launch. It has not had a manual audit.

## License

Licensed under the Apache License, Version 2.0 (see `LICENSE`).
Copyright 2026 Retail Trading Realities Ltd.
