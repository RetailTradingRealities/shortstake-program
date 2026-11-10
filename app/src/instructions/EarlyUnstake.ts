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

import { getEarlyUnstakeInstructionAsync } from '../../../clients/js/src/generated/instructions/earlyUnstake';
import { findLenderOfferPda, findMarketVaultPda } from '../../../clients/js/src/generated/pdas';
import { fetchMarketVault, fetchMaybeLenderOffer } from '../../../clients/js/src/generated/accounts';
import { type SolanaClient } from './OpenBorrowPosition';

/**
 * Withdraws every token of the offer that isn't lent out. With no loans open that is the whole
 * offer, and the offer account is closed. With loans open the offer stays, closed to new
 * borrowing; each loan's tokens return to it as the loan settles, and calling this again
 * collects them (the call that finds no loans open closes the offer).
 */
export async function executeEarlyUnstake(
    client: SolanaClient,
    staker: TransactionSigner<Address>,
    tokenMint: Address,
    offerId: bigint,
) {
    // lenderOffer isn't auto-resolved by the async builder (depends on the caller-chosen offerId).
    const [lenderOffer] = await findLenderOfferPda({ staker: staker.address, tokenMint, offerId });
    const maybeOffer = await fetchMaybeLenderOffer(client.rpc, lenderOffer);
    if (!maybeOffer.exists) throw new Error(`Offer #${offerId} doesn't exist: never staked, or already fully unstaked.`);
    const offer = maybeOffer.data;
    const openLoans = offer.currentActiveBorrows;
    if (offer.availableToBorrow === 0n && openLoans > 0) {
        // The program would reject it (NothingToWithdraw) - don't pay a fee to find out.
        throw new Error(`Offer #${offerId}: all ${offer.totalOffered} tokens are lent out (${openLoans} open loan(s)); try again after a loan closes.`);
    }

    // payerWallet must equal market_vault.payer (rent-refund destination on full market drain) -
    // not necessarily the staker themselves, so it has to be read off the on-chain MarketVault.
    const [marketVault] = await findMarketVaultPda({ tokenMint });
    const { data: marketVaultData } = await fetchMarketVault(client.rpc, marketVault);

    // marketVault, vaultTokenAccount, stakerTokenAccount and tokenProgram are all
    // auto-derived/defaulted by the async builder.
    const earlyUnstakeIx = await getEarlyUnstakeInstructionAsync({
        staker,
        payerWallet: marketVaultData.payer,
        tokenMint,
        lenderOffer,
    });

    const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();

    const message = pipe(
        createTransactionMessage({ version: 0 }),
        (m) => setTransactionMessageFeePayerSigner(staker, m),
        (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
        (m) => appendTransactionMessageInstructions([earlyUnstakeIx], m),
    );

    const signedTransaction = await signTransactionMessageWithSigners(message);
    assertIsTransactionWithBlockhashLifetime(signedTransaction);

    await sendAndConfirmTransactionFactory(client)(signedTransaction, { commitment: "confirmed" });

    const signature = getSignatureFromTransaction(signedTransaction);
    console.log(`Early unstake confirmed! TX: https://solscan.io/tx/${signature}`);
    if (openLoans > 0) {
        console.log(`Withdrew ${offer.availableToBorrow} tokens. ${offer.totalOffered - offer.availableToBorrow} are still lent out across ${openLoans} loan(s); the offer is closed to new borrowing - unstake again as they settle.`);
    } else {
        console.log(`Withdrew ${offer.availableToBorrow} tokens; the offer is closed.`);
    }
    return signature;
}
