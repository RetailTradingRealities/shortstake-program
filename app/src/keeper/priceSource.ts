// Where the keeper gets a liquidation price for each market. The same value is used for the
// health check AND passed to the program, so the keeper never acts on a price the program
// won't see. Pluggable so tests can inject fixed prices.
import { type Address, type GetAccountInfoApi, type KeyPairSigner, type Rpc } from "@solana/kit";
import { OracleFlag } from "../../../clients/js/src/generated/types";
import { PriceAction, fetchSignedPrice, signPrice } from "../oracle/signedPrice";
import { type OracleSelection } from "../instructions/OpenBorrowPosition";

export interface LiquidationPrice {
    oracle: Exclude<OracleSelection, { kind: "none" }>;
    /** 10^-12 USD per whole token - the price the program will use. */
    priceUsd: bigint;
}

export interface PriceSource {
    liquidationPrice(tokenMint: Address, oracleFlag: OracleFlag): Promise<LiquidationPrice>;
}

/**
 * Production prices: a LiquidatePosition-signed price from the price-oracle Edge Function for
 * every market, cached per mint for a few seconds (a liquidation price is only usable on-chain
 * for ~15 s, and every call costs the oracle upstream API quota).
 */
export class LivePriceSource implements PriceSource {
    private readonly cache = new Map<Address, { at: number; price: LiquidationPrice }>();

    constructor(
        private readonly rpc: Rpc<GetAccountInfoApi>,
        private readonly oracleUrl: string,
        private readonly supabaseAnonKey: string,
        // Short: the program accepts a liquidation price for only 15 s
        // (signed_price.rs MAX_LIQUIDATION_PRICE_AGE_SECONDS).
        private readonly signedPriceCacheMs = 3_000,
    ) {}

    // Every market is priced by the signed oracle since 2026-11-21 (Pyth stopped pushing its
    // meme-coin feeds), so the market's stored oracle flag is no longer consulted.
    async liquidationPrice(tokenMint: Address, _oracleFlag?: OracleFlag): Promise<LiquidationPrice> {
        const cached = this.cache.get(tokenMint);
        if (cached && Date.now() - cached.at < this.signedPriceCacheMs) return cached.price;
        const signedPrice = await fetchSignedPrice(this.oracleUrl, this.supabaseAnonKey, tokenMint, PriceAction.LiquidatePosition);
        const price: LiquidationPrice = { oracle: { kind: "custom", signedPrice }, priceUsd: signedPrice.priceUsd };
        this.cache.set(tokenMint, { at: Date.now(), price });
        return price;
    }
}

/**
 * Test prices: signs a fixed price per mint with a local oracle-signer key (which must equal
 * global_config.oracle_signer), stamped with `now()` - e.g. a fork's clock.
 */
export class FixedPriceSource implements PriceSource {
    constructor(
        private readonly oracleSigner: KeyPairSigner,
        private readonly prices: Map<Address, bigint>,
        private readonly now: () => Promise<bigint>,
    ) {}

    async liquidationPrice(tokenMint: Address): Promise<LiquidationPrice> {
        const priceUsd = this.prices.get(tokenMint);
        if (priceUsd === undefined) throw new Error(`no test price for ${tokenMint}`);
        const signedPrice = await signPrice(this.oracleSigner, tokenMint, priceUsd, PriceAction.LiquidatePosition, await this.now());
        return { oracle: { kind: "custom", signedPrice }, priceUsd };
    }
}
