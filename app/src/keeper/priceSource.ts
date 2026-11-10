// Where the keeper gets a liquidation price for each market. The same value is used for the
// health check AND passed to the program, so the keeper never acts on a price the program
// won't see. Pluggable so tests can inject fixed prices.
import { type Address, type GetAccountInfoApi, type KeyPairSigner, type Rpc } from "@solana/kit";
import { OracleFlag } from "../../../clients/js/src/generated/types";
import { getPythPriceAccount, readPythPrice } from "../oracle/pyth";
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
 * Production prices. Custom-oracle markets: a LiquidatePosition-signed price from the
 * price-oracle Edge Function, cached per mint (the program accepts signed prices up to 60s old,
 * and every call costs the oracle upstream API quota). Pyth markets: the sponsored price
 * account read directly - always fresh, the program re-reads the same account.
 */
export class LivePriceSource implements PriceSource {
    private readonly cache = new Map<Address, { at: number; price: LiquidationPrice }>();

    constructor(
        private readonly rpc: Rpc<GetAccountInfoApi>,
        private readonly oracleUrl: string,
        private readonly supabaseAnonKey: string,
        private readonly signedPriceCacheMs = 20_000,
    ) {}

    async liquidationPrice(tokenMint: Address, oracleFlag: OracleFlag): Promise<LiquidationPrice> {
        if (oracleFlag === OracleFlag.Pyth) {
            const priceFeed = getPythPriceAccount(tokenMint);
            if (!priceFeed) throw new Error(`market ${tokenMint} is Pyth-priced but its mint isn't in PYTH_FEEDS`);
            const { units } = await readPythPrice(this.rpc, priceFeed);
            return { oracle: { kind: "pyth", priceFeed }, priceUsd: units };
        }
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
