import {
    type Address,
    address,
    type Instruction,
    type Rpc,
    type RpcSubscriptions,
    type SolanaRpcApi,
    type SolanaRpcSubscriptionsApi,
    type TransactionSigner,
    AccountRole,
    appendTransactionMessageInstructions,
    assertIsTransactionWithBlockhashLifetime,
    compressTransactionMessageUsingAddressLookupTables,
    createTransactionMessage,
    fetchAddressesForLookupTables,
    getSignatureFromTransaction,
    pipe,
    sendAndConfirmTransactionFactory,
    setTransactionMessageFeePayerSigner,
    setTransactionMessageLifetimeUsingBlockhash,
    signTransactionMessageWithSigners,
} from "@solana/kit";
import {
    TOKEN_PROGRAM_ADDRESS,
    findAssociatedTokenPda,
    getCreateAssociatedTokenIdempotentInstructionAsync,
} from "@solana-program/token";
import fetch from "cross-fetch"; // or native fetch if Node 18+

// Codama-generated client (clients/js) - discriminator/account order/PDA seeds come from here
// instead of being hand-derived, so they stay in sync with target/idl/short_stake001.json.
import {
    fetchLenderOffer,
    findLenderOfferPda,
    getOpenBorrowPositionInstructionAsync,
    SHORT_STAKE001_PROGRAM_ADDRESS,
} from "../../../clients/js/src/generated";
import { type SignedPrice, getSignedPriceInstruction } from "../oracle/signedPrice";

// Fixed by the program's `#[account(address = usdc_mainnet::ID)]` constraint - never variable per call.
const USDC_MAINNET_MINT = address("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
// Mirrors constants::FEE_COLLECTOR_PUBKEY of the MAINNET build (the Config Squad vault); not a `#[constant]`, so Codama doesn't export it.
const FEE_COLLECTOR_PUBKEY = address("52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr");

export interface SolanaClient {
    rpc: Rpc<SolanaRpcApi>;
    rpcSubscriptions: RpcSubscriptions<SolanaRpcSubscriptionsApi>;
}

// Which oracle mode the vault uses for this token mint. Exactly one applies at a time:
// Pyth is passed as the optional pyth_price_feed account; custom is a backend-signed price
// sent as an Ed25519 instruction directly before the program instruction (see oracle/signedPrice.ts).
export type OracleSelection =
    | { kind: "pyth"; priceFeed: Address }
    | { kind: "custom"; signedPrice: SignedPrice }
    | { kind: "none" };

// Ed25519 instruction(s) that must sit directly before the priced program instruction.
export function getOraclePrelude(oracle: OracleSelection): Instruction[] {
    return oracle.kind === "custom" ? [getSignedPriceInstruction(oracle.signedPrice)] : [];
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

/**
 * Opens a short-stake001 borrow position and, in the same atomic transaction, swaps the
 * freshly borrowed SPL tokens for USDC via Jupiter. Order of operations on-chain:
 *   1. Borrower's USDC collateral is pulled into the global USDC vault.
 *   2. Borrowed SPL tokens are transferred from the market vault to the borrower's wallet.
 *   3. The countdown to `expiry_timestamp` begins (BorrowPosition PDA is created).
 *   4. The borrowed SPL tokens are swapped for USDC, landing back in the borrower's USDC account.
 */
export async function executeAtomicBorrowAndSwap(
    client: SolanaClient,
    // A raw KeyPairSigner works (scripts/tests), but so does a wallet-connected signer
    // (e.g. from Wallet Standard) for browser/frontend usage.
    borrower: TransactionSigner<Address>,
    tokenMint: Address,
    stakerPubkey: Address,
    offerId: bigint,
    positionId: bigint,
    borrowAmount: bigint,
    providedCollateralUsdc: bigint,
    oracle: OracleSelection = { kind: "none" },
    slippageBps = 50,
    programAddress: Address = SHORT_STAKE001_PROGRAM_ADDRESS,
    /**
     * The offer's terms as the borrower saw them; the program refuses the open if the offer got
     * worse (Slot Zero M-02). Omitted: the offer's CURRENT on-chain terms, which gives no
     * protection against a switch - fine for scripts, never for a UI.
     */
    acceptedTerms?: { interestRateBps: number; maxExpiryDays: number },
) {
    // lenderOffer isn't auto-resolved by the async builder (its seeds depend on the caller-chosen
    // offerId), so it still has to be derived up front, same as the ATAs below.
    const [lenderOffer] = await findLenderOfferPda(
        { staker: stakerPubkey, tokenMint, offerId },
        { programAddress },
    );

    let terms: { interestRateBps: number; maxExpiryDays: number };
    if (acceptedTerms) {
        terms = acceptedTerms;
    } else {
        const { data } = await fetchLenderOffer(client.rpc, lenderOffer);
        terms = { interestRateBps: data.interestRateBps, maxExpiryDays: data.maxExpiryDays };
    }

    const [borrowerTokenAccount] = await findAssociatedTokenPda({
        owner: borrower.address,
        mint: tokenMint,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });
    const [borrowerUsdcAccount] = await findAssociatedTokenPda({
        owner: borrower.address,
        mint: USDC_MAINNET_MINT,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });
    // Receives the borrow fee (global_config.fee_rate_borrowers_bps of the notional), paid by the
    // borrower on top of providedCollateralUsdc - the borrower's USDC balance must cover both.
    const [feeCollectorUsdcAccount] = await findAssociatedTokenPda({
        owner: FEE_COLLECTOR_PUBKEY,
        mint: USDC_MAINNET_MINT,
        tokenProgram: TOKEN_PROGRAM_ADDRESS,
    });

    // Omitting pythPriceFeed entirely (not just setting it to `undefined` -
    // exactOptionalPropertyTypes rejects that) lets the generated builder point it at the
    // program id itself - Anchor's `Option<Account>` convention for "not provided".
    const oracleAccounts: { pythPriceFeed?: Address } =
        oracle.kind === "pyth" ? { pythPriceFeed: oracle.priceFeed } : {};

    // marketVault, vaultTokenAccount, globalUsdcVault, globalConfig, borrowPosition, usdcMint,
    // systemProgram and tokenProgram are all auto-derived/defaulted by the async builder.
    const borrowIx = await getOpenBorrowPositionInstructionAsync(
        {
            borrower,
            tokenMint,
            lenderOffer,
            borrowerTokenAccount,
            borrowerUsdcAccount,
            feeCollectorUsdcAccount,
            ...oracleAccounts,
            borrowAmount,
            positionId,
            providedCollateralUsdc,
            maxInterestRateBps: terms.interestRateBps,
            minExpiryDays: terms.maxExpiryDays,
        },
        { programAddress },
    );

    // Ensure the borrower's token/USDC ATAs exist before the borrow instruction reads them -
    // the program requires both as already-initialized `Account<TokenAccount>`, not `init`.
    const [createTokenAtaIx, createUsdcAtaIx] = await Promise.all([
        getCreateAssociatedTokenIdempotentInstructionAsync({
            payer: borrower,
            owner: borrower.address,
            mint: tokenMint,
        }),
        getCreateAssociatedTokenIdempotentInstructionAsync({
            payer: borrower,
            owner: borrower.address,
            mint: USDC_MAINNET_MINT,
        }),
    ]);

    // Fetch Jupiter Swap Quote (SPL Token -> USDC) for the amount about to be borrowed
    const quoteResponse = await fetch(
        `https://quote-api.jup.ag/v6/quote?inputMint=${tokenMint}&outputMint=${USDC_MAINNET_MINT}&amount=${borrowAmount}&slippageBps=${slippageBps}`,
    ).then((res) => res.json());

    // Get the serialized Jupiter Swap Instructions
    const swapInstructionsResponse = await fetch("https://quote-api.jup.ag/v6/swap-instructions", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({
            quoteResponse,
            userPublicKey: borrower.address,
        }),
    }).then((res) => res.json());

    if (swapInstructionsResponse.error) {
        throw new Error(`Jupiter swap-instructions error: ${swapInstructionsResponse.error}`);
    }

    // Include Jupiter's compute budget/setup/cleanup instructions too, not just the swap itself -
    // dropping them silently breaks routes that need extra CU or wrap/unwrap/setup steps.
    const jupiterInstructions: Instruction[] = [
        ...(swapInstructionsResponse.computeBudgetInstructions ?? []).map(toKitInstruction),
        ...(swapInstructionsResponse.setupInstructions ?? []).map(toKitInstruction),
        toKitInstruction(swapInstructionsResponse.swapInstruction),
        ...(swapInstructionsResponse.cleanupInstruction
            ? [toKitInstruction(swapInstructionsResponse.cleanupInstruction)]
            : []),
    ];

    const lookupTableAddresses: Address[] = (swapInstructionsResponse.addressLookupTableAddresses ?? []).map(
        address,
    );

    // Bundle everything into an atomic Versioned Transaction: borrow first, then swap.
    const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();

    // Address Lookup Tables must actually be applied, otherwise large Jupiter routes can
    // exceed the transaction size limit despite the accounts being eligible for compression.
    // (An empty lookup table list is a safe no-op here.)
    const addressesByLookupTableAddress = await fetchAddressesForLookupTables(lookupTableAddresses, client.rpc);

    const transactionMessage = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(borrower, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        (m) =>
            appendTransactionMessageInstructions(
                [
                    createTokenAtaIx,
                    createUsdcAtaIx,
                    ...getOraclePrelude(oracle),
                    borrowIx, // Step A: Secure the loan & pull collateral
                    ...jupiterInstructions, // Step B: Dump the borrowed token for USDC instantly
                ],
                m,
            ),
        (m) => compressTransactionMessageUsingAddressLookupTables(m, addressesByLookupTableAddress),
    );

    const signedTransaction = await signTransactionMessageWithSigners(transactionMessage);
    assertIsTransactionWithBlockhashLifetime(signedTransaction);

    await sendAndConfirmTransactionFactory(client)(signedTransaction, { commitment: "confirmed" });

    const signature = getSignatureFromTransaction(signedTransaction);
    console.log(`Borrow & Swap executed atomically! TX: https://solscan.io/tx/${signature}`);
    return signature;
}