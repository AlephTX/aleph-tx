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

The supervised live profile uses mainnet on both venues. Lighter's current
BTC and ETH testnet market IDs exceed the perpetual market range of the
bundled signer. The launcher rejects that testnet profile. The offline test
suite exercises the hedge path against a local exchange simulator.

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
  to its maker quote spread floor. Execution still depends on available
  liquidity, venue limits and the price when each order reaches the book.
- `pids/aleph-trade.active.jsonl` records the live session and each
  Hyperliquid order intent before dispatch. A lost acknowledgment retains
  that reservation. Recovery checks the client order ID and reconciles a
  provable terminal fill against the account position. Some terminal statuses
  do not report a provable fill size and require operator reconciliation.
- A session that ends with an error retains the active journal and blocks
  restart. Use the journal's client order IDs and both venues' order and
  position views to reconcile any outstanding orders and inventory. After
  confirming zero open orders and zero positions, archive the active journal
  and start a fresh session. A clean exit archives it automatically.

The supervisor owns a private directory under `/dev/shm` and locks each
configured account on the host. Use each account and signing key exclusively
for the session across hosts. The legacy launch targets and relative-value
replay have separate entry points.
