import {
    type Address,
    type TransactionSigner,
    pipe,
    createTransactionMessage,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    appendTransactionMessageInstructions,
    signTransactionMessageWithSigners,
    sendAndConfirmTransactionFactory,
    getSignatureFromTransaction,
} from '@solana/kit';
import { assertIsTransactionWithBlockhashLifetime } from "@solana/transactions";
import { getCreateAssociatedTokenIdempotentInstructionAsync } from "@solana-program/token";
// 1. Import the generated builder that Codama created for you!
import { getDepositStakeInstructionAsync } from '../../../clients/js/src/generated/instructions/depositStake';
import { type OracleSelection, type SolanaClient, getOraclePrelude } from './OpenBorrowPosition';

export async function executeDeposit(
    client: SolanaClient,
    staker: TransactionSigner<Address>,
    tokenMint: Address,
    amountToStake: bigint,
    interestRateBps: number,
    maxExpiryDays: number,
    offerId: bigint,
    // deposit_stake always needs exactly one real price source (picked by the program based on
    // whether token_mint is Pyth-supported) - there's no valid "none" here, unlike borrow/close.
    oracle: Exclude<OracleSelection, { kind: "none" }>,
) {
    // deposit_stake's get_current_price needs the price source the program picks for token_mint:
    // the pyth_price_feed account for Pyth mints, or (custom) a signed-price Ed25519 instruction
    // directly before deposit_stake. For a brand-new custom mint, the backend only signs a price
    // after its Rugcheck/median checks pass, so this doubles as the listing gate.
    const oracleAccounts: { pythPriceFeed?: Address } =
        oracle.kind === "pyth" ? { pythPriceFeed: oracle.priceFeed } : {};

    // 4. Build the instruction using Codama
    // Notice how you don't need to pass the market_vault PDA, vault_token_account PDA, 
    // or system programs. Codama figures them out automatically from the IDL seeds!
    const depositStakeIx = await getDepositStakeInstructionAsync({
        staker,
        tokenMint,
        ...oracleAccounts,
        amountToStake,
        interestRateBps,
        maxExpiryDays,
        offerId,
    });

    // Unlike open_borrow_position's borrower ATAs, staker_token_account is a plain `mut` account
    // on the Rust side (not `init`/`init_if_needed`) - it must already exist before this runs.
    const createStakerAtaIx = await getCreateAssociatedTokenIdempotentInstructionAsync({
        payer: staker,
        owner: staker.address,
        mint: tokenMint,
    });

    // 5. Fetch the latest blockhash required for the transaction
    const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();

    // 6. Build the modern Kit TransactionMessage
    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(staker, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        (m) => appendTransactionMessageInstructions([createStakerAtaIx, ...getOraclePrelude(oracle), depositStakeIx], m),
    );

    // 7. Compile, Sign, and Send
    const signedTransaction = await signTransactionMessageWithSigners(message);
    assertIsTransactionWithBlockhashLifetime(signedTransaction);

    await sendAndConfirmTransactionFactory(client)(signedTransaction, { commitment: "confirmed" });

    const signature = getSignatureFromTransaction(signedTransaction);
    console.log(`Deposit confirmed! TX: https://solscan.io/tx/${signature}`);
    return signature;
}
