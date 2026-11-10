import {
  type Address,
  type TransactionSigner,
  address,
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
import { TOKEN_PROGRAM_ADDRESS, findAssociatedTokenPda } from '@solana-program/token';

import { getClosePositionInstructionAsync } from '../../../clients/js/src/generated/instructions/closePosition';
import { findBorrowPositionPda, findLenderOfferPda } from '../../../clients/js/src/generated/pdas';
import { type OracleSelection, type SolanaClient, getOraclePrelude } from './OpenBorrowPosition';

// Fixed by the program's `#[account(address = usdc_mainnet::ID)]` constraint - never variable per call.
const USDC_MAINNET_MINT = address("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v");
// Mirrors constants::FEE_COLLECTOR_PUBKEY of the MAINNET build (the Config Squad vault); not a `#[constant]`, so Codama doesn't export it.
const FEE_COLLECTOR_PUBKEY = address("52g8arY1QG7wQkAb1N9hPJ5UVs8Kqfo2wNVZbPrZ8Lfr");

export async function executeClosePosition(
  client: SolanaClient,
  borrower: TransactionSigner<Address>,
  tokenMint: Address,
  stakerPubkey: Address,
  offerId: bigint,
  positionId: bigint,
  // close_position always needs exactly one real price source (picked by the program based on
  // market_vault.expected_oracle_feed) - there's no valid "none" here, same as deposit_stake.
  oracle: Exclude<OracleSelection, { kind: "none" }>,
) {
  // lenderOffer/borrowPosition aren't auto-resolved by the async builder (depend on
  // caller-chosen offerId/positionId), so they have to be derived up front.
  const [lenderOffer] = await findLenderOfferPda({ staker: stakerPubkey, tokenMint, offerId });
  const [borrowPosition] = await findBorrowPositionPda({ borrower: borrower.address, positionId });


  const [stakerUsdcAccount] = await findAssociatedTokenPda({
    owner: stakerPubkey,
    mint: USDC_MAINNET_MINT,
    tokenProgram: TOKEN_PROGRAM_ADDRESS,
  });
  const [borrowerUsdcAccount] = await findAssociatedTokenPda({
    owner: borrower.address,
    mint: USDC_MAINNET_MINT,
    tokenProgram: TOKEN_PROGRAM_ADDRESS,
  });
  // Receives the protocol's cut of the interest (global_config.fee_rate_stakers_bps).
  const [feeCollectorUsdcAccount] = await findAssociatedTokenPda({
    owner: FEE_COLLECTOR_PUBKEY,
    mint: USDC_MAINNET_MINT,
    tokenProgram: TOKEN_PROGRAM_ADDRESS,
  });

  // get_current_price needs the source chosen by market_vault.expected_oracle_feed: the
  // pyth_price_feed account, or a signed-price Ed25519 instruction directly before close_position.
  const oracleAccounts: { pythPriceFeed?: Address } =
    oracle.kind === "pyth" ? { pythPriceFeed: oracle.priceFeed } : {};

  // usdcMint, borrowerTokenAccount, marketVault, vaultTokenAccount, globalUsdcVault,
  // globalConfig, systemProgram and tokenProgram are all auto-derived/defaulted by the
  // async builder.
  const closePositionIx = await getClosePositionInstructionAsync({
    borrower,
    tokenMint,
    stakerUsdcAccount,
    borrowerUsdcAccount,
    feeCollectorUsdcAccount,
    lenderOffer,
    borrowPosition,
    ...oracleAccounts,
  });

  const { value: latestBlockhash } = await client.rpc.getLatestBlockhash().send();

  const message = pipe(
    createTransactionMessage({ version: 0 }),
    (m) => setTransactionMessageFeePayerSigner(borrower, m),
    (m) => setTransactionMessageLifetimeUsingBlockhash(latestBlockhash, m),
    (m) => appendTransactionMessageInstructions([...getOraclePrelude(oracle), closePositionIx], m),
  );

  const signedTransaction = await signTransactionMessageWithSigners(message);
  assertIsTransactionWithBlockhashLifetime(signedTransaction);

  await sendAndConfirmTransactionFactory(client)(signedTransaction, { commitment: 'confirmed' });

  const signature = getSignatureFromTransaction(signedTransaction);
  console.log(`Position closed! TX: https://solscan.io/tx/${signature}`);
  return signature;
}
