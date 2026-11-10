// The keeper's own Address Lookup Table. A flash-loan liquidation carries ~35 accounts outside
// Jupiter's tables (liquidate_position's, the flash loan's, payout ATAs) plus the Ed25519 price -
// as full 32-byte keys that's well over the 1232-byte transaction limit. Each address stored here
// costs 1 byte in the transaction instead of 32.
//
// Global/market accounts are added once; a position's own accounts are added just before it's
// liquidated. New entries only become usable after the slot they were added in, so
// `ensureInLookupTable` waits for that. Rent (~0.00022 SOL per address) is recoverable by
// deactivating and closing the table later.
import {
    type Address,
    type Instruction,
    type KeyPairSigner,
    type TransactionSigner,
    AccountRole,
    appendTransactionMessageInstructions,
    assertIsTransactionWithBlockhashLifetime,
    createTransactionMessage,
    pipe,
    sendAndConfirmTransactionFactory,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    signTransactionMessageWithSigners,
} from "@solana/kit";
import {
    fetchAddressLookupTable,
    getCreateLookupTableInstructionAsync,
    getExtendLookupTableInstruction,
} from "@solana-program/address-lookup-table";
import { type SolanaClient } from "./OpenBorrowPosition";

/** A lookup table holds at most 256 addresses. */
const MAX_TABLE_ADDRESSES = 256;
/** Addresses per extend transaction - 20 * 32 bytes keeps it well under the size limit. */
const EXTEND_CHUNK = 20;

/** Sends a small helper transaction (table create/extend, ATA warm-up) and waits for confirmation. */
export async function sendIxs(client: SolanaClient, payer: TransactionSigner, instructions: Instruction[]) {
    const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(payer, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        (m) => appendTransactionMessageInstructions(instructions, m),
    );
    const signed = await signTransactionMessageWithSigners(message);
    assertIsTransactionWithBlockhashLifetime(signed);
    await sendAndConfirmTransactionFactory(client)(signed, { commitment: "confirmed" });
}

/** Creates a new lookup table owned by `authority` (who also pays). Returns its address. */
/**
 * How far behind the finalized slot to pick the table's "recent slot". The program checks it
 * against the validator's SlotHashes; behind a load balancer (Helius), the node that simulates
 * can lag the node that answered getSlot, and then rejects the slot as "not a recent slot"
 * (InvalidInstructionData). Every node has seen a slot a few seconds old; SlotHashes covers the
 * last 512 slots (~3 minutes), so this stays well inside it.
 */
const RECENT_SLOT_MARGIN = 20n;
const CREATE_ATTEMPTS = 4;

export async function createKeeperLookupTable(client: SolanaClient, authority: KeyPairSigner | TransactionSigner): Promise<Address> {
    let lastError: unknown;
    for (let attempt = 1; attempt <= CREATE_ATTEMPTS; attempt++) {
        const finalized = await client.rpc.getSlot({ commitment: "finalized" }).send();
        const recentSlot = finalized - RECENT_SLOT_MARGIN;
        const ix = await getCreateLookupTableInstructionAsync({ authority: authority.address, payer: authority, recentSlot });
        try {
            await sendIxs(client, authority, [ix]);
            return ix.accounts[0].address;
        } catch (err) {
            lastError = err;
            if (attempt < CREATE_ATTEMPTS) await new Promise((resolve) => setTimeout(resolve, 2_000));
        }
    }
    throw new Error(`couldn't create the keeper lookup table after ${CREATE_ATTEMPTS} attempts: ${describeSendError(lastError)}`);
}

/** The error message plus the simulation logs, if the RPC returned them (preflight failure). */
export function describeSendError(err: unknown): string {
    const context = (err as { context?: { logs?: unknown; err?: unknown } })?.context;
    const logs = Array.isArray(context?.logs) ? (context.logs as string[]).slice(-4).join(" | ") : "";
    const reason = context?.err !== undefined ? ` ${JSON.stringify(context.err, (_k, v) => (typeof v === "bigint" ? v.toString() : v))}` : "";
    return `${err instanceof Error ? err.message : String(err)}${reason}${logs ? ` - logs: ${logs}` : ""}`;
}

/**
 * Accounts worth putting in the table for `instructions`: everything that isn't a signer and
 * isn't an invoked program (both must stay as static keys in a v0 transaction).
 */
export function lookupCandidates(instructions: readonly Instruction[]): Address[] {
    const programs = new Set(instructions.map((ix) => ix.programAddress));
    const out = new Set<Address>();
    for (const ix of instructions) {
        for (const meta of ix.accounts ?? []) {
            const isSigner = meta.role === AccountRole.READONLY_SIGNER || meta.role === AccountRole.WRITABLE_SIGNER;
            if (!isSigner && !programs.has(meta.address)) out.add(meta.address);
        }
    }
    return [...out];
}

/**
 * Adds whichever of `addresses` are missing from `table`, then waits until they're usable.
 * Throws if the table would overflow - rotate to a fresh table (createKeeperLookupTable) then.
 */
export async function ensureInLookupTable(
    client: SolanaClient,
    authority: TransactionSigner,
    table: Address,
    addresses: readonly Address[],
): Promise<void> {
    const { data } = await fetchAddressLookupTable(client.rpc, table);
    const present = new Set(data.addresses);
    const missing = [...new Set(addresses)].filter((a) => !present.has(a));
    if (missing.length === 0) return;
    if (data.addresses.length + missing.length > MAX_TABLE_ADDRESSES) {
        throw new Error(`lookup table ${table} is full (${data.addresses.length}/${MAX_TABLE_ADDRESSES}): create a new one`);
    }
    for (let i = 0; i < missing.length; i += EXTEND_CHUNK) {
        await sendIxs(client, authority, [
            getExtendLookupTableInstruction({
                address: table,
                authority,
                payer: authority,
                addresses: missing.slice(i, i + EXTEND_CHUNK),
            }),
        ]);
    }
    // Entries added in slot N can be used from slot N+1 on.
    const { data: extended } = await fetchAddressLookupTable(client.rpc, table);
    for (;;) {
        const slot = await client.rpc.getSlot({ commitment: "confirmed" }).send();
        if (slot > extended.lastExtendedSlot) return;
        await new Promise((resolve) => setTimeout(resolve, 200));
    }
}
