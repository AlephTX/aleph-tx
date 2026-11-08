# Supervised Lighter trading

The supervised session runs inventory-aware Lighter market making for one BTC
or ETH perpetual. The optional inventory hedge uses the matching native
Hyperliquid perpetual. It works from confirmed account positions and trades
only the amount needed to reduce the portfolio's net base exposure.

## Start and stop

1. On Linux x86-64 with Rust and Go installed, copy `config.example.toml` to
   `config.toml`. Set the Lighter market ID, symbol mapping, tick and lot
   size. The supervised feeder uses the Lighter section and, when enabled, the
   Hyperliquid section for external fair value.
2. Create a local `.env.lighter` with `LIGHTER_PRIVATE_KEY` (40-byte hex),
   `LIGHTER_ACCOUNT_INDEX` and `LIGHTER_API_KEY_INDEX`. The launcher also
   recognizes the existing `API_KEY_PRIVATE_KEY` name. Use a dedicated trading
   account. Startup requires zero position and zero open orders on the selected
   Lighter market.
3. To enable hedging, add `[inventory_hedge]` using the commented example in
   `config.example.toml`. Review the base cap, residual tolerance, IOC notional,
   available balance and slippage. Set `HYPERLIQUID_ACCOUNT_ADDRESS` to the
   funded account address and `HYPERLIQUID_AGENT_PRIVATE_KEY` to its approved
   API wallet key. The supervised route uses the account directly with its own
   approved agent. Hyperliquid startup requires a flat account and no open orders.
4. Run `make trade` for offline config validation. Run `make trade MODE=live`
   to start the supervised session. Set `CONFIG=/path/to/profile.toml` and
   `ENV_FILE=/path/to/.env` to use other files. Set `ENV_FILE=-` to use the
   process environment.

### Testnet

Copy `config.example.toml` to `config.testnet.toml`. In `[lighter]`, use
`wss://testnet.zklighter.elliot.ai/stream` and map the selected symbol to its
testnet market. In `[hyperliquid]`, use
`wss://api.hyperliquid-testnet.xyz/ws`. Set the matching IDs and precision in
`[inventory_neutral_mm]`. Verify the live venue metadata before use.

| Lighter market | Mainnet ID | Testnet ID | Testnet tick | Testnet step |
|----------------|------------|------------|--------------|--------------|
| ETH perpetual | 0 | 4095 | 0.01 | 0.0001 |
| BTC perpetual | 1 | 4096 | 0.1 | 0.00001 |

Create `.env.testnet` with dedicated, funded testnet credentials and set
`LIGHTER_BASE_URL=https://testnet.zklighter.elliot.ai` and
`LIGHTER_CHAIN_ID=300`. Enable `[inventory_hedge]` and configure the testnet
Hyperliquid account and agent to exercise both legs.

```bash
make trade CONFIG=config.testnet.toml
make trade MODE=testnet
```

`MODE=testnet` requires both the profile and the Lighter credential endpoint
to select testnet. `MODE=live` requires a mainnet profile.
The testnet maker anchors to the local Lighter book because testnet venues
can trade at different price levels. The hedge uses the Hyperliquid book
for its own price limits.

The session runs in the foreground. Ctrl+C and SIGTERM stop quote submission.
The supervisor keeps the feeder alive while the strategy cancels Lighter
orders, verifies its position, then closes and verifies the Hyperliquid hedge.
Successful shutdown verifies zero residual positions and open orders in the
session markets. Resting quotes have a ten-minute venue expiry in addition to
the strategy's normal shorter cancellation timer.

## Execution and recovery

- Both a single leg and worst-case net exposure, including all resting Lighter
  bids or asks that could fill, are capped by `max_unhedged_base`. The hedge
  waits for confirmed Lighter cancellations and reads actual venue positions
  before sending a bounded IOC. Position reductions use reduce-only orders.
- `deadband_base` is a running tolerance. It should account for both venues'
  lot sizes and the hedge venue's order minimum. Shutdown requires both legs
  to be flat.
- The strategy adds the configured round-trip taker fees and slippage budget
  to its maker quote spread floor. It also checks each side against fair
  value and, when hedged, the opposing Hyperliquid price after taker fees and
  slippage. A resting quote outside those limits triggers a cancellation.
  Execution depends on liquidity, venue limits and the book at order arrival.
- `pids/aleph-trade.active.jsonl` records the live session and each
  Hyperliquid order intent before dispatch. A lost acknowledgment retains
  that reservation. Recovery checks the client order ID and reconciles a
  provable terminal fill against the account position. Some terminal statuses
  do not report a provable fill size and require operator reconciliation.
- A session with unverified cleanup retains the active journal and blocks
  restart. Use the journal's client order IDs and both venues' order and
  position views to reconcile outstanding orders and inventory. After
  confirming zero open orders and zero positions, archive the active journal
  and start a fresh session. Verified cleanup archives the journal, including
  after a feeder exit that returned an error.
- `cargo run --release --bin cross_venue -- hedge-journal-audit
  pids/aleph-trade.active.jsonl [EVIDENCE.json]` explains the journal
  offline. It reads only the given files, needs no credentials and makes no
  network request. Each Hyperliquid IOC is reported as `recorded`, `matched`,
  `pending` or `invalid`, with line numbers, the possible hedge position
  range and the evidence still needed. Exit code 0 means every journaled IOC
  is explained, 2 means some remain unresolved and 3 means the journal or
  evidence is inconsistent, including a session start without a session
  ID, a positive Lighter account index or a symbol, v1 terminal timing that
  the live writer could not have produced, or a position range outside the
  decimal range (reported as null). Schema 2 journals record the Lighter
  market on session start and, with each IOC intent, the Lighter account read
  it was planned from: account, market, position, transaction time, request
  duration and age against the freshness budget, which may not exceed the
  live 5 s limit. The writer and the audit apply the same age rule, so the
  live process never journals a record the audit rejects. The report shows
  this per IOC as `journal_claim`, `unavailable` (older journals) or
  `invalid`. It is this process's own record, not venue-signed proof of
  Lighter state or time. The report never permits restart. Without Lighter
  evidence it does not cover Lighter orders or inventory, and it never
  changes the active journal.
- For a `pending` IOC, capture the raw Hyperliquid `orderStatus` response
  for its `cloid` and a later `clearinghouseState` response. Put them in an
  evidence file with `network`, `hyperliquid_account`, `coin`, `lookups`
  (`cloid`, `observed_at_ms`, `response`) and `account_snapshot`
  (`observed_at_ms`, `state`). An `unknownOid` lookup does not prove the IOC
  was never accepted.
- To check the venue's fee for a reconciled IOC, add `fills`: raw
  `userFills` or `userFillsByTime` responses (`observed_at_ms`,
  `response`). Each order's `fee` view binds fills only through the oid its
  own `orderStatus` lookups name, and sums `fee` per `feeToken` only when
  those fills cover exactly the reconciled size. Builder fees are already
  included and are shown, not added. Short coverage leaves the fee
  unattributed. Excess, conflicting or foreign fills are invalid. This is
  the venue's frozen declaration, separate from the configured-rate
  estimate in telemetry.
- For the Lighter leg of a schema 2 journal, an optional `lighter` section
  in the same evidence file takes `account_index`, `market_id`, `symbol`,
  the session's `client_order_ids`, raw `accountOrders` responses in
  `order_histories` (`observed_at_ms`, `response`) and a later raw
  `account?by=index` response in `account`. The report's `primary_leg`
  applies the REST recovery rules: each order must be terminal for that
  account and market, and the account watermark must cover the highest
  fill watermark before its position counts. It shows each order's state,
  the required and observed watermarks and the residual position. It covers
  only the IDs given, never every order on the account, and it does not
  change any Hyperliquid result. Schema 3 journals, written so far only by
  offline trials, also carry `lighter_create_batch` records synced before
  each batch with creates is sent. Their IDs are checked even when not
  declared, and each order shows whether it came from the journal, the
  caller or both. A record proves only that the batch was prepared, not
  that it reached the venue.
- To check the declared Lighter fee rate for a terminal order, add raw
  `/api/v1/trades` responses to the `lighter` section as `trades`
  (`observed_at_ms`, `response`). Each order's `fee` view binds trades only
  through the `order_index` in that order's own history entry, and needs one
  trade that alone covers the exact filled size. It takes the account's
  maker or taker role from the trade's side and `is_maker_ask`, checked
  against the order's time in force, and reads only that role's rate in
  parts per million. `estimated_fee` is `rate / 1e6 * price * size`, exact.
  Lighter omits zero or empty fields, so an absent or null rate is
  `unknown` with no amount; only an explicit `0` is a verifiable zero. Wrong
  account, market, side or client order, conflicting repeats of a trade ID
  and fills above the order's size are invalid. A short or split fill, a
  declared integrator fee or a liquidation trade stays `unknown`. The
  estimate applies the declared rate, is not the amount charged and names no
  asset.
- When the authenticated Lighter equity stream is quiet, the feeder refreshes
  balances from the account REST endpoint. Position freshness remains
  independent, and an older REST request cannot overwrite a newer WS update.
- If the feeder fails during shutdown, Lighter recovery queries the venue's
  open orders and confirms each known order by its client ID. It checks the
  account transaction watermark after fills before issuing or confirming a
  reduce-only IOC. The hedge is unwound after Lighter confirms flat.

The supervisor owns a private directory under `/dev/shm` and locks each
configured account on the host. Use each account and signing key exclusively
for the session across hosts. The legacy launch targets and relative-value
replay have separate entry points.
