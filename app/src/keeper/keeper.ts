// One keeper cycle: scan open positions, price each market once, run the program's own
// liquidation check, and liquidate what qualifies via executeLiquidatePosition (flash-loan
// buyback or plain write-off). Stateless across restarts except for in-memory backoff and
// standby timers - safe to kill and restart at any time.
import { type Address, type TransactionSigner } from "@solana/kit";
import { createKeeperLookupTable } from "../instructions/keeperLookupTable";
import { fetchGlobalConfig } from "../../../clients/js/src/generated/accounts";
import { findGlobalConfigPda } from "../../../clients/js/src/generated/pdas";
import { type LiquidationResult, LookupTableFullError, executeLiquidatePosition } from "../instructions/LiquidatePosition";
import { checkLiquidatable } from "../instructions/liquidationPlan";
import { type SolanaClient } from "../instructions/OpenBorrowPosition";
import { type LiquidationPrice, type PriceSource } from "./priceSource";
import { type OpenPosition, loadOpenPositions } from "./scan";

export interface KeeperConfig {
    /** Solana RPC URL - also used by the Jupiter Lend SDK (web3.js). */
    rpcUrl: string;
    jupiterApiKey?: string;
    lookupTable?: Address;
    minProfitUsdc: bigint;
    slippageBps: number;
    priorityFeeMicroLamports?: bigint;
    /**
     * Backup instance: only act on a position that has been liquidatable for at least this long
     * (seconds). 0 = primary. A healthy primary has handled it by then, so two instances don't race.
     */
    standbyDelaySeconds: number;
    /** Most liquidations to attempt per cycle (each one is several RPC + Jupiter calls). */
    maxPerCycle: number;
    dryRun: boolean;
    /** Most the signed price may sit above Jupiter's market price, in bps (M-01). Default 1000. */
    maxPriceDeviationBps?: number;
    log: (level: "info" | "warn" | "error", msg: string, fields?: Record<string, unknown>) => void;
    /** Called after the keeper rotated to a fresh lookup table, so main.ts can persist it. */
    onLookupTableRotated?: (table: Address) => void;
}

export interface CycleReport {
    positions: number;
    liquidatable: number;
    attempted: number;
    results: { position: Address; status: AttemptStatus | "deferred" }[];
}

type AttemptStatus = LiquidationResult["status"] | "error" | "lookup_table_rotated";

// How long to leave a position alone after an outcome that won't change within seconds
// (Slot Zero L-03). Unprofitable buybacks wait 5 minutes unless their market's price moves
// (UNPROFITABLE_RETRY_ON_MOVE_BPS); failures back off exponentially from these values, capped
// at MAX_BACKOFF_SECONDS, so a handful of stuck positions can't hog every slot.
const BACKOFF_SECONDS: Partial<Record<AttemptStatus, number>> = {
    skipped_unprofitable: 300,
    skipped_price_deviation: 30,
    simulation_failed: 30,
    transaction_too_large: 120,
    error: 30,
};
const MAX_BACKOFF_SECONDS = 600;
const UNPROFITABLE_RETRY_ON_MOVE_BPS = 200n;
/** Outcomes that don't use up one of the cycle's maxPerCycle attempts. */
const FREE_OUTCOMES: ReadonlySet<AttemptStatus> = new Set(["skipped_unprofitable", "skipped_price_deviation", "lookup_table_rotated"]);

export class Keeper {
    /** When each position was first seen liquidatable (standby delay). */
    private readonly firstLiquidatable = new Map<Address, number>();
    /** Positions to skip until this time (ms). */
    private readonly backoffUntil = new Map<Address, number>();
    /** Consecutive failed attempts per position (exponential backoff). */
    private readonly failures = new Map<Address, number>();
    /** Price (10^-12 USD) when a buyback was skipped as unprofitable: a move re-arms it early. */
    private readonly unprofitableAt = new Map<Address, bigint>();

    constructor(
        private readonly client: SolanaClient,
        private readonly keeper: TransactionSigner,
        private readonly prices: PriceSource,
        private readonly config: KeeperConfig,
    ) {}

    async runCycle(nowUnix: bigint = BigInt(Math.floor(Date.now() / 1000))): Promise<CycleReport> {
        const { log } = this.config;
        const [globalConfig] = await findGlobalConfigPda();
        const [{ data: config }, positions] = await Promise.all([
            fetchGlobalConfig(this.client.rpc, globalConfig),
            loadOpenPositions(this.client.rpc),
        ]);

        // One price per market per cycle; a market whose price fails is skipped this cycle.
        const prices = new Map<Address, LiquidationPrice>();
        for (const mint of new Set(positions.map((p) => p.market.tokenMint))) {
            const flag = positions.find((p) => p.market.tokenMint === mint)!.market.expectedOracleFeed;
            try {
                prices.set(mint, await this.prices.liquidationPrice(mint, flag));
            } catch (err) {
                log("warn", "no price for market, skipping its positions this cycle", { mint, error: String(err) });
            }
        }

        const due: { p: OpenPosition; shortfall: bigint; expired: boolean }[] = [];
        for (const p of positions) {
            const price = prices.get(p.market.tokenMint);
            if (!price) continue;
            const check = checkLiquidatable({
                amountBorrowed: p.position.amountBorrowed,
                collateralUsdc: p.position.collateralDeposited,
                entryPriceUsd: p.position.entryPriceUsd,
                priceUsd: price.priceUsd,
                decimals: p.decimals,
                interestRateBps: p.offer.interestRateBps,
                loanStartTimestamp: p.position.loanStartTimestamp,
                expiryTimestamp: p.position.expiryTimestamp,
                liquidationThreshBps: config.liquidationThreshBps,
                nowUnix,
            });
            if (check.liquidatable) {
                due.push({ p, shortfall: check.maintenanceUsdc - check.equityUsdc, expired: check.isExpired });
            } else {
                this.firstLiquidatable.delete(p.address);
            }
        }
        // Forget timers for positions that no longer exist.
        const live = new Set(positions.map((p) => p.address));
        for (const map of [this.firstLiquidatable, this.backoffUntil, this.failures, this.unprofitableAt]) {
            for (const key of map.keys()) if (!live.has(key)) map.delete(key);
        }

        // Most underwater first: those are closest to becoming bad debt.
        due.sort((a, b) => (b.shortfall > a.shortfall ? 1 : b.shortfall < a.shortfall ? -1 : 0));

        const report: CycleReport = { positions: positions.length, liquidatable: due.length, attempted: 0, results: [] };
        const nowMs = Date.now();
        for (const { p, expired } of due) {
            if (!this.firstLiquidatable.has(p.address)) this.firstLiquidatable.set(p.address, nowMs);
            const waitedMs = nowMs - this.firstLiquidatable.get(p.address)!;
            if (waitedMs < this.config.standbyDelaySeconds * 1000) {
                report.results.push({ position: p.address, status: "deferred" });
                continue;
            }
            const price = prices.get(p.market.tokenMint)!;
            const skippedAt = this.unprofitableAt.get(p.address);
            if (skippedAt !== undefined && skippedAt > 0n) {
                const moveBps = ((price.priceUsd > skippedAt ? price.priceUsd - skippedAt : skippedAt - price.priceUsd) * 10_000n) / skippedAt;
                if (moveBps >= UNPROFITABLE_RETRY_ON_MOVE_BPS) this.backoffUntil.delete(p.address);
            }
            if ((this.backoffUntil.get(p.address) ?? 0) > nowMs) continue;
            if (report.attempted >= this.config.maxPerCycle) break;

            const status = await this.liquidate(p, price, expired);
            if (!FREE_OUTCOMES.has(status)) report.attempted++;
            report.results.push({ position: p.address, status });

            if (status === "sent" || status === "simulated") {
                this.failures.delete(p.address);
                this.unprofitableAt.delete(p.address);
            } else if (status === "skipped_unprofitable") {
                this.unprofitableAt.set(p.address, price.priceUsd);
            }
            const base = BACKOFF_SECONDS[status];
            if (base) {
                const failing = status === "simulation_failed" || status === "transaction_too_large" || status === "error";
                const n = failing ? (this.failures.get(p.address) ?? 0) + 1 : 1;
                if (failing) this.failures.set(p.address, n);
                const seconds = Math.min(base * 2 ** (n - 1), Math.max(base, MAX_BACKOFF_SECONDS));
                this.backoffUntil.set(p.address, Date.now() + seconds * 1000);
            }
        }
        return report;
    }

    private async liquidate(p: OpenPosition, price: LiquidationPrice, expired: boolean): Promise<AttemptStatus> {
        const { log } = this.config;
        const fields = {
            position: p.address,
            mint: p.market.tokenMint,
            trigger: expired ? "expired" : "undercollateralized",
            priceUsd: (Number(price.priceUsd) / 1e12).toString(),
        };
        try {
            const result = await executeLiquidatePosition(
                this.client,
                this.keeper,
                p.market.tokenMint,
                p.offer.stakerPubkey,
                p.position.borrowerPubkey,
                BigInt(p.offer.offerId),
                BigInt(p.position.positionId),
                price.oracle,
                {
                    rpcUrl: this.config.rpcUrl,
                    jupiterApiKey: this.config.jupiterApiKey,
                    lookupTable: this.config.lookupTable,
                    minProfitUsdc: this.config.minProfitUsdc,
                    slippageBps: this.config.slippageBps,
                    priorityFeeMicroLamports: this.config.priorityFeeMicroLamports,
                    dryRun: this.config.dryRun,
                    maxPriceDeviationBps: this.config.maxPriceDeviationBps,
                },
            );
            const extra: Record<string, unknown> = { path: result.plan.path, payoutUsdc: Number(result.plan.payoutUsdc) / 1e6 };
            if (result.status === "sent") Object.assign(extra, { signature: result.signature, bytes: result.transactionBytes, cu: result.unitsConsumed?.toString() });
            if (result.status === "skipped_unprofitable") extra.expectedProfitUsdc = Number(result.expectedProfitUsdc) / 1e6;
            if (result.status === "transaction_too_large") extra.bytes = result.transactionBytes;
            if (result.status === "simulation_failed") extra.logs = result.logs.slice(-6);
            if (result.status === "skipped_price_deviation") {
                Object.assign(extra, {
                    signedPriceUsd: Number(result.signedPriceUsd) / 1e12,
                    marketPriceUsd: Number(result.marketPriceUsd) / 1e12,
                });
            }
            log(result.status === "sent" || result.status === "simulated" ? "info" : "warn", result.status, { ...fields, ...extra });
            return result.status;
        } catch (err) {
            // A full lookup table (Slot Zero L-02): switch to a fresh one and retry next cycle.
            if (err instanceof LookupTableFullError && !this.config.dryRun) {
                const table = await createKeeperLookupTable(this.client, this.keeper);
                log("warn", "keeper lookup table full, rotated to a new one", { ...fields, full: err.table, lookupTable: table });
                this.config.lookupTable = table;
                this.config.onLookupTableRotated?.(table);
                return "lookup_table_rotated";
            }
            log("error", "liquidation attempt threw", { ...fields, error: String(err) });
            return "error";
        }
    }
}
