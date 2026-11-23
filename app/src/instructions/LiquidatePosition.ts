import {
    type Address,
    type TransactionSigner,
    type Instruction,
    address,
    AccountRole,
    pipe,
    createTransactionMessage,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    compressTransactionMessageUsingAddressLookupTables,
    fetchAddressesForLookupTables,
    signTransactionMessageWithSigners,
    sendAndConfirmTransactionFactory,
    getSignatureFromTransaction,
    getBase64EncodedWireTransaction,
} from '@solana/kit';
import { assertIsTransactionWithBlockhashLifetime } from "@solana/transactions";
import {
    ASSOCIATED_TOKEN_PROGRAM_ADDRESS,
    TOKEN_PROGRAM_ADDRESS,
    fetchMint,
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
} from '@solana-program/token';
import fetch from "cross-fetch";

import { getLiquidatePositionInstructionAsync } from '../../../clients/js/src/generated/instructions/liquidatePosition';
import { findBorrowPositionPda, findLenderOfferPda } from '../../../clients/js/src/generated/pdas';
import { fetchBorrowPosition } from '../../../clients/js/src/generated/accounts';
import { readPythPrice } from '../oracle/pyth';
import { isNoRouteError, sizeExactInBuyback } from './exactInBuyback';
import { getFlashLoanInstructions } from './flashLoan';
import { ensureInLookupTable, lookupCandidates, sendIxs } from './keeperLookupTable';
import { resolvePayoutAccount } from './payoutAccounts';
import { type LiquidationPlan, expectedBuybackProfit, planLiquidation } from './liquidationPlan';
import { type OracleSelection, type SolanaClient, getOraclePrelude } from './OpenBorrowPosition';

// Fixed by the program's `#[account(address = usdc_mainnet::ID)]` constraint - never variable per call.
const USDC_MAINNET_MINT = address("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");

// Mirrors constants::FEE_COLLECTOR_PUBKEY of the MAINNET build (the Config Squad vault); not a `#[constant]`, so Codama doesn't export it.
const FEE_COLLECTOR_PUBKEY = address("52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr");

// Jupiter Metis Swap API v1: the only Jupiter swap API with ExactOut (Swap v2 /build is
// ExactIn only), and we must receive EXACTLY amount_borrowed. Keyless works at low rate limits;
// pass an API key (x-api-key) in production. quote-api.jup.ag/v6 no longer resolves.
// FORK TESTS ONLY may point this elsewhere (e.g. a local proxy limited to ordinary pools, since
// proprietary market makers reject trades on a fork's stale slot). Unset everywhere else.
const JUPITER_SWAP_API = process.env.KEEPER_JUPITER_SWAP_API_FOR_FORK_TESTS ?? "https://api.jup.ag/swap/v1";

const COMPUTE_BUDGET_PROGRAM = address("ComputeBudget111111111111111111111111111111");
const MAX_TRANSACTION_BYTES = 1232;

// The only programs a Jupiter response may make the keeper sign for (Slot Zero I-03). The swap
// itself is one Jupiter instruction that CPIs into the DEXes; setup/cleanup are create-ATA
// instructions. We ask for wrapAndUnwrapSol=false, so no System or wSOL token instructions.
const JUPITER_V6_PROGRAM = "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4";
const ATA_CREATE = 0;
const ATA_CREATE_IDEMPOTENT = 1;

/**
 * Throws unless every Jupiter instruction is the Jupiter swap program, or a create-ATA (the ATA
 * program's Create / CreateIdempotent). A compromised or hostile Jupiter response could
 * otherwise slip in a token Approve / SetAuthority / Transfer or a System transfer over the
 * keeper's accounts, and it would still pass simulation because the flash loan is repaid.
 */
export function assertSafeJupiterInstructions(instructions: readonly JupiterInstruction[]): void {
    for (const ix of instructions) {
        if (ix.programId === JUPITER_V6_PROGRAM) continue;
        if (ix.programId === ASSOCIATED_TOKEN_PROGRAM_ADDRESS) {
            const data = Buffer.from(ix.data, "base64");
            if (data.length === 0 || data[0] === ATA_CREATE || data[0] === ATA_CREATE_IDEMPOTENT) continue;
        }
        throw new Error(`refusing Jupiter instruction for program ${ix.programId} (not on the keeper's allow-list)`);
    }
}

/** The thrown error when the keeper's lookup table can't take more addresses (keeper.ts rotates). */
export class LookupTableFullError extends Error {
    constructor(readonly table: Address) {
        super(`keeper lookup table ${table} is full`);
        this.name = "LookupTableFullError";
    }
}

// Jupiter's raw instruction/account-meta JSON shape (base64-encoded instruction data).
interface JupiterAccountMeta {
    pubkey: string;
    isSigner: boolean;
    isWritable: boolean;
}
interface JupiterInstruction {
    programId: string;
    accounts: JupiterAccountMeta[];
    data: string;
}

function roleFromFlags(isSigner: boolean, isWritable: boolean): AccountRole {
    if (isSigner && isWritable) return AccountRole.WRITABLE_SIGNER;
    if (isSigner) return AccountRole.READONLY_SIGNER;
    if (isWritable) return AccountRole.WRITABLE;
    return AccountRole.READONLY;
}

function toKitInstruction(raw: JupiterInstruction): Instruction {
    return {
        programAddress: address(raw.programId),
        accounts: raw.accounts.map((meta) => ({
            address: address(meta.pubkey),
            role: roleFromFlags(meta.isSigner, meta.isWritable),
        })),
        data: Buffer.from(raw.data, "base64"),
    };
}

// ComputeBudget instructions, encoded by hand (tag byte + LE integer) to avoid a dependency.
// Jupiter's own compute-budget instructions are dropped: its limit only covers the swap, and a
// second SetComputeUnitLimit in one transaction fails it.
function setComputeUnitLimitIx(units: number): Instruction {
    const data = Buffer.alloc(5);
    data.writeUInt8(2, 0);
    data.writeUInt32LE(units, 1);
    return { programAddress: COMPUTE_BUDGET_PROGRAM, accounts: [], data };
}
function setComputeUnitPriceIx(microLamports: bigint): Instruction {
    const data = Buffer.alloc(9);
    data.writeUInt8(3, 0);
    data.writeBigUInt64LE(microLamports, 1);
    return { programAddress: COMPUTE_BUDGET_PROGRAM, accounts: [], data };
}

async function jupiterJson(url: string, apiKey: string | undefined, init?: { method: string; body: string }) {
    const headers: Record<string, string> = { "Content-Type": "application/json" };
    if (apiKey) headers["x-api-key"] = apiKey;
    const res = await fetch(url, { ...init, headers });
    const json = await res.json();
    if (!res.ok || json.error) throw new Error(`Jupiter ${url.split("?")[0]}: ${json.error ?? res.status}`);
    return json;
}

export interface LiquidationOptions {
    /** Needed by the Jupiter Lend SDK (web3.js), for the buyback path. Use the same cluster as `client`. */
    rpcUrl: string;
    slippageBps?: number;
    /** Skip a buyback whose expected profit (payout - Jupiter's expected input - flash fee) is at or below this. */
    minProfitUsdc?: bigint;
    jupiterApiKey?: string;
    /**
     * Pins the Jupiter route's account cap. By default the keeper tries Jupiter's default (64),
     * then progressively tighter caps until the transaction fits in 1232 bytes.
     */
    maxAccounts?: number;
    /** A flash-loan buyback measured ~315k CU. Priority fees are charged on the limit, not on usage. */
    computeUnitLimit?: number;
    priorityFeeMicroLamports?: bigint;
    /** Build and simulate, but don't send - and send NOTHING else either (no table extends). */
    dryRun?: boolean;
    /**
     * Custom-oracle markets: refuse to liquidate when the signed price is more than this above
     * the Jupiter market price (Slot Zero M-01: a pumped pool's signed price would pay the
     * liquidator out of the borrower's margin). Default 1000 (10%). Fork tests that sign far
     * above market on purpose raise it.
     */
    maxPriceDeviationBps?: number;
    /**
     * The keeper's own lookup table (see keeperLookupTable.ts), owned by the liquidator. Needed for
     * the flash-loan path to fit in 1232 bytes; missing accounts are added to it automatically.
     */
    lookupTable?: Address;
}

export type LiquidationResult =
    | { status: "sent"; plan: LiquidationPlan; signature: string; unitsConsumed?: bigint; transactionBytes: number }
    | { status: "simulated"; plan: LiquidationPlan; unitsConsumed?: bigint; logs: readonly string[]; transactionBytes: number }
    | { status: "skipped_unprofitable"; plan: LiquidationPlan; expectedProfitUsdc: bigint }
    | { status: "skipped_price_deviation"; plan: LiquidationPlan; signedPriceUsd: bigint; marketPriceUsd: bigint }
    | { status: "transaction_too_large"; plan: LiquidationPlan; transactionBytes: number; needsLookupTable: boolean }
    | { status: "simulation_failed"; plan: LiquidationPlan; error: unknown; logs: readonly string[] };

/**
 * Liquidates an expired or undercollateralized position, choosing the path the program will take
 * from the same price it will see (see liquidationPlan.ts):
 *
 * - buyback: flash-borrow USDC (Jupiter Lend) -> Jupiter ExactOut swap into amount_borrowed of the
 *   token -> liquidate_position (returns the tokens, pays us notional + 4% bounty) -> repay the
 *   flash loan. All atomic: if the payout can't cover the repayment, the whole transaction reverts.
 * - write-off: collateral can't cover buyback + bounty, so the program takes no tokens and pays us
 *   nothing. A flash loan could never be repaid there, so this sends a plain liquidate_position:
 *   the staker gets the collateral and the bad debt is cleared. We only earn the position's rent.
 *
 * Every transaction is simulated first; nothing is sent if the simulation fails.
 */
export async function executeLiquidatePosition(
    client: SolanaClient,
    liquidator: TransactionSigner<Address>,
    tokenMint: Address,
    stakerPubkey: Address,
    borrowerPubkey: Address,
    offerId: bigint,
    positionId: bigint,
    // liquidate_position always needs exactly one real price source (picked by the program based
    // on market_vault.expected_oracle_feed) - there's no valid "none" here, same as close_position.
    oracle: Exclude<OracleSelection, { kind: "none" }>,
    options: LiquidationOptions,
): Promise<LiquidationResult> {
    const {
        rpcUrl,
        slippageBps = 50,
        minProfitUsdc = 0n,
        jupiterApiKey,
        maxAccounts,
        computeUnitLimit = 600_000,
        priorityFeeMicroLamports,
        dryRun = false,
        lookupTable,
        maxPriceDeviationBps = 1_000,
    } = options;

    // lenderOffer/borrowPosition aren't auto-resolved by the async builder (depend on
    // caller-chosen offerId/positionId), so they have to be derived up front.
    const [lenderOffer] = await findLenderOfferPda({ staker: stakerPubkey, tokenMint, offerId });
    const [borrowPosition] = await findBorrowPositionPda({ borrower: borrowerPubkey, positionId });

    const [{ data: position }, { data: mint }] = await Promise.all([
        fetchBorrowPosition(client.rpc, borrowPosition),
        fetchMint(client.rpc, tokenMint),
    ]);

    // The price the program will use. A signed price is exact; a Pyth price can still move before
    // the transaction lands - if it tips the position into the other path, simulation fails and
    // nothing is sent.
    const priceUsd =
        oracle.kind === "custom" ? oracle.signedPrice.priceUsd : (await readPythPrice(client.rpc, oracle.priceFeed)).units;
    const plan = planLiquidation({
        amountBorrowed: position.amountBorrowed,
        collateralUsdc: position.collateralDeposited,
        priceUsd,
        decimals: mint.decimals,
    });

    const ata = async (owner: Address, mintAddress: Address) =>
        (await findAssociatedTokenPda({ owner, mint: mintAddress, tokenProgram: TOKEN_PROGRAM_ADDRESS }))[0];
    // Counterparty payout accounts: any account they own (payoutAccounts.ts, Slot Zero H-01), so
    // a hijacked or frozen ATA can't block the liquidation.
    // staker_token_account is optional and only used by a write-off (the offer's unborrowed
    // tokens go back to the staker), so a buyback leaves it out: fewer bytes, nothing to create.
    const isWriteOff = plan.path !== "buyback";
    const [stakerUsdc, stakerToken, borrowerUsdc, liquidatorUsdcAccount, feeCollectorUsdcAccount] = await Promise.all([
        resolvePayoutAccount(client, liquidator, stakerPubkey, USDC_MAINNET_MINT),
        isWriteOff ? resolvePayoutAccount(client, liquidator, stakerPubkey, tokenMint) : Promise.resolve(null),
        resolvePayoutAccount(client, liquidator, borrowerPubkey, USDC_MAINNET_MINT),
        ata(liquidator.address, USDC_MAINNET_MINT),
        ata(FEE_COLLECTOR_PUBKEY, USDC_MAINNET_MINT),
    ]);
    const stakerUsdcAccount = stakerUsdc.address;
    const stakerTokenAccount = stakerToken?.address;
    const borrowerUsdcAccount = borrowerUsdc.address;
    // A fresh counterparty account needs its own transaction first. Never in a dry run (no side
    // effects); its simulation then fails on the missing account, which says why.
    const payouts = [stakerUsdc, stakerToken, borrowerUsdc].filter((p) => p !== null);
    const separateCreates = payouts.flatMap((p) => p.createSeparately ?? []);
    if (separateCreates.length > 0) {
        if (dryRun) console.warn("[liquidate] a counterparty needs a fresh payout account; dry run, not creating it");
        else await sendIxs(client, liquidator, separateCreates);
    }

    // get_current_price needs the source chosen by market_vault.expected_oracle_feed: the
    // pyth_price_feed account, or a signed-price Ed25519 instruction directly before
    // liquidate_position (the keeper signs a fresh one for each liquidation).
    const oracleAccounts: { pythPriceFeed?: Address } =
        oracle.kind === "pyth" ? { pythPriceFeed: oracle.priceFeed } : {};

    // usdcMint, liquidatorTokenAccount, marketVault, vaultTokenAccount, globalUsdcVault,
    // globalConfig, systemProgram and tokenProgram are all auto-derived/defaulted by the
    // async builder.
    const liquidatePositionIx = await getLiquidatePositionInstructionAsync({
        liquidator,
        borrower: borrowerPubkey,
        staker: stakerPubkey,
        tokenMint,
        stakerUsdcAccount,
        ...(stakerTokenAccount ? { stakerTokenAccount } : {}),
        liquidatorUsdcAccount,
        feeCollectorUsdcAccount,
        borrowerUsdcAccount,
        lenderOffer,
        borrowPosition,
        ...oracleAccounts,
    });

    // Every account liquidate_position pays into (or merely requires) must exist. Only missing
    // ones get a create instruction - each one costs transaction bytes. The liquidator pays rent.
    const createAtasIfMissing = async (atas: [owner: Address, mint: Address, ata: Address][]) => {
        const { value } = await client.rpc
            .getMultipleAccounts(atas.map(([, , a]) => a), { encoding: "base64", dataSlice: { offset: 0, length: 0 } })
            .send();
        return Promise.all(
            atas
                .filter((_, i) => value[i] === null)
                .map(([owner, mintAddress]) =>
                    getCreateAssociatedTokenIdempotentInstructionAsync({ payer: liquidator, owner, mint: mintAddress }),
                ),
        );
    };
    const payoutAtaIxs = [
        ...payouts.flatMap((p) => (p.createInline ? [p.createInline] : [])),
        ...(await createAtasIfMissing([[FEE_COLLECTOR_PUBKEY, USDC_MAINNET_MINT, feeCollectorUsdcAccount]])),
    ];

    const computeBudgetIxs = [
        setComputeUnitLimitIx(computeUnitLimit),
        ...(priorityFeeMicroLamports ? [setComputeUnitPriceIx(priorityFeeMicroLamports)] : []),
    ];

    /** Compiles, signs and measures one candidate transaction. */
    const compile = async (instructions: Instruction[], lookupTableAddresses: Address[]) => {
        const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();
        // Address Lookup Tables must actually be applied, otherwise large Jupiter routes can
        // exceed the transaction size limit despite the accounts being eligible for compression.
        // (An empty lookup table list is a safe no-op here.)
        const addressesByLookupTableAddress = await fetchAddressesForLookupTables(lookupTableAddresses, client.rpc);
        const transactionMessage = pipe(
            createTransactionMessage({ version: 0 }),
            (m) => setTransactionMessageFeePayerSigner(liquidator, m),
            (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
            (m) => appendTransactionMessageInstructions(instructions, m),
            (m) => compressTransactionMessageUsingAddressLookupTables(m, addressesByLookupTableAddress),
        );
        const signedTransaction = await signTransactionMessageWithSigners(transactionMessage);
        assertIsTransactionWithBlockhashLifetime(signedTransaction);
        const wireTransaction = getBase64EncodedWireTransaction(signedTransaction);
        return { signedTransaction, wireTransaction, transactionBytes: Buffer.from(wireTransaction, "base64").length };
    };

    /** Puts our + the flash loan's accounts in the keeper's table; Jupiter's are in Jupiter's own. */
    // A dry run never extends the table (no side effects; Slot Zero L-02): the transaction may
    // then be too large to simulate, which it reports. A full table throws LookupTableFullError,
    // and the keeper rotates to a fresh one.
    const withKeeperTable = async (ours: Instruction[], jupiterTables: Address[]) => {
        if (!lookupTable) return jupiterTables;
        if (!dryRun) {
            try {
                await ensureInLookupTable(client, liquidator, lookupTable, lookupCandidates(ours));
            } catch (err) {
                if (String(err).includes("is full")) throw new LookupTableFullError(lookupTable);
                throw err;
            }
        }
        return [lookupTable, ...jupiterTables];
    };

    /** Market price in program units (10^-12 USD per whole token) from a USDC-in amount. */
    const impliedPrice = (usdcIn: bigint) => (usdcIn * 1_000_000n * 10n ** BigInt(mint.decimals)) / position.amountBorrowed;
    const deviates = (marketPriceUsd: bigint) =>
        oracle.kind === "custom" && priceUsd * 10_000n > marketPriceUsd * BigInt(10_000 + maxPriceDeviationBps);

    /**
     * A USDC -> token quote that delivers at least amount_borrowed:
     * - ExactOut where Jupiter has such a route: exactly amount_borrowed; the swap may spend up to
     *   otherAmountThreshold (the worst case after slippage), the estimate is inAmount.
     * - Otherwise ExactIn (e.g. Pump.fun AMM tokens have no ExactOut route): a USDC amount sized
     *   so even the worst fill covers amount_borrowed (exactInBuyback.ts). It spends exactly
     *   inAmount; the surplus tokens stay in the keeper's token account. Its expected cost of
     *   amount_borrowed tokens is inAmount scaled by amount_borrowed / outAmount.
     * The mode is decided once per liquidation, from an uncapped ExactOut quote.
     */
    let buyBackMode: "exactOut" | "exactIn" | undefined;
    let sellEstimateUsdc: bigint | undefined;
    const quoteUrl = (inputMint: Address, outputMint: Address, amount: bigint, swapMode: string, cap: number | undefined) =>
        `${JUPITER_SWAP_API}/quote?inputMint=${inputMint}&outputMint=${outputMint}` +
        `&amount=${amount}&swapMode=${swapMode}&slippageBps=${slippageBps}` +
        (cap ? `&maxAccounts=${cap}` : "");
    const buyBackQuote = async (cap: number | undefined): Promise<{ quoteResponse: any; maxInUsdc: bigint; expectedInUsdc: bigint }> => {
        if (buyBackMode === undefined) {
            try {
                const exactOut = await jupiterJson(quoteUrl(USDC_MAINNET_MINT, tokenMint, position.amountBorrowed, "ExactOut", undefined), jupiterApiKey);
                buyBackMode = "exactOut";
                if (cap === undefined) {
                    return { quoteResponse: exactOut, maxInUsdc: BigInt(exactOut.otherAmountThreshold), expectedInUsdc: BigInt(exactOut.inAmount) };
                }
            } catch (err) {
                if (!isNoRouteError(err)) throw err;
                buyBackMode = "exactIn";
            }
        }
        if (buyBackMode === "exactOut") {
            const q = await jupiterJson(quoteUrl(USDC_MAINNET_MINT, tokenMint, position.amountBorrowed, "ExactOut", cap), jupiterApiKey);
            return { quoteResponse: q, maxInUsdc: BigInt(q.otherAmountThreshold), expectedInUsdc: BigInt(q.inAmount) };
        }
        // First guess: what selling amount_borrowed would fetch (the bid); sized up from there.
        sellEstimateUsdc ??= BigInt(
            (await jupiterJson(quoteUrl(tokenMint, USDC_MAINNET_MINT, position.amountBorrowed, "ExactIn", undefined), jupiterApiKey)).outAmount,
        );
        const { quote: q, usdcIn } = await sizeExactInBuyback(position.amountBorrowed, sellEstimateUsdc, (amount) =>
            jupiterJson(quoteUrl(USDC_MAINNET_MINT, tokenMint, amount, "ExactIn", cap), jupiterApiKey),
        );
        return { quoteResponse: q, maxInUsdc: usdcIn, expectedInUsdc: (usdcIn * position.amountBorrowed) / BigInt(q.outAmount) };
    };

    let compiled: Awaited<ReturnType<typeof compile>>;

    if (plan.path === "write_off") {
        // Reference price (Slot Zero M-01): a write-off pays the staker all the collateral, so a
        // signed price far above the market shouldn't trigger one. No route = can't check; go on.
        if (oracle.kind === "custom") {
            const reference = await buyBackQuote(undefined).catch(() => null);
            if (reference) {
                const marketPriceUsd = impliedPrice(reference.expectedInUsdc);
                if (deviates(marketPriceUsd)) return { status: "skipped_price_deviation", plan, signedPriceUsd: priceUsd, marketPriceUsd };
            }
        }
        // No tokens change hands, but the program still requires both liquidator accounts.
        const liquidatorTokenAccount = await ata(liquidator.address, tokenMint);
        const instructions = [
            ...computeBudgetIxs,
            ...(await createAtasIfMissing([
                [liquidator.address, USDC_MAINNET_MINT, liquidatorUsdcAccount],
                [liquidator.address, tokenMint, liquidatorTokenAccount],
            ])),
            ...payoutAtaIxs,
            ...getOraclePrelude(oracle),
            liquidatePositionIx,
        ];
        compiled = await compile(instructions, await withKeeperTable(instructions, []));
    } else {
        // Warm-up: the keeper's own USDC and token ATAs are created ONCE, in a separate small
        // transaction - creating them inside the liquidation costs ~70 bytes it can't spare
        // (a fresh keeper's first POPCAT liquidation measured 1239 of 1232 bytes).
        const liquidatorTokenAccount = await ata(liquidator.address, tokenMint);
        const warmup = await createAtasIfMissing([
            [liquidator.address, USDC_MAINNET_MINT, liquidatorUsdcAccount],
            [liquidator.address, tokenMint, liquidatorTokenAccount],
        ]);
        if (warmup.length > 0 && !dryRun) await sendIxs(client, liquidator, warmup);
        const liquidatorAtasExist = warmup.length === 0 || !dryRun;

        // Route size varies per quote (1137 vs 1381 bytes seen for the same POPCAT position), so
        // re-quote with tighter account caps until the transaction fits.
        const routeCaps = maxAccounts ? [maxAccounts] : [undefined, 48, 36, 28, 20];
        let fitting: typeof compiled | undefined;
        let smallest = Infinity;
        for (const cap of routeCaps) {
            // `maxInUsdc` is what we must flash-borrow (the most the swap may spend), `expectedInUsdc`
            // the expected cost of amount_borrowed tokens (for the price-deviation check).
            const { quoteResponse, maxInUsdc, expectedInUsdc } = await buyBackQuote(cap);
            const marketPriceUsd = impliedPrice(expectedInUsdc);
            if (deviates(marketPriceUsd)) return { status: "skipped_price_deviation", plan, signedPriceUsd: priceUsd, marketPriceUsd };

            const flash = await getFlashLoanInstructions({
                rpcUrl,
                signer: liquidator.address,
                asset: USDC_MAINNET_MINT,
                amount: maxInUsdc,
            });

            // A tighter route can be pricier, so profit is re-checked for every quote. Against the
            // WORST case the swap may fill at (otherAmountThreshold), not the estimate: a fill
            // within slippage would otherwise be paid from the keeper's own USDC (Slot Zero I-03).
            const expectedProfitUsdc = expectedBuybackProfit(plan, maxInUsdc, flash.fee);
            if (expectedProfitUsdc <= minProfitUsdc) {
                return { status: "skipped_unprofitable", plan, expectedProfitUsdc };
            }

            const swap = await jupiterJson(`${JUPITER_SWAP_API}/swap-instructions`, jupiterApiKey, {
                method: "POST",
                body: JSON.stringify({ quoteResponse, userPublicKey: liquidator.address, wrapAndUnwrapSol: false }),
            });
            assertSafeJupiterInstructions([
                ...((swap.setupInstructions ?? []) as JupiterInstruction[]),
                swap.swapInstruction as JupiterInstruction,
                ...(swap.cleanupInstruction ? [swap.cleanupInstruction as JupiterInstruction] : []),
                ...((swap.otherInstructions ?? []) as JupiterInstruction[]),
            ]);
            // Jupiter always adds a create-ATA for the output token; after the warm-up it's a
            // no-op that only costs bytes (the ATA program id alone is 32), so drop it.
            const jupiterSetup = ((swap.setupInstructions ?? []) as JupiterInstruction[]).filter(
                (ix) => !(liquidatorAtasExist && ix.programId === ASSOCIATED_TOKEN_PROGRAM_ADDRESS && ix.accounts[1]?.pubkey === liquidatorTokenAccount),
            );
            const jupiterIxs = [
                ...jupiterSetup.map(toKitInstruction),
                toKitInstruction(swap.swapInstruction), // USDC -> at least amount_borrowed tokens
                ...(swap.cleanupInstruction ? [toKitInstruction(swap.cleanupInstruction)] : []),
                ...(swap.otherInstructions ?? []).map(toKitInstruction),
            ];
            const ours = [
                ...computeBudgetIxs,
                ...flash.setupIxs, // liquidator's USDC ATA, if missing
                ...payoutAtaIxs,
                flash.borrowIx, // +maxInUsdc USDC
            ];
            const settle = [
                ...getOraclePrelude(oracle), // must be DIRECTLY before liquidate_position
                liquidatePositionIx, // tokens -> vault, notional + bounty USDC -> us
                flash.paybackIx, // -maxInUsdc (+fee) USDC, or the whole tx reverts
                ...flash.cleanupIxs,
            ];
            const tables = await withKeeperTable([...ours, ...settle], (swap.addressLookupTableAddresses ?? []).map(address));
            const candidate = await compile([...ours, ...jupiterIxs, ...settle], tables);
            if (candidate.transactionBytes <= MAX_TRANSACTION_BYTES) {
                fitting = candidate;
                break;
            }
            smallest = Math.min(smallest, candidate.transactionBytes);
        }
        if (!fitting) {
            return { status: "transaction_too_large", plan, transactionBytes: smallest, needsLookupTable: !lookupTable };
        }
        compiled = fitting;
    }

    const { signedTransaction, wireTransaction, transactionBytes } = compiled;
    if (transactionBytes > MAX_TRANSACTION_BYTES) {
        return { status: "transaction_too_large", plan, transactionBytes, needsLookupTable: !lookupTable };
    }

    // Simulate first: a revert costs fees on chain, a failed simulation costs nothing.
    const { value: simulation } = await client.rpc
        .simulateTransaction(wireTransaction, {
            encoding: "base64",
            commitment: "confirmed",
        })
        .send();
    const logs = simulation.logs ?? [];
    if (simulation.err) {
        return { status: "simulation_failed", plan, error: simulation.err, logs };
    }
    if (dryRun) {
        return { status: "simulated", plan, unitsConsumed: simulation.unitsConsumed, logs, transactionBytes };
    }

    await sendAndConfirmTransactionFactory(client)(signedTransaction, { commitment: "confirmed" });
    const signature = getSignatureFromTransaction(signedTransaction);
    console.log(`Position ${plan.path === "write_off" ? "written off" : "liquidated"}! TX: https://solscan.io/tx/${signature}`);
    return { status: "sent", plan, signature, unitsConsumed: simulation.unitsConsumed, transactionBytes };
}
