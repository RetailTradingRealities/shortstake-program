// Jupiter Lend flash loans, adapted to @solana/kit. The official SDK (@jup-ag/lend) only speaks
// @solana/web3.js 1.x, so it's used purely as an instruction builder here: its legacy
// TransactionInstructions are converted with @solana/compat, and nothing else in the app
// touches web3.js. Program: jupgfSgfuAXv4B6R2Uxu85Z1qdzgju79s6MfZekN6XS.
import { type Address, type Instruction } from "@solana/kit";
import { fromLegacyTransactionInstruction } from "@solana/compat";
import { Connection, PublicKey } from "@solana/web3.js";
import BN from "bn.js";
import { getFlashloanIx } from "@jup-ag/lend/flashloan";

export interface FlashLoanInstructions {
    /** Must come first: creates the borrower's ATA for `asset` if it doesn't exist yet. */
    setupIxs: Instruction[];
    /** Credits `amount` of `asset` to the signer's ATA. */
    borrowIx: Instruction;
    /** Takes `amount` + `fee` back from the signer's ATA - reverts the whole tx if short. */
    paybackIx: Instruction;
    cleanupIxs: Instruction[];
    /** Read live from Jupiter's flashloan admin account (bps of `amount`, rounded up). */
    fee: bigint;
}

/**
 * Borrow `amount` base units of `asset` for the duration of one transaction. Order them as
 * [...setupIxs, borrowIx, <your instructions>, paybackIx, ...cleanupIxs].
 *
 * `rpcUrl` is needed because the SDK reads the flashloan admin account (for the fee) and the
 * mint's token program through its own web3.js Connection.
 */
export async function getFlashLoanInstructions(params: {
    rpcUrl: string;
    signer: Address;
    asset: Address;
    amount: bigint;
}): Promise<FlashLoanInstructions> {
    const { borrowIx, paybackIx, setupIxs, cleanupIxs, feeAmount } = await getFlashloanIx({
        connection: new Connection(params.rpcUrl, "confirmed"),
        signer: new PublicKey(params.signer),
        asset: new PublicKey(params.asset),
        amount: new BN(params.amount.toString()),
        includeATASetup: true,
    });
    return {
        setupIxs: setupIxs.map(fromLegacyTransactionInstruction),
        borrowIx: fromLegacyTransactionInstruction(borrowIx),
        paybackIx: fromLegacyTransactionInstruction(paybackIx),
        cleanupIxs: cleanupIxs.map(fromLegacyTransactionInstruction),
        fee: BigInt(feeAmount.toString()),
    };
}
