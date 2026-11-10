import { type Address, type Rpc, type GetAccountInfoApi, address } from "@solana/kit";
import { PYTH_FEEDS } from "./generated/pythFeeds";

// Which price source the program uses for a mint, mirroring its is_pyth_supported(): a mint on
// PYTH_ALLOWED_FEEDS is priced by Pyth (pass its sponsored price account as pyth_price_feed),
// any other mint by the signed custom oracle. Works before the market exists, so it also
// tells a first depositor which oracle the new market will get.

/** Pyth's sponsored push-feed price account for `tokenMint`, or null if it isn't Pyth-priced. */
export function getPythPriceAccount(tokenMint: Address): Address | null {
    const feed = PYTH_FEEDS[tokenMint];
    return feed ? address(feed.priceAccount) : null;
}

export function isPythMint(tokenMint: Address): boolean {
    return tokenMint in PYTH_FEEDS;
}

/**
 * Price and publish time from a Pyth Receiver PriceUpdateV2 account (Full verification),
 * normalized to the program's 10^-12 USD units the same way get_pyth_price_in_usdc.rs does.
 */
export async function readPythPrice(
    rpc: Rpc<GetAccountInfoApi>,
    account: Address,
): Promise<{ units: bigint; publishTime: bigint; usd: number }> {
    const { value } = await rpc.getAccountInfo(account, { encoding: "base64" }).send();
    if (!value) throw new Error(`Pyth price account ${account} not found`);
    const d = Buffer.from(value.data[0], "base64");
    // discriminator(8) write_authority(32) verification_level(1 for Full) feed_id(32) price i64, conf u64, exponent i32, publish_time i64
    if (d[40] !== 1) throw new Error("Pyth price account is not fully verified");
    const price = d.readBigInt64LE(73);
    const exponent = d.readInt32LE(89);
    const publishTime = d.readBigInt64LE(93);
    const shift = 12 + exponent; // program units are 10^-12 USD (get_pyth_price_in_usdc.rs)
    const units = shift >= 0 ? price * 10n ** BigInt(shift) : price / 10n ** BigInt(-shift);
    return { units, publishTime, usd: Number(price) * 10 ** exponent };
}
