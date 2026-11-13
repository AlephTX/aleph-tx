# Cross-venue research and execution primitives

The cross-venue CLI discovers markets, inspects public books and replays
two-leg order events. For funding carry and basis reversion on matched crypto
or equity perpetuals, use the depth-aware [paper replay](PERP_RELATIVE_VALUE.md).

## Venue capabilities

| Venue | Data in AlephTX | Order transport | Operating scope |
| --- | --- | --- | --- |
| Lighter | Go BBO, depth and private events plus public L2 capture | Native signer on Linux x86-64 | Primary BTC or ETH maker |
| Hyperliquid | Go BBO plus public WebSocket and REST L2 capture | Native REST limit orders, cancellation and order status | Native BTC or ETH inventory hedge |
| Entropy | HIP-3 metadata and `io:SYMBOL` books via both Hyperliquid transports | Hyperliquid REST with resolved HIP-3 asset IDs | Offline pair replay |
| TradeXYZ | HIP-3 metadata and `xyz:SYMBOL` books via both Hyperliquid transports | Hyperliquid REST with resolved HIP-3 asset IDs | Offline pair replay |
| Arcus | Public REST metadata and BBO, REST and WebSocket L2 capture | Standalone signed Ed25519 REST, order status and fills | Offline pair replay |
| BULK | Public REST metadata and BBO, REST and WebSocket L2 capture | Read-only integration | Offline pair replay |
| Extended | Public CLOB metadata, sequenced WebSocket L2 and public trades | Read-only capture | Offline maker-first replay |
| Variational | Public v2 perpetual and swap statistics with source timestamps | Read-only indicative data | Market research |

A liquidity domain identifies a matching book. Two frontends using the same
book are one liquidity domain and cannot form a pair. A risk domain
identifies shared infrastructure. Native Hyperliquid, Entropy and TradeXYZ
have distinct books and share that risk domain. Entropy × TradeXYZ
requires `allow_shared_risk_domain: true` on the pair.
[Hyperliquid HIP-3](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals).

A single recording can support all six pairings among Entropy, Arcus,
TradeXYZ and BULK. Each replay has its own limits and evaluates one pair
in isolation. Joint allocation and live reconciliation need a coordinator
that reserves exposure across all routes. Additional HIP-3 deployers use
`VenueRegistry::register_hip3`. An independent venue adds its own adapter
and registry entry. Research capture runs outside the live shared-memory path. [Four-venue recording](PERP_RECORDING.md#four-venue-pair-recordings).

## Evaluate and replay

```sh
make build
target/release/cross_venue venues
target/release/cross_venue replay examples/cross_venue_replay.json
target/release/cross_venue hedge-replay examples/hedge_replay.json
```

The synthetic market fixture contains an eligible Lighter/Hyperliquid pair, a
more attractive indicative quote, a fee increase and stale books. Each frame
prints candidate directions and their cost breakdown or rejection reason. The
hedge fixture starts at the first frame's approved limits and quantity. It
exercises an ACK without a fill, a partial IOC fill, an uncertain hedge
submission, reconciliation and completion with zero residual base exposure.
Both fixtures run in `make test`.

```sh
target/release/cross_venue markets hyperliquid
target/release/cross_venue markets entropy
target/release/cross_venue markets trade_xyz
target/release/cross_venue markets bulk
target/release/cross_venue markets variational
target/release/cross_venue bbo arcus BTC-USD
target/release/cross_venue bbo bulk NVDA-USD
```

### Pair admission

Verify canonical underlying, payoff adjustments, settlement asset and base
units per contract against the venue specifications. Configure native tick,
lot and minimums, the account's applicable fees and capacity, and each
payment schedule. A settlement or payoff mismatch excludes a pair. FX,
inverse contracts, spot-perpetual pairs and asymmetric swap funding require
separate models. The included examples use synthetic terms.

The SHM scanner accepts explicit economics and instrument metadata through
`configure_market`. Without that configuration it collects BBOs only.

## Arcus REST

Use `cross_venue arcus testnet` for the testnet or `cross_venue arcus mainnet`
for the mainnet. Market discovery needs no credentials. Account reads take
`ARCUS_ACCOUNT_ADDRESS` and `ARCUS_ACCOUNT_INDEX` (default `0`). Order writes
also require `ARCUS_API_PRIVATE_KEY`, the 32-byte Ed25519 signing key registered
for that address and subaccount. See the [Arcus REST guide](https://docs.arcus.xyz/guides/rest-trading).

```sh
target/release/cross_venue arcus testnet markets
target/release/cross_venue arcus testnet bbo BTC-USD
target/release/cross_venue arcus testnet open-orders 100
target/release/cross_venue arcus testnet positions
```

`place-limit MARKET SIDE SIZE PRICE TIF CLIENT_ID [--reduce-only]` sends a
limit order with `alo`, `ioc`, `fok`, or `gtt`. Set the price and size to the
market's tick and step. Use a unique client ID for each intent and save it
before submitting. HTTP 202 confirms receipt. Use `order ORDER_ID` and
`fills [LIMIT]` to confirm fills. If the response is lost, reconcile the
client ID through `open-orders`, `orders` and subsequent fill and position
evidence before another submission. Allow for delayed visibility after
the gateway responds.

`cancel-order MARKET_ID ORDER_ID`, `cancel-client MARKET_ID CLIENT_ID`, and
`cancel-all MARKET_ID` submit cancellations. Use `cancel-all all` for the whole
subaccount. Verify the outcome through order and fill queries before releasing
reserved exposure.

## Risk and interpretation

- The planner computes executable entry limits, sizes both legs to a common
  tradable lot and reserves entry and exit fees, slippage and closing basis.
  It limits depth use, per-leg notional and single-leg exposure. Candidates are
  ranked scenarios and may compete for the same funds. Portfolio limits and
  actual available margin belong to the caller.
- Funding carry uses per-payment rates, exchange payment schedules and oracle
  notionals, with an explicit uncertainty allowance. The projection holds the
  forecast fixed over the horizon. Use recorded rates and order book events for
  historical performance measurement. The sample fixture measures behavior.
- Firm RFQs require a quote ID, exact size and sufficient validity. The current
  Variational endpoint supplies indicative prices with a source timestamp.
  Perpetual interval rates and asymmetric swap rates are parsed separately.
- The hedge machine follows confirmed cumulative fills and reserves each intent
  before returning it. On uncertain submission, query order status before
  issuing another intent. An ACK alone cannot release a reservation. A restored
  checkpoint requires reconciliation and stays halted. Persist venue/client ID
  mappings, add an order journal and supervise unresolved exposure before
  connecting a live coordinator. The replay commands never submit orders.
- Arcus signing uses the top-level tick as the signed integer denominator and
  checks coarser price tiers separately. Its order commands are explicit REST
  operations. The supervised inventory strategy uses Lighter and Hyperliquid.

### Dated settlement and combination orders

The offline `matched-settlement` command screens two linear cash-settled
contracts with a common underlying, fixing index, fixing rule, expiry and
settlement currency. The fixing rule includes the index source, averaging
window and fallback methodology. Both legs must have terminal-only cash
flows. The screen converts native quantities to a common base lot and
subtracts supplied fee and holding-cost bounds from the terminal spread.
Entry limits are expressed per base unit and apply at the supplied capacity.
The return denominator is supplied capital across both accounts and any
external reserve. The cash bound depends on matched fills, collateral through
expiry and fulfillment of both contracts. Record venue minimums and verify
capacity and settlement specifications before supplying an input.

```sh
target/release/cross_venue matched-settlement examples/matched_settlement.json
```

OKX Nitro Spreads and Deribit Combo Books execute their supported legs as a
combination. Paradigm reports clearing status for all legs at the underlying
settlement venue. Keep order matching and final clearing evidence distinct.
The current AlephTX cross-venue simulator submits independent IOC legs and
models partial execution and recovery.
[OKX Nitro Spreads](https://www.okx.com/en-gb/help/nitro-spreads-introduction) ·
[Deribit Combo Books](https://support.deribit.com/hc/en-us/articles/31424954956061-Combo-Books) ·
[Paradigm clearing](https://docs.paradigm.co/unified-markets/summary)

Deribit linear futures deliver in USDC using a 30-minute Deribit index
average. OKX's USDT expiry product specifies USDT and a final-hour index
average. Both venues also describe daily PnL settlement. A matching BTC
ticker and delivery timestamp leave these contracts outside the current
terminal-only screen.
[Deribit linear futures](https://support.deribit.com/hc/en-us/articles/31424954805405-Linear-Futures) ·
[Deribit daily settlement](https://support.deribit.com/hc/en-us/articles/29734325712413-Settlement) ·
[OKX expiry futures](https://www.okx.com/en-us/help/expiry-futures)

The public Deribit Box probe checks the signed legs of each open four-leg
combination for common USDC settlement, fixing, expiry and multiplier.
A long Box has a fixed gross expiry value equal to quantity times strike
width. The probe calculates a depth-weighted debit and uses the native
fill-or-kill limit to bound cash outflow. A positive net cash-flow bound
also requires bounded delivery, financing and trading costs. Account
margin remains unknown from public endpoints.

```sh
python3 scripts/probe_deribit_box.py
```

At 11:18:02 UTC on 2026-11-13, the probe checked 36 active USDC combinations,
including four with four legs and complete leg metadata, against 3,162
open USDC linear options. It found zero native Box candidates. The
documented combo creation types omit Box. Block RFQ has a separate minimum
size, quote and margin workflow. No account quote or margin evidence was
collected.
[Deribit combo types](https://support.deribit.com/hc/en-us/articles/31424954956061-Combo-Books) ·
[Combo order types](https://support.deribit.com/hc/en-us/articles/25944794271261-Option-Combo-Order) ·
[Linear USDC options](https://support.deribit.com/hc/en-us/articles/31424932728093-Linear-USDC-Options) ·
[Fees](https://support.deribit.com/hc/en-us/articles/25944746248989-Fees) ·
[Block RFQ](https://support.deribit.com/hc/en-us/articles/25951371614621-Deribit-Block-RFQ)

### Extended execution research

Oasis records Extended BTC-USD and ETH-USD active CLOB perpetuals with
Lighter on one receipt clock. The initial sealed sample included
sequenced depth and public trades for both markets.
[Capture and replay](PERP_RECORDING.md#extended--lighter-synchronized-capture)

Before using CLOB queue simulation, check `isRfq` and verify the public depth
sequence, trade IDs, timestamps, fee tier and settlement currency. RFQ market
depth on the standard stream is indicative. The dedicated RFQ stream carries
resting orders that an opposing IOC may pick up at the maker fee. The RFQ
order endpoint has a one-request-per-second limit. Venue credits require
separate account evidence.
[Extended API and RFQ documentation](https://api.docs.extended.exchange/#rfq-markets)

Extended's published ordinary crypto rates are 0 bps maker and 2.5 bps
taker. Its RWA taker rate is 1 bp. Ordinary crypto maker rebates of 0.2,
0.4, 0.6 and 1.0 bps depend on trailing 30-day maker market share and a
$10 minimum daily payout. Lighter Standard regular orders are 0 bps on
both sides, with 300 ms added to taker and cancel/modify actions. Plus
uses 0.5 bps on both sides with the same delays. Treat these as venue
schedule inputs and confirm the actual account tier for execution research.
[Extended fee schedule](https://docs.extended.exchange/extended-resources/trading/trading-fees-and-rebates.md) ·
[Lighter fee and latency schedule](https://docs.lighter.xyz/trading/trading-fees)

Design references: [Hummingbot fill-driven hedge architecture](https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/),
[Hyperliquid perpetual metadata](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals),
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding),
[Hyperliquid signing vectors](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py),
[Entropy HIP-3 deployment](https://docs.entropy.io/about-entropy/readme.md),
[Arcus typed signing](https://docs.arcus.xyz/api-reference/authentication),
[Variational API](https://docs.variational.io/technical-documentation/api).
