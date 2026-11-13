# Configuration reference

Keep versioned strategy parameters and local credentials in separate
files. Start from the checked-in profile and validate it offline before
using a network mode. [Example profile](../config.example.toml).

```sh
make trade MODE=check CONFIG=config.example.toml
```

## Supervised profile

Set `strategy_mode` at the TOML root, before the first table.

| Mode | Constraints |
| --- | --- |
| `quant` | Run the Lighter inventory-aware strategy. Leave `[inventory_hedge]` absent. |
| `inventory_hedge` | Enable `[inventory_hedge]`, Lighter post-only quoting and the matching native Hyperliquid feed. |
| Omitted | Resolve the mode from the presence of `[inventory_hedge]`. Existing profiles use this rule. |

| Section | Contract |
| --- | --- |
| `[symbols]` | Maps tickers to Go and Rust shared-memory IDs. |
| `[lighter]` | Enable the feeder and map exactly one configured ticker to its native Lighter market ID. |
| `[hyperliquid]` | For hedging, enable the feed and map the same ticker to its native perpetual on the same network. |
| `[inventory_neutral_mm]` | Set the exchange and symbol IDs, Lighter market ID, native tick and step, maker fee, quote size and position and loss limits. |
| `[inventory_hedge]` | Set `max_unhedged_base`, `deadband_base`, per-IOC notional, minimum available balance, limit slippage and taker fee. |

A shared-memory symbol ID identifies a local slot. A venue market ID
identifies an instrument in the venue protocol and can differ between
networks. `tick_size` and `step_size` come from the selected market.
Configure `max_spread_bps` to cover the maker round trip, configured hedge
cost and profit floor. The launcher validates this relationship when
hedging is enabled. [Trading runbook](LIVE_TRADING.md#configure-and-validate).

The optional `[inventory_neutral_mm.markout_reserve]` table widens the
inventory-adding side using its own recent fill markouts against fresh
external fair value. `max_widen_bps` caps the reserve on each side and
`max_spread_bps` caps the model's combined spread. Set `close_cost_bps` to
the modeled exit fee and impact. The gross markout target includes this
cost, the maker fee and half the round-trip profit floor. The estimate
requires eight marked fills per side and expires after
`max_sample_age_ms`. Validate the policy at the account tier's actual
placement, cancellation and IOC latencies in the public-data replay.
[Limit order placement and latency](https://arxiv.org/abs/1610.00261) ·
[Oasis execution sensitivity](PERP_RELATIVE_VALUE.md#market-selection-and-execution-sensitivity-2026-11-13)

The optional `[inventory_neutral_mm.order_flow_reserve]` table uses
changes in the native Hyperliquid best prices and displayed sizes.
`half_life_ms` sets the decay and `max_widen_bps` bounds the price
reserve on the side exposed to the pressure. The estimate advances
once per fresh publication and resets on stale books or a clock
regression. The quote loop takes the larger of the order-flow and
fill-markout reserves on each side and preserves the spread cap and
the quote toward flat.
Tune `min_cycle_interval_ms` against eligible fills, net PnL and
inventory at the modeled venue delays.
[Order flow imbalance](https://doi.org/10.1093/jjfinec/nbt003) ·
[Cross-venue crypto price discovery](https://arxiv.org/abs/2108.09750) ·
[Oasis cadence comparison](PERP_RELATIVE_VALUE.md#quote-cadence-and-hedge-recovery-2026-11-13)

## Credentials and network

Keep funded credentials in the local environment file or process
environment. The default file is `.env.lighter` for mainnet and
`.env.testnet` for testnet. Set `ENV_FILE=-` to use the process environment.
Treat account responses and session journals as private operational data.

| Venue | Environment keys |
| --- | --- |
| Lighter | `LIGHTER_PRIVATE_KEY` or legacy `API_KEY_PRIVATE_KEY`, `LIGHTER_ACCOUNT_INDEX`, `LIGHTER_API_KEY_INDEX` |
| Lighter network override | `LIGHTER_BASE_URL`, `LIGHTER_CHAIN_ID` |
| Hyperliquid inventory hedge | `HYPERLIQUID_ACCOUNT_ADDRESS`, `HYPERLIQUID_AGENT_PRIVATE_KEY` |

Use the signer key for the selected Lighter API key slot and an approved
Hyperliquid agent for the selected account. The profile endpoints and
the Lighter signing network must match the launcher mode. The
[network runbook](LIVE_TRADING.md#start-and-stop) specifies testnet IDs
and stop and reconciliation procedures.

## Paper strategy configuration

The paper runner uses JSON or streaming NDJSON. Each run has two native
markets with a reviewed common underlying, payoff and settlement, plus
per-leg precision, minimums, fees and funding terms. Use `pair_hedge` for
unbudgeted strategy replay within the configured risk limits or `turnover`
with quote-denominated activity and fee caps. Both use the same signal
and exposure checks by default. An explicit incentive policy can study
the cost of activity on a reviewed unissued-token venue within a dated
eligibility window. [Pair sizing and replay](PERP_RELATIVE_VALUE.md#replay).

The capture compiler adds transport and timing settings, archived venue
context, independent reference prices and session evidence. Enable paper
entries after the inputs are verified. [Capture profiles](PERP_RECORDING.md).
