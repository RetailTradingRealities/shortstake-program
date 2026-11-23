// The SITE lookup table: accounts every open_borrow_position / close_position from the website
// uses, so they cost 1 byte each in the transaction instead of 32. Without it, an open or close
// whose Jupiter route needs 32+ accounts (low-cap memes on Pump.fun AMM, often via SOL) is over
// the 1232-byte limit (fork test, 2026-11-14: BULLISH open 1365-1455 bytes).
// Protocol-wide accounts plus, per market, its mint, market vault and vault token account.
// A table holds at most 256 addresses: 9 protocol-wide + 3 per market -> ~80 markets per table.
// Anyone can read a lookup table; only its authority can extend, deactivate or close it, and an
// entry can't be changed or removed, so a table can't make a transaction do anything different.

import { type Address, address } from "@solana/kit";
import { TOKEN_PROGRAM_ADDRESS, findAssociatedTokenPda } from "@solana-program/token";
import { findGlobalConfigPda, findGlobalUsdcVaultPda, findMarketVaultPda, findProtocolStatePda, findVaultTokenAccountPda } from "../../../clients/js/src/generated/pdas";

const SYSTEM_PROGRAM = address("11111111111111111111111111111111");
const INSTRUCTIONS_SYSVAR = address("Sysvar1nstructions1111111111111111111111111");

/** The protocol-wide accounts (same for every market). */
export async function siteProtocolAddresses(usdcMint: Address, feeCollector: Address): Promise<Address[]> {
    const [[globalConfig], [globalUsdcVault], [protocolState], [feeCollectorUsdc]] = await Promise.all([
        findGlobalConfigPda(),
        findGlobalUsdcVaultPda(),
        findProtocolStatePda(),
        findAssociatedTokenPda({ owner: feeCollector, mint: usdcMint, tokenProgram: TOKEN_PROGRAM_ADDRESS }),
    ]);
    return [globalConfig, globalUsdcVault, protocolState, feeCollector, feeCollectorUsdc, usdcMint, TOKEN_PROGRAM_ADDRESS, SYSTEM_PROGRAM, INSTRUCTIONS_SYSVAR];
}

/** One market's accounts: its token mint, market vault (PDA) and vault token account. */
export async function siteMarketAddresses(tokenMint: Address): Promise<Address[]> {
    const [[marketVault], [vaultTokenAccount]] = await Promise.all([
        findMarketVaultPda({ tokenMint }),
        findVaultTokenAccountPda({ tokenMint }),
    ]);
    return [tokenMint, marketVault, vaultTokenAccount];
}
