// Where liquidate_position pays a counterparty (staker USDC, staker token, borrower USDC).
//
// The program accepts ANY token account the counterparty owns (Slot Zero H-01): an owner can
// hand its ATA to another key with SetAuthority, and an ATA-only rule let it block settlement.
// This picks, in order:
//   1. its ATA, if it exists, still belongs to the owner and isn't frozen;
//   2. its ATA, created in the liquidation transaction, if it doesn't exist yet;
//   3. another usable account the owner holds for that mint (ATA hijacked or frozen);
//   4. a FRESH account owned by the owner, created in its own small transaction first (a
//      liquidation is already close to the 1,232-byte limit). Paid by the liquidator, about
//      0.002 SOL, and found by step 3 on every later attempt, so it's paid once per account.

import { type Address, type Instruction, type TransactionSigner, generateKeyPairSigner } from "@solana/kit";
import { getCreateAccountInstruction } from "@solana-program/system";
import {
    AccountState,
    TOKEN_PROGRAM_ADDRESS,
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
    getInitializeAccount3Instruction,
    getTokenDecoder,
    getTokenSize,
} from "@solana-program/token";
import type { SolanaClient } from "./OpenBorrowPosition";

export type PayoutAccount = {
    address: Address;
    /** Create-ATA instruction to put in the liquidation transaction itself (case 2). */
    createInline?: Instruction;
    /** Instructions for a separate transaction that must land first (case 4). */
    createSeparately?: Instruction[];
};

type RawAccount = { owner: Address; data: readonly [string, string] | [string, string] };

/** True if the raw account is an initialized, unfrozen SPL token account of `owner` for `mint`. */
export function isUsableTokenAccount(account: RawAccount, owner: Address, mint: Address): boolean {
    if (account.owner !== TOKEN_PROGRAM_ADDRESS) return false;
    try {
        const token = getTokenDecoder().decode(Buffer.from(account.data[0], "base64"));
        return token.owner === owner && token.mint === mint && token.state === AccountState.Initialized;
    } catch {
        return false;
    }
}

export async function resolvePayoutAccount(
    client: SolanaClient,
    payer: TransactionSigner<Address>,
    owner: Address,
    mint: Address,
): Promise<PayoutAccount> {
    const [ata] = await findAssociatedTokenPda({ owner, mint, tokenProgram: TOKEN_PROGRAM_ADDRESS });
    const { value } = await client.rpc.getAccountInfo(ata, { encoding: "base64" }).send();
    if (value === null) {
        return { address: ata, createInline: await getCreateAssociatedTokenIdempotentInstructionAsync({ payer, owner, mint }) };
    }
    if (isUsableTokenAccount(value, owner, mint)) return { address: ata };

    const { value: held } = await client.rpc.getTokenAccountsByOwner(owner, { mint }, { encoding: "base64" }).send();
    const usable = held.find((entry) => entry.pubkey !== ata && isUsableTokenAccount(entry.account, owner, mint));
    if (usable) return { address: usable.pubkey };

    const account = await generateKeyPairSigner();
    const space = getTokenSize();
    const lamports = await client.rpc.getMinimumBalanceForRentExemption(BigInt(space)).send();
    return {
        address: account.address,
        createSeparately: [
            getCreateAccountInstruction({ payer, newAccount: account, lamports, space, programAddress: TOKEN_PROGRAM_ADDRESS }),
            getInitializeAccount3Instruction({ account: account.address, mint, owner }),
        ],
    };
}
