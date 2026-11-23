# Liquidation keeper on a Linux VPS

The keeper (`app/src/keeper/`) is one long-running Node process. Every `KEEPER_POLL_SECONDS` it
reads all open positions **from the chain**, prices each market (signed price from the
price-oracle function, refreshed right before each liquidation), runs the program's own liquidation check, and liquidates what
qualifies:

- **buyback** - Jupiter Lend flash loan -> Jupiter swap -> `liquidate_position` -> repay, in one
  atomic transaction; it keeps notional + 4% bounty minus swap cost. Needs no USDC up front.
- **write-off** (collateral can't cover buyback + bounty) - plain `liquidate_position`; the staker
  gets the collateral and the bad debt is cleared. Earns only the position's rent.

Every transaction is simulated before it's sent. Mainnet only.

## 1. Server

Any small Linux VPS works (1 vCPU / 1 GB). Pick a region close to your RPC provider.

```bash
# as root
adduser --disabled-password --gecos "" keeper
curl -fsSL https://deb.nodesource.com/setup_24.x | bash - && apt-get install -y nodejs git
su - keeper
git clone <your repo> short-stake001 && cd short-stake001
npm ci
```

The VPS runs whatever is on GitHub, so **push first** (the mainnet cutover commits: program
`sstow...`, fee collector = Config vault). For a private repo, give the VPS a read-only
**deploy key**: GitHub repo -> Settings -> Deploy keys. Create it on the VPS with
`ssh-keygen -t ed25519`, add the `.pub` there, and clone with the `git@github.com:...` URL.

## 2. Keeper wallet

Use a **dedicated** wallet - never the admin/deployer key. Create it on the VPS so the secret
never leaves it:

```bash
sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"   # Solana CLI (solana-keygen)
export PATH="$HOME/.local/share/solana/install/active_release/bin:$PATH"
solana-keygen new --no-bip39-passphrase -o /home/keeper/keeper-keypair.json
chmod 600 /home/keeper/keeper-keypair.json
solana-keygen pubkey /home/keeper/keeper-keypair.json
```

Fund it with **~0.3 SOL**: transaction fees, its own USDC + token ATAs (created once per market,
~0.002 SOL each) and its lookup table (~0.00022 SOL per address). It warns in the log below
0.05 SOL. Profits accumulate in its USDC ATA - sweep them to cold storage periodically.

## 3. Configure

```bash
cp .env.keeper.example .env.keeper && chmod 600 .env.keeper
nano .env.keeper          # KEEPER_RPC_URL, KEEPER_KEYPAIR, SUPABASE_ANON_KEY, JUPITER_API_KEY
```

First run: one dry-run cycle (builds and simulates, sends nothing):

```bash
KEEPER_DRY_RUN=true npm run keeper -- --once
```

It creates the keeper's lookup table on first start and saves it to `keeper-state.json`. Keep
that file (or set `KEEPER_LOOKUP_TABLE`) so restarts reuse it. When the table fills up (256
addresses), the keeper creates a fresh one and records it there, listing old ones under
`previousLookupTables` (their rent can be recovered later by deactivating and closing them).
A dry run sends nothing at all.

Safeguards (Slot Zero audit):
- **Price deviation (M-01):** it won't liquidate a custom-oracle position when the signed price
  is more than `KEEPER_MAX_PRICE_DEVIATION_BPS` (default 10%) above Jupiter's market price.
- **Jupiter allow-list (I-03):** it only signs Jupiter instructions for the Jupiter program and
  create-ATA, and checks profit against the worst-case fill.
- **Payouts (H-01):** a counterparty whose ATA was handed away is paid into another account it
  owns, which the keeper creates if needed (~0.002 SOL).
- **Backoff (L-03):** a buyback skipped as unprofitable waits 5 minutes unless its price moves
  2%, skips don't count against `KEEPER_MAX_PER_CYCLE`, and repeated failures back off
  exponentially (up to 10 minutes).

## 4. Run it 24/7 with systemd

```bash
# as root
cp /home/keeper/short-stake001/deploy/keeper/short-stake-keeper.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now short-stake-keeper
journalctl -u short-stake-keeper -f        # live logs
```

Then tell the monitor which wallet to watch (low SOL, positions left past expiry or past their
liquidation price), from your own machine:

```bash
supabase secrets set KEEPER_PUBKEY=<keeper public key> --project-ref euwmdcepgwcesoqntlnd
supabase functions deploy monitor --no-verify-jwt --project-ref euwmdcepgwcesoqntlnd
```

systemd restarts it after crashes and reboots; `systemctl stop` lets it finish the current step.
A healthy log prints one `cycle positions=... liquidatable=... attempted=...` line per poll.

Updating: `git pull && npm ci && sudo systemctl restart short-stake-keeper`.

## 5. A second instance (backup)

Run the same setup on a **different VPS** (ideally another provider/region) with its **own
wallet** and `KEEPER_STANDBY_DELAY_SECONDS=30`. It only acts on a position that has stayed
liquidatable for 30s - which means the primary is down or stuck. Two primaries would race for
the same positions and the loser pays fees for failed transactions.

## Scaling

One instance handles many positions; the limits are request rates, not CPU:

- **RPC**: each poll is one `getProgramAccounts` plus a few batched reads. Use a paid RPC.
- **Jupiter**: two calls per buyback attempt (more if a route must be re-quoted to fit the
  1232-byte transaction limit). Raise the Jupiter plan before adding instances.
- **Price oracle**: one signed-price request per custom-oracle market per ~20s.
- **Priority fees**: if other liquidators appear, set `KEEPER_PRIORITY_FEE_MICROLAMPORTS`.

## Testing

- `npm run test:keeper` - the trigger and write-off math against the program's formulas.
- `SURFPOOL_PORT=18899 npm run surfpool:keeper` - standby/primary cycles on a mainnet fork.
- `SURFPOOL_PORT=18899 npm run surfpool:flash` - both liquidation paths on real Jupiter routes.
