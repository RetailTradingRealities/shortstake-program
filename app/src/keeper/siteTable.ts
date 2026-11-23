// Keeps the SITE lookup table (instructions/siteLookupTable.ts) complete: the protocol-wide
// accounts plus every market's mint, market vault and vault token account, so the website's
// opens and closes with long Jupiter routes fit in one transaction. The keeper owns the table
// (its wallet is the table's authority) and adds a new market's accounts on its next sync.
// Markets come straight from the chain (every MarketVault account), including markets with offers
// but no positions yet - their first short needs the table too.
// Display/size helper only: a lookup table can't change what a transaction does.
import {
    type Address,
    type Base58EncodedBytes,
    type TransactionSigner,
    getBase58Decoder,
} from "@solana/kit";
import { MARKET_VAULT_DISCRIMINATOR, getMarketVaultDecoder } from "../../../clients/js/src/generated/accounts";
import { SHORT_STAKE001_PROGRAM_ADDRESS } from "../../../clients/js/src/generated/programs";
import { ensureInLookupTable } from "../instructions/keeperLookupTable";
import { type SolanaClient } from "../instructions/OpenBorrowPosition";
import { siteMarketAddresses, siteProtocolAddresses } from "../instructions/siteLookupTable";

/** Every market's token mint, from the MarketVault accounts on chain. */
export async function loadMarketMints(client: SolanaClient): Promise<Address[]> {
    const accounts = await client.rpc
        .getProgramAccounts(SHORT_STAKE001_PROGRAM_ADDRESS, {
            encoding: "base64",
            filters: [
                {
                    memcmp: {
                        offset: 0n,
                        bytes: getBase58Decoder().decode(MARKET_VAULT_DISCRIMINATOR) as Base58EncodedBytes,
                        encoding: "base58",
                    },
                },
            ],
        })
        .send();
    const decoder = getMarketVaultDecoder();
    return accounts.map((a) => decoder.decode(Buffer.from(a.account.data[0], "base64")).tokenMint);
}

/**
 * Adds whatever is missing from the site table. Returns the number of markets covered.
 * Throws if the table is full (256 addresses: 9 protocol-wide + 3 per market, ~82 markets).
 */
export async function syncSiteLookupTable(
    client: SolanaClient,
    authority: TransactionSigner,
    table: Address,
    usdcMint: Address,
    feeCollector: Address,
): Promise<{ markets: number }> {
    const mints = await loadMarketMints(client);
    const addresses = [
        ...(await siteProtocolAddresses(usdcMint, feeCollector)),
        ...(await Promise.all(mints.map((mint) => siteMarketAddresses(mint)))).flat(),
    ];
    await ensureInLookupTable(client, authority, table, addresses);
    return { markets: mints.length };
}
