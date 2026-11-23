// ShortStake liquidation keeper - the long-running process for a VPS (see deploy/keeper/).
//
//   npm run keeper            # loop forever (systemd restarts it if it ever exits)
//   npm run keeper -- --once  # one cycle, then exit (smoke test)
//
// Configuration comes from the environment (.env.keeper on the VPS - see .env.keeper.example).
// Mainnet only: liquidations need Jupiter routes and the Jupiter Lend flash loan, neither of
// which exists on devnet. A Surfpool mainnet fork works for testing.
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import {
    type Address,
    address,
    createKeyPairSignerFromBytes,
    createSolanaRpc,
    createSolanaRpcSubscriptions,
} from "@solana/kit";
import { fetchToken } from "@solana-program/token";
import { findGlobalUsdcVaultPda } from "../../../clients/js/src/generated/pdas";
import { createKeeperLookupTable } from "../instructions/keeperLookupTable";
import { type SolanaClient } from "../instructions/OpenBorrowPosition";
import { Keeper, type KeeperConfig } from "./keeper";
import { LivePriceSource } from "./priceSource";
import { syncSiteLookupTable } from "./siteTable";

const USDC_MAINNET_MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
/** The mainnet program's FEE_COLLECTOR_PUBKEY (the Config Squad vault). */
const FEE_COLLECTOR = address("52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr");
const DEFAULT_ORACLE_URL = "https://euwmdcepgwcesoqntlnd.supabase.co/functions/v1/price-oracle";
const LOW_SOL_LAMPORTS = 50_000_000n; // 0.05 SOL: fees, ATA rent, lookup-table rent

function log(level: "info" | "warn" | "error", msg: string, fields: Record<string, unknown> = {}) {
    const kv = Object.entries(fields)
        .map(([k, v]) => `${k}=${typeof v === "string" ? v : JSON.stringify(v, (_k, x) => (typeof x === "bigint" ? x.toString() : x))}`)
        .join(" ");
    (level === "error" ? console.error : console.log)(`${new Date().toISOString()} ${level.toUpperCase().padEnd(5)} ${msg}${kv ? " " + kv : ""}`);
}

function env(name: string, fallback?: string): string {
    const v = process.env[name]?.trim();
    if (v) return v;
    if (fallback !== undefined) return fallback;
    throw new Error(`missing required environment variable ${name}`);
}

/** "0.05" (USDC) -> 50000n (micro-USDC). */
function parseUsdc(value: string): bigint {
    const [whole, frac = ""] = value.split(".");
    if (!/^\d+$/.test(whole!) || !/^\d{0,6}$/.test(frac)) throw new Error(`invalid USDC amount "${value}"`);
    return BigInt(whole!) * 1_000_000n + BigInt(frac.padEnd(6, "0"));
}

const sleep = (ms: number, signal: AbortSignal) =>
    new Promise<void>((resolve) => {
        const t = setTimeout(resolve, ms);
        signal.addEventListener("abort", () => (clearTimeout(t), resolve()), { once: true });
    });

async function main() {
    const once = process.argv.includes("--once");
    const rpcUrl = env("KEEPER_RPC_URL");
    const wsUrl = env("KEEPER_WS_URL", rpcUrl.replace(/^http/, "ws"));
    const client: SolanaClient = { rpc: createSolanaRpc(rpcUrl), rpcSubscriptions: createSolanaRpcSubscriptions(wsUrl) };
    const keeper = await createKeyPairSignerFromBytes(new Uint8Array(JSON.parse(readFileSync(env("KEEPER_KEYPAIR"), "utf8"))));

    // Refuse to run against a deployment whose USDC isn't mainnet USDC (e.g. devnet): the
    // instruction builders and Jupiter routes are mainnet-only.
    const [globalUsdcVault] = await findGlobalUsdcVaultPda();
    const vaultMint = (await fetchToken(client.rpc, globalUsdcVault)).data.mint;
    if (vaultMint !== USDC_MAINNET_MINT) {
        throw new Error(`global USDC vault holds ${vaultMint}, not mainnet USDC - the keeper only runs on mainnet (or a mainnet fork)`);
    }

    // The keeper's own lookup table: created once, then reused. KEEPER_LOOKUP_TABLE wins;
    // otherwise it's remembered in KEEPER_STATE_FILE.
    const stateFile = env("KEEPER_STATE_FILE", "keeper-state.json");
    const state: { lookupTable?: string; siteLookupTable?: string } = existsSync(stateFile) ? JSON.parse(readFileSync(stateFile, "utf8")) : {};
    let lookupTable: Address | undefined = process.env.KEEPER_LOOKUP_TABLE ? address(env("KEEPER_LOOKUP_TABLE")) : state.lookupTable ? address(state.lookupTable) : undefined;
    if (!lookupTable) {
        lookupTable = await createKeeperLookupTable(client, keeper);
        writeFileSync(stateFile, JSON.stringify({ ...state, lookupTable }, null, 2) + "\n");
        log("info", "created keeper lookup table", { lookupTable, savedTo: stateFile });
    }

    const config: KeeperConfig = {
        rpcUrl,
        jupiterApiKey: process.env.JUPITER_API_KEY?.trim() || undefined,
        lookupTable,
        minProfitUsdc: parseUsdc(env("KEEPER_MIN_PROFIT_USDC", "0.05")),
        slippageBps: Number(env("KEEPER_SLIPPAGE_BPS", "100")),
        priorityFeeMicroLamports: process.env.KEEPER_PRIORITY_FEE_MICROLAMPORTS ? BigInt(env("KEEPER_PRIORITY_FEE_MICROLAMPORTS")) : undefined,
        standbyDelaySeconds: Number(env("KEEPER_STANDBY_DELAY_SECONDS", "0")),
        maxPerCycle: Number(env("KEEPER_MAX_PER_CYCLE", "5")),
        dryRun: env("KEEPER_DRY_RUN", "false") === "true",
        maxPriceDeviationBps: Number(env("KEEPER_MAX_PRICE_DEVIATION_BPS", "1000")),
        log,
        // L-02: remember a rotated lookup table across restarts (KEEPER_LOOKUP_TABLE, if set, wins).
        onLookupTableRotated: (table) => {
            const current = existsSync(stateFile) ? JSON.parse(readFileSync(stateFile, "utf8")) : {};
            writeFileSync(stateFile, JSON.stringify({ ...current, lookupTable: table, previousLookupTables: [...(current.previousLookupTables ?? []), current.lookupTable].filter(Boolean) }, null, 2) + "\n");
        },
    };
    // The SITE lookup table (siteTable.ts), owned by this keeper's wallet: KEEPER_SITE_LOOKUP_TABLE
    // = its address, or "create" to make one on first start (saved in the state file). Unset: off.
    // Its address goes into the website's VITE_SITE_LOOKUP_TABLE.
    let siteLookupTable: Address | undefined;
    const siteSetting = process.env.KEEPER_SITE_LOOKUP_TABLE?.trim();
    if (siteSetting === "create") {
        const saved = (existsSync(stateFile) ? JSON.parse(readFileSync(stateFile, "utf8")) : {}) as { siteLookupTable?: string };
        if (saved.siteLookupTable) {
            siteLookupTable = address(saved.siteLookupTable);
        } else {
            siteLookupTable = await createKeeperLookupTable(client, keeper);
            const current = existsSync(stateFile) ? JSON.parse(readFileSync(stateFile, "utf8")) : {};
            writeFileSync(stateFile, JSON.stringify({ ...current, siteLookupTable }, null, 2) + "\n");
            log("info", "created SITE lookup table - set the website's VITE_SITE_LOOKUP_TABLE to it", { siteLookupTable, savedTo: stateFile });
        }
    } else if (siteSetting) {
        siteLookupTable = address(siteSetting);
    }
    const siteSyncMs = Number(env("KEEPER_SITE_TABLE_SYNC_MINUTES", "10")) * 60_000;
    let lastSiteSync = 0;
    /** Adds new markets' accounts to the site table. Never fails the cycle: logs and retries later. */
    const syncSiteTable = async () => {
        if (!siteLookupTable || config.dryRun || Date.now() - lastSiteSync < siteSyncMs) return;
        lastSiteSync = Date.now();
        try {
            const { markets } = await syncSiteLookupTable(client, keeper, siteLookupTable, address(USDC_MAINNET_MINT), FEE_COLLECTOR);
            log("info", "site lookup table synced", { siteLookupTable, markets });
        } catch (err) {
            log("warn", "site lookup table sync failed (retried next time)", { siteLookupTable, error: String(err) });
        }
    };

    const prices = new LivePriceSource(client.rpc, env("PRICE_ORACLE_URL", DEFAULT_ORACLE_URL), env("SUPABASE_ANON_KEY"));
    const bot = new Keeper(client, keeper, prices, config);
    const pollMs = Number(env("KEEPER_POLL_SECONDS", "15")) * 1000;

    log("info", "keeper starting", {
        keeper: keeper.address,
        lookupTable,
        siteLookupTable: siteLookupTable ?? "off",
        role: config.standbyDelaySeconds > 0 ? `standby (${config.standbyDelaySeconds}s delay)` : "primary",
        pollSeconds: pollMs / 1000,
        minProfitUsdc: Number(config.minProfitUsdc) / 1e6,
        dryRun: config.dryRun,
        jupiterApiKey: config.jupiterApiKey ? "set" : "keyless (0.5 RPS)",
    });

    // systemd sends SIGTERM on stop/restart: finish the current step, then exit cleanly.
    const stop = new AbortController();
    for (const sig of ["SIGTERM", "SIGINT"] as const) {
        process.on(sig, () => {
            log("info", `received ${sig}, stopping after the current cycle`);
            stop.abort();
        });
    }

    let consecutiveFailures = 0;
    while (!stop.signal.aborted) {
        try {
            const lamports = (await client.rpc.getBalance(keeper.address).send()).value;
            if (lamports < LOW_SOL_LAMPORTS) log("warn", "keeper SOL balance is low - top it up", { sol: Number(lamports) / 1e9 });

            await syncSiteTable();
            const report = await bot.runCycle();
            consecutiveFailures = 0;
            log("info", "cycle", {
                positions: report.positions,
                liquidatable: report.liquidatable,
                attempted: report.attempted,
                ...(report.results.length ? { results: report.results.map((r) => `${r.position.slice(0, 8)}:${r.status}`).join(",") } : {}),
            });
        } catch (err) {
            // RPC hiccups etc.: never let one bad cycle kill the process. Back off up to 5 min.
            consecutiveFailures++;
            log("error", "cycle failed", { error: String(err), consecutiveFailures });
        }
        if (once) break;
        const backoff = Math.min(pollMs * 2 ** Math.max(0, consecutiveFailures - 1), 300_000);
        await sleep(backoff, stop.signal);
    }
    log("info", "keeper stopped");
}

main().catch((err) => {
    log("error", "keeper failed to start", { error: err instanceof Error ? err.message : String(err) });
    process.exit(1);
});
