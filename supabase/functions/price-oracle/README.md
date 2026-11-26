# price-oracle

Supabase Edge Function that prices a custom-oracle token from three data feeds and returns
the price signed by `global_config.oracle_signer`, ready for the program's Ed25519 check
(`programs/short-stake001/src/signed_price.rs`).

```
app --POST {mint, action}--> this function --> Birdeye + DexScreener + CoinGecko
                                   |  combine (core.ts) -> jump guard -> sign
app <---- SignedPrice JSON --------+   (state + 5s cache in Postgres, service role only)
app: getSignedPriceInstruction(signedPrice) directly before the priced instruction
```

- `core.ts` - pure rules: outlier removal, median, per-action minimum sources, jump guard,
  USD -> 10^-12 USD price units, the 81-byte signed message. No I/O.
- `sources.ts` - one fetcher per data feed, each returning a `SourceQuote` or throwing.
- `rugcheck.ts` - the token safety gate for `deposit` / `open` (rules below).
- `index.ts` - HTTP handler: cache, fetch, combine, guard, sign, respond.
- Tests (offline, recorded responses): `npm run test:oracle`.
  Live check of the feeds with your `.env` keys: `npm run oracle:check -- <mint>`.

## Rules

| Action | Min agreeing sources |
|---|---|
| `deposit`, `open` | 2 |
| `close`, `liquidate` | 1 |

CoinGecko counts as one source: its aggregated `/simple/token_price` price (fresh, includes
exchanges) for tokens CoinGecko lists, otherwise its on-chain price (often minutes old, so
often dropped). Unlisted mints are remembered for an hour, so each price costs one CoinGecko
call. The aggregated price has no liquidity figure, so it skips the liquidity floor.

A source is ignored when its price is missing, its own timestamp is over 2 minutes old,
or its liquidity is under $25k. With 3 sources, any more than 5% from the median is dropped;
2 sources more than 5% apart are both dropped. The result is the median of what is left.
**Time-anchored prices** (Slot Zero M-01). The **anchor** is the latest accepted price that is
at least 60 s old, or one that already passed the 60 s confirmation hold. `oracle_price_state.price_history`
keeps 10 minutes of accepted prices, plus one older entry.
- A move of more than 15% from the anchor is only signed once it has held for 60 s
  (503 + `Retry-After` until then). Measuring from the anchor, not from the last accepted
  price, stops quick 15% steps from ratcheting the price up.
- `liquidate` signs the **lower** of the current price and the anchor. So a pumped pool has to
  hold for over a minute before it can liquidate shorts, and a genuine fast rise reaches
  liquidations about a minute late.
- Every price is signed as of now. The program accepts a `liquidate` price for only **15 s**
  (`MAX_LIQUIDATION_PRICE_AGE_SECONDS` in `signed_price.rs`) and the others for 60 s. A liquidate
  signature names no position, so with a long window a liquidator could keep the highest price of
  a falling minute and use it after the drop (reported by KupiaSec, 2026-11-23). Fetch a
  liquidation price right before sending it.
- `open` signs the **higher** of the two, so a dumped pool can't let a borrower post less
  collateral.
- `deposit` and `close` sign the current price.

**Exit fallback** (Slot Zero L-01). When no source survives for `close` or `liquidate` (every
pool under the liquidity floor, or two sources disagreeing), the oracle signs a fallback
instead of refusing. It's built from the last accepted price and the raw quotes, including
dropped ones: the **higher** for close (the close price only sets interest) and the **lower**
for liquidate (a thin pool can't be pushed up through it). The response carries
`fallback: true` and the log says `priced_exit_fallback`. Deposits and opens never fall back.

Thresholds live in `GUARDS` / `MIN_SOURCES` in `core.ts`. A data feed whose API key secret is
missing counts as one failed source; it doesn't fail every request (I-08).

### Token safety gate (Rugcheck) - `deposit` and `open` only

Before signing a deposit or open price the token must pass Rugcheck; exits (`close`,
`liquidate`) are never gated. Pyth-listed tokens never call the oracle, so they are never
checked. A verdict is cached for 1 hour per token (`oracle_token_checks`); if Rugcheck can't
be reached the request is refused with 503 - a token is never let through unchecked.

| Rejects (422, reasons listed) | Warns only (returned as `warnings`) |
|---|---|
| `rugged` is true or missing | token program is not SPL Token |
| `score_normalised` above 50 (higher = riskier) or missing | freeze authority enabled |
| locked LP below 20% **and** total liquidity below $1M (either one passing is enough) | mint authority enabled |
| any risk with level `danger`, or risks missing | |

One call: `/report?refresh=true` (a fresh rescan, which needs the paid key in `X-API-KEY`).
Locked LP is computed from that report's markets - `sum(lpLockedUSD) / sum(quoteUSD + baseUSD)`,
the same figure as `/report/summary`'s `lpLockedPct`. The summary itself is not used: plain it
is cached, and with `refresh=true` it wrongly reports 0% locked. A check takes 4-30s (45s timeout).
Rescans are rate-limited: HTTP 429 is retried twice (after 1s, then 3s, or a `Retry-After`
of up to 5s) before the request is refused with 503.
Thresholds live in `RUGCHECK_RULES` in `rugcheck.ts`.

**Liquidity rule exemption.** Rugcheck counts liquidity only from pools where the token is the
*base* token, so wrapped SOL (`So111...112`, the quote side of nearly every pool) reports 0 markets
and $0. Mints in `LIQUIDITY_RULE_EXEMPT_MINTS` skip that one rule; the rugged, score and danger
checks still apply, and the price still needs agreeing data feeds like any other token.

## Logs

Each notable outcome is one JSON line in the function logs (Dashboard -> Edge Functions ->
price-oracle -> Logs; search `price_oracle_event`), with the mint, action, reason and every
dropped / failed feed:

| `price_oracle_event` | Meaning |
|---|---|
| `priced_with_feeds_missing` | signed, but some feed was dropped or failed |
| `retrying_too_few_sources` | first attempt short of sources; fetching once more after 1s |
| `recovered_on_retry` | the retry found enough agreeing sources and the price was signed |
| `refused_too_few_sources` | still short after the retry - 422 |
| `refused_token_unsafe` | failed the Rugcheck gate - 422 |
| `refused_token_check_unavailable` | Rugcheck unreachable - 503 |
| `refused_jump_pending` | large move awaiting confirmation - 503 |
| `refused_price_out_of_range` | price outside the on-chain unit range - 422 |

Counting `refused_too_few_sources` against `recovered_on_retry`, and which feeds appear in
`dropped` / `failed`, shows whether a streaming feed is worth building.

## Deploy

```bash
# tables: supabase/migrations/20261014120000_price_oracle_state.sql,
#         supabase/migrations/20261015120000_oracle_token_checks.sql
supabase db push

# secrets - BIRDEYE / COINGECKO / RUGCHECK keys from .env, and the oracle signer keypair (JSON byte array)
supabase secrets set --env-file .env --project-ref euwmdcepgwcesoqntlnd
supabase secrets set ORACLE_SIGNER_KEYPAIR="$(cat keys/oracle-signer-keypair.json)" --project-ref euwmdcepgwcesoqntlnd

# deploy (JWT verification on: callers send the project's anon key)
supabase functions deploy price-oracle --project-ref euwmdcepgwcesoqntlnd
```

**Devnet testing only:** devnet test tokens have no market price, so the oracle would refuse
them. `DEVNET_PRICE_ALIASES="devnetMint=realMainnetMint,..."` makes it price (and Rugcheck)
the real token instead and sign that price for the devnet mint, so the website can be tested
end to end on devnet. **Unset it before the mainnet deployment** (`supabase secrets unset
DEVNET_PRICE_ALIASES`): a signature names the mint it is for, and a devnet mint address must
never be priced as another token on mainnet.

`SUPABASE_URL` / `SUPABASE_SERVICE_ROLE_KEY` are injected automatically by the platform.
The signer's public key must equal `global_config.oracle_signer` on the target cluster, or
the program rejects every price. Use a fresh keypair for mainnet.

## Call it

```bash
curl -s https://euwmdcepgwcesoqntlnd.supabase.co/functions/v1/price-oracle \
  -H "Authorization: Bearer $SUPABASE_ANON_KEY" -H 'Content-Type: application/json' \
  -d '{"mint":"EKpQGSJtjMFqKZ9KQanSqYXRcF8fBopzLHYxdM65zcjm","action":"open"}'
```

From TypeScript: `fetchSignedPrice(oracleUrl, anonKey, mint, PriceAction.OpenBorrowPosition)`
in `app/src/oracle/signedPrice.ts`.

Devnet test mints are unknown to the data feeds, so on devnet only mainnet mint addresses
get a price.
