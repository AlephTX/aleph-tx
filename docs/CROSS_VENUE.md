# Cross-venue research and execution primitives

For same-underlying crypto and stock perp strategies, use the
[multi-level paper replay](PERP_RELATIVE_VALUE.md). It evaluates executable
depth, a causal basis model, per-payment funding and equity session constraints.

The cross-venue tool evaluates price and funding opportunities and replays order
events locally. Its market commands read public endpoints. Existing Lighter and
Hyperliquid Go feeds keep their original shared-memory layout.

| Venue | Market data | Order support |
| --- | --- | --- |
| Lighter | Existing Go BBO, depth and private feeds | Existing Linux x86-64 signer and strategy binaries |
| Hyperliquid | Existing Go BBO plus Rust REST book and perpetual metadata | Rust REST limit orders, cancellation and status lookup |
| Entropy | Hyperliquid HIP-3 metadata and `io:SYMBOL` books | Same Hyperliquid REST client with dynamically resolved HIP-3 asset IDs |
| Arcus | Public REST perpetual markets, BBO, orders, fills and positions | Ed25519 REST limit orders and cancellation |
| Variational | Public v2 perpetual/swap statistics with source timestamps | Read-only. Public prices are indicative |

The registry separates liquidity from shared infrastructure risk. Hyperliquid
core and Entropy have different liquidity domains and share a risk domain.
Another frontend for the same HIP-3 book shares its liquidity domain. Use
`VenueRegistry::register_hip3` for additional deployers. Independent venues
supply their own adapter and registry entry. No new Go shared-memory slot is
required for the research path.

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
target/release/cross_venue markets variational
target/release/cross_venue bbo arcus BTC-USD
```

Use an explicit contract mapping for underlying/index identity, settlement and
base units per contract. Verify it against market metadata and the account's
trading terms. The example's mapping, fees and capacity are synthetic.
A settlement or index mismatch excludes the pair. FX, inverse contracts,
spot-perpetual pairs and asymmetric swap funding require separate models.
The existing SHM scanner accepts explicit economics and instrument metadata
through `configure_market`. Its default instance collects BBOs without emitting
opportunities.

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
before submitting. HTTP 202 only confirms receipt. Use `order ORDER_ID` and
`fills [LIMIT]` to confirm fills. If the response is lost, use `open-orders`
and `orders` to reconcile the client ID before another submission. An order may
appear in those views after the gateway responds.

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

Design references: [Hummingbot fill-driven hedge architecture](https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/),
[Hyperliquid perpetual metadata](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals),
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding),
[Hyperliquid signing vectors](https://github.com/hyperliquid-dex/hyperliquid-python-sdk/blob/master/tests/signing_test.py),
[Entropy HIP-3 deployment](https://docs.entropy.io/about-entropy/readme.md),
[Arcus typed signing](https://docs.arcus.xyz/api-reference/authentication),
[Variational API](https://docs.variational.io/technical-documentation/api).
