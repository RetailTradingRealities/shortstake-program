// Loads every open BorrowPosition straight from the chain, with its offer and market. The chain
// is the source of truth: the Supabase mirror can lag or miss events, and a keeper acting on
// stale rows would waste fees or miss liquidations.
import {
    type Address,
    type Base58EncodedBytes,
    type GetAccountInfoApi,
    type GetMultipleAccountsApi,
    type GetProgramAccountsApi,
    type Rpc,
    getBase58Decoder,
} from "@solana/kit";
import { fetchMint } from "@solana-program/token";
import {
    type BorrowPosition,
    type LenderOffer,
    type MarketVault,
    BORROW_POSITION_DISCRIMINATOR,
    fetchAllMaybeLenderOffer,
    fetchAllMaybeMarketVault,
    getBorrowPositionDecoder,
} from "../../../clients/js/src/generated/accounts";
import { findMarketVaultPda } from "../../../clients/js/src/generated/pdas";
import { SHORT_STAKE001_PROGRAM_ADDRESS } from "../../../clients/js/src/generated/programs";

export interface OpenPosition {
    address: Address;
    position: BorrowPosition;
    offerAddress: Address;
    offer: LenderOffer;
    market: MarketVault;
    decimals: number;
}

/** getMultipleAccounts takes at most 100 addresses per call. */
async function fetchInChunks<T>(addresses: Address[], fetchChunk: (chunk: Address[]) => Promise<T[]>): Promise<T[]> {
    const out: T[] = [];
    for (let i = 0; i < addresses.length; i += 100) out.push(...(await fetchChunk(addresses.slice(i, i + 100))));
    return out;
}

/** Mint decimals never change, so they're cached for the keeper's lifetime. */
const decimalsCache = new Map<Address, number>();

export async function loadOpenPositions(rpc: Rpc<GetProgramAccountsApi & GetMultipleAccountsApi & GetAccountInfoApi>): Promise<OpenPosition[]> {
    const accounts = await rpc
        .getProgramAccounts(SHORT_STAKE001_PROGRAM_ADDRESS, {
            encoding: "base64",
            filters: [
                {
                    memcmp: {
                        offset: 0n,
                        bytes: getBase58Decoder().decode(BORROW_POSITION_DISCRIMINATOR) as Base58EncodedBytes,
                        encoding: "base58",
                    },
                },
            ],
        })
        .send();
    if (accounts.length === 0) return [];

    const decoder = getBorrowPositionDecoder();
    const positions = accounts.map((a) => ({
        address: a.pubkey,
        position: decoder.decode(Buffer.from(a.account.data[0], "base64")),
    }));

    const offerAddresses = [...new Set(positions.map((p) => p.position.lenderOfferRef))];
    const offers = new Map<Address, LenderOffer>();
    for (const offer of await fetchInChunks(offerAddresses, (chunk) => fetchAllMaybeLenderOffer(rpc, chunk))) {
        if (offer.exists) offers.set(offer.address, offer.data);
    }

    const mints = [...new Set([...offers.values()].map((o) => o.tokenMint))];
    const marketAddresses = await Promise.all(mints.map(async (tokenMint) => (await findMarketVaultPda({ tokenMint }))[0]));
    const markets = new Map<Address, MarketVault>();
    for (const market of await fetchInChunks(marketAddresses, (chunk) => fetchAllMaybeMarketVault(rpc, chunk))) {
        if (market.exists) markets.set(market.data.tokenMint, market.data);
    }
    for (const mint of mints) {
        if (!decimalsCache.has(mint)) decimalsCache.set(mint, (await fetchMint(rpc, mint)).data.decimals);
    }

    const open: OpenPosition[] = [];
    for (const { address, position } of positions) {
        const offer = offers.get(position.lenderOfferRef);
        const market = offer && markets.get(offer.tokenMint);
        // A position always has a live offer and market on chain; skip anything half-closed.
        if (!offer || !market) continue;
        open.push({ address, position, offerAddress: position.lenderOfferRef, offer, market, decimals: decimalsCache.get(offer.tokenMint)! });
    }
    return open;
}
