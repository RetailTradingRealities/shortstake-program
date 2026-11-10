import {
    type Address,
    type Instruction,
    type KeyPairSigner,
    type SignatureBytes,
    address,
    getAddressEncoder,
    getI64Encoder,
    getU16Encoder,
    getU64Encoder,
    signBytes,
} from "@solana/kit";
import { SHORT_STAKE001_PROGRAM_ADDRESS } from "../../../clients/js/src/generated/programs";

// Custom-oracle prices are signed off-chain by global_config.oracle_signer and verified on-chain
// through an Ed25519 precompile instruction that must sit DIRECTLY BEFORE the deposit_stake /
// open_borrow_position / close_position / liquidate_position instruction it prices.
// Mirrors programs/short-stake001/src/signed_price.rs - keep the two in sync.

export const ED25519_PROGRAM_ADDRESS = address("Ed25519SigVerify111111111111111111111111111");

// The program rejects prices older than this (or more than 10s in the future), measured
// against the cluster clock, so fetch a fresh signed price right before building each tx.
export const MAX_SIGNED_PRICE_AGE_SECONDS = 60n;

const SIGNED_PRICE_MESSAGE_LEN = 32 + 32 + 8 + 8 + 1;

// Prices are integer counts of 10^-12 USD per whole token ($1.00 = 1_000_000_000_000n), the
// same unit the program's Pyth path normalizes to (constants.rs PRICE_DECIMALS).
export const PRICE_DECIMALS = 12;

// Which instruction a price was signed for. The backend applies different sourcing rules per
// action, and the program rejects a price signed for any other action.
export const PriceAction = {
    DepositStake: 1,
    OpenBorrowPosition: 2,
    ClosePosition: 3,
    LiquidatePosition: 4,
} as const;
export type PriceAction = (typeof PriceAction)[keyof typeof PriceAction];
const ED25519_DATA_START = 2 + 14; // [num_signatures, padding] + one offsets struct
const PUBKEY_OFFSET = ED25519_DATA_START;
const SIGNATURE_OFFSET = PUBKEY_OFFSET + 32;
const MESSAGE_OFFSET = SIGNATURE_OFFSET + 64;
// "Data is inside this same Ed25519 instruction" - the program requires this for all three.
const CURRENT_INSTRUCTION = 0xffff;

export interface SignedPrice {
    tokenMint: Address;
    /** 10^-12 USD per whole token (PRICE_DECIMALS), same unit as the Pyth path. */
    priceUsd: bigint;
    /** Unix seconds when the backend produced the price. */
    publishTime: bigint;
    /** The only instruction this price is valid for. */
    action: PriceAction;
    /** Must equal global_config.oracle_signer. */
    signer: Address;
    signature: SignatureBytes;
}

/** program_id || token_mint || price (u64 LE) || publish_time (i64 LE) || action (u8) */
export function getSignedPriceMessage(
    tokenMint: Address,
    priceUsd: bigint,
    publishTime: bigint,
    action: PriceAction,
    programAddress: Address = SHORT_STAKE001_PROGRAM_ADDRESS,
): Uint8Array {
    const addressEncoder = getAddressEncoder();
    const message = new Uint8Array(SIGNED_PRICE_MESSAGE_LEN);
    message.set(addressEncoder.encode(programAddress), 0);
    message.set(addressEncoder.encode(tokenMint), 32);
    message.set(getU64Encoder().encode(priceUsd), 64);
    message.set(getI64Encoder().encode(publishTime), 72);
    message[80] = action;
    return message;
}

/**
 * BACKEND ONLY - signs a price with the oracle signer's keypair. The keypair must never be
 * shipped to the browser; the frontend should request a SignedPrice from the backend instead.
 */
export async function signPrice(
    oracleSigner: KeyPairSigner,
    tokenMint: Address,
    priceUsd: bigint,
    action: PriceAction,
    publishTime: bigint = BigInt(Math.floor(Date.now() / 1000)),
    programAddress: Address = SHORT_STAKE001_PROGRAM_ADDRESS,
): Promise<SignedPrice> {
    const message = getSignedPriceMessage(tokenMint, priceUsd, publishTime, action, programAddress);
    const signature = await signBytes(oracleSigner.keyPair.privateKey, message);
    return { tokenMint, priceUsd, publishTime, action, signer: oracleSigner.address, signature };
}

const PRICE_ACTION_NAMES: Record<PriceAction, string> = {
    [PriceAction.DepositStake]: "deposit",
    [PriceAction.OpenBorrowPosition]: "open",
    [PriceAction.ClosePosition]: "close",
    [PriceAction.LiquidatePosition]: "liquidate",
};

/**
 * Asks the price-oracle Edge Function (supabase/functions/price-oracle) for a freshly signed
 * price. Call it right before building the transaction - the program accepts it for 60s.
 * Throws with the oracle's reason when it refuses: the token failed the Rugcheck safety gate
 * (deposit / open only - the reasons are in the message), too few agreeing sources, or a
 * retryable condition (Rugcheck unreachable, a large move still being confirmed).
 * `warnings` are token-safety notes to show the user (e.g. an enabled freeze authority).
 */
export async function fetchSignedPrice(
    oracleUrl: string,
    supabaseAnonKey: string,
    tokenMint: Address,
    action: PriceAction,
): Promise<SignedPrice & { warnings: string[] }> {
    const response = await fetch(oracleUrl, {
        method: "POST",
        headers: {
            "Content-Type": "application/json",
            Authorization: `Bearer ${supabaseAnonKey}`,
            apikey: supabaseAnonKey,
        },
        body: JSON.stringify({ mint: tokenMint, action: PRICE_ACTION_NAMES[action] }),
    });
    const body = await response.json();
    if (!response.ok) {
        const retry = body.retryAfterSeconds ? ` (retry in ${body.retryAfterSeconds}s)` : "";
        const reasons = Array.isArray(body.reasons) && body.reasons.length ? `: ${body.reasons.join("; ")}` : "";
        // Which data feeds didn't count, e.g. "dropped coingecko (stale); failed birdeye (HTTP 429)".
        const feeds = [
            ...(Array.isArray(body.dropped) ? body.dropped.map((d: { source: string; reason: string }) => `dropped ${d.source} (${d.reason})`) : []),
            ...(Array.isArray(body.failed) ? body.failed.map((f: { source: string; error: string }) => `failed ${f.source} (${f.error})`) : []),
        ];
        const feedDetail = feeds.length ? ` [${feeds.join("; ")}]` : "";
        throw new Error(`price oracle refused: ${body.error ?? response.status}${reasons}${feedDetail}${retry}`);
    }
    if (body.tokenMint !== tokenMint || body.action !== action) {
        throw new Error("price oracle returned a price for a different mint or action");
    }
    return {
        tokenMint,
        priceUsd: BigInt(body.priceUsd),
        publishTime: BigInt(body.publishTime),
        action,
        signer: address(body.signer),
        signature: Uint8Array.from(atob(body.signature), (c) => c.charCodeAt(0)) as SignatureBytes,
        warnings: Array.isArray(body.warnings) ? body.warnings : [],
    };
}

/** Ed25519 precompile instruction carrying the signed price; place it right before the priced ix. */
export function getSignedPriceInstruction(
    signedPrice: SignedPrice,
    programAddress: Address = SHORT_STAKE001_PROGRAM_ADDRESS,
): Instruction {
    const message = getSignedPriceMessage(
        signedPrice.tokenMint,
        signedPrice.priceUsd,
        signedPrice.publishTime,
        signedPrice.action,
        programAddress,
    );
    const u16 = getU16Encoder();
    const data = new Uint8Array(MESSAGE_OFFSET + message.length);
    data.set([1, 0], 0); // one signature, padding byte
    [
        SIGNATURE_OFFSET,
        CURRENT_INSTRUCTION,
        PUBKEY_OFFSET,
        CURRENT_INSTRUCTION,
        MESSAGE_OFFSET,
        message.length,
        CURRENT_INSTRUCTION,
    ].forEach((value, i) => data.set(u16.encode(value), 2 + i * 2));
    data.set(getAddressEncoder().encode(signedPrice.signer), PUBKEY_OFFSET);
    data.set(signedPrice.signature, SIGNATURE_OFFSET);
    data.set(message, MESSAGE_OFFSET);

    return { programAddress: ED25519_PROGRAM_ADDRESS, accounts: [], data };
}
