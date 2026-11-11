# Supervised Lighter trading

The supervisor runs inventory-aware market making on one Lighter BTC or
ETH perpetual. With hedging enabled it uses the matching native
Hyperliquid perpetual to reduce confirmed net base exposure. The signer
and supervised session require Linux x86-64.

## Configure and validate

Set `strategy_mode` before the TOML tables.

| Profile mode | Required configuration |
| --- | --- |
| `quant` | Lighter strategy and market data. Omit `[inventory_hedge]`. |
| `inventory_hedge` | Lighter post-only quotes, matching native Hyperliquid feed and `[inventory_hedge]` limits. |

With no explicit mode, the presence of `[inventory_hedge]` selects the
hedged session. `MODE=check` resolves the mode and validates market IDs,
precision, network URLs and configuration constraints without loading
credentials or starting the feeder. [Example profile](../config.example.toml).

```sh
make trade MODE=check CONFIG=config.example.toml
```

1. Copy the example to a local profile. Set the Lighter market ID, symbol
   mapping, tick and lot from venue metadata. The shared-memory symbol ID
   and venue market ID are separate values.
2. Configure a dedicated Lighter account in a local environment file. The
   launcher reads `LIGHTER_PRIVATE_KEY` (40-byte hex),
   `LIGHTER_ACCOUNT_INDEX` and `LIGHTER_API_KEY_INDEX`. It also recognizes
   the existing `API_KEY_PRIVATE_KEY` name. Startup requires a flat position
   and zero open orders on the selected Lighter market.
3. For `inventory_hedge`, review the base cap, residual tolerance, maximum
   IOC notional, available balance and slippage. Set
   `HYPERLIQUID_ACCOUNT_ADDRESS` to the funded account and
   `HYPERLIQUID_AGENT_PRIVATE_KEY` to its approved agent key. The native
   Hyperliquid account must be flat with no open orders at startup.

## Start and stop

| Launcher mode | Default profile | Default credential file | Effect |
| --- | --- | --- | --- |
| `MODE=check` | `config.toml` | None | Validate and print the resolved mode offline |
| `MODE=testnet` | `config.testnet.toml` | `.env.testnet` | Run the supervised session against testnet |
| `MODE=live` | `config.toml` | `.env.lighter` | Run the supervised session against mainnet |

Both network modes submit and cancel orders. Run them only with
authorization for the selected network and accounts. Override the
defaults with `CONFIG=/path/to/profile.toml` and `ENV_FILE=/path/to/.env`.
Set `ENV_FILE=-` to use the process environment. Use
`make trade MODE=live` after the mainnet profile and account are verified.

### Testnet

Set `[lighter].feeder_ws_url` to `wss://testnet.zklighter.elliot.ai/stream` and
`[hyperliquid].feeder_ws_url` to `wss://api.hyperliquid-testnet.xyz/ws`. Use the
matching IDs and precision in the Lighter feed map and
`[inventory_neutral_mm]`. Verify current venue metadata before use.

| Lighter market | Mainnet ID | Testnet ID | Testnet tick | Testnet step |
| --- | ---: | ---: | ---: | ---: |
| ETH perpetual | 0 | 4095 | 0.01 | 0.0001 |
| BTC perpetual | 1 | 4096 | 0.1 | 0.00001 |

Set `LIGHTER_BASE_URL=https://testnet.zklighter.elliot.ai` and
`LIGHTER_CHAIN_ID=300` with the dedicated, funded credentials in
`.env.testnet`. Configure the Hyperliquid testnet account and agent if
exercising both legs. The launcher checks the profile and Lighter endpoint
against the selected network. The testnet maker uses local Lighter fair
value. The hedge uses the Hyperliquid book for its own limits.

```sh
make trade MODE=check CONFIG=config.testnet.toml
make trade MODE=testnet
```

### Shutdown and restart

The session runs in the foreground. Ctrl+C and SIGTERM stop new quotes.
The supervisor keeps the feeder alive while the strategy cancels Lighter
orders and verifies or closes the primary position. It then closes the
Hyperliquid hedge and verifies cleanup on both legs. Resting quotes also
have a ten-minute venue expiry.

The supervisor owns a private directory under `/dev/shm` and locks each
configured account on the host. Use each account and signing key exclusively
for the session across hosts. The legacy launch targets and paper replay use
separate entry points.

## Execution and recovery

- Both a single leg and worst-case net exposure, including all resting Lighter
  bids or asks that could fill, are capped by `max_unhedged_base`. The hedge
  waits for confirmed Lighter cancellations and reads actual venue positions
  before sending a bounded IOC. Position reductions use reduce-only orders.
- Quote allowances are refreshed by a single background REST observation.
  The strategy uses an allowance only while its Lighter position, Hyperliquid
  account and book evidence remain fresh. It can withdraw known Lighter
  orders by order index while an observation is still in flight. A hedge
  still waits for Lighter cancellation confirmation and a fresh position read.
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
- When the authenticated Lighter equity stream is quiet, the feeder
  refreshes balances through REST. Position freshness remains independent,
  and an older REST request cannot overwrite a newer WebSocket update.
- Cleanup rebases Lighter inventory. Only orders registered after that rebase
  can advance the new inventory from private fills. An unseen fill for an
  older or untracked order suspends new risk and is retained when event
  telemetry is enabled. REST recovery checks the exact order's terminal
  history and the account transaction watermark before resuming or
  completing cleanup.
- If the feeder fails during shutdown, recovery queries Lighter open
  orders and confirms known orders by client ID. It checks the account
  transaction watermark after fills before issuing or confirming a
  reduce-only IOC. The hedge is unwound after Lighter confirms flat.

## Offline evidence and audit

Build the CLI with `make build`. These commands read frozen local files
without credentials. A report describes the supplied evidence. Restart
requires a separate reconciliation of both venues.

```sh
target/release/cross_venue hedge-journal-audit JOURNAL [EVIDENCE]
target/release/cross_venue hedge-fee-view JOURNAL EVIDENCE TELEMETRY CLOID
target/release/cross_venue hedge-fee-coverage JOURNAL EVIDENCE TELEMETRY
```

| Exit code | Journal audit | Fee view and coverage |
| --- | --- | --- |
| 0 | Each journaled IOC is explained in the supplied evidence | The selected fill or every positive fill in scope binds both fee records |
| 2 | Exposure or reconciliation evidence is unresolved | Missing evidence, an unresolved outcome, a torn file or an empty coverage denominator |
| 3 | Journal or evidence is inconsistent | Records conflict or name the wrong session, account or market |

### Journal and venue evidence

The journal audit labels each Hyperliquid IOC `recorded`, `matched`,
`pending` or `invalid`, with line numbers, position bounds and missing
evidence. Schema 2 records the Lighter market on session start. Each hedge
intent stores the Lighter account, market, position, transaction time,
request duration and observation age against a freshness budget capped at
5 s. The writer and audit enforce the same age contract. The audit reports
the observation as `journal_claim`, `unavailable` for older schemas, or
`invalid`. A journal claim describes what the strategy observed. Independent
venue evidence is needed to verify the primary leg.

- For a `pending` IOC, capture the raw Hyperliquid `orderStatus` response
  for its `cloid` and a later `clearinghouseState` response. Put them in an
  evidence file with `network`, `hyperliquid_account`, `coin`, `lookups`
  (`cloid`, `observed_at_ms`, `response`) and `account_snapshot`
  (`observed_at_ms`, `state`). An `unknownOid` lookup does not prove the IOC
  was never accepted.
- To check the venue's fee for a reconciled IOC, add `fills`: raw
  `userFills` or `userFillsByTime` responses with `observed_at_ms` and
  `response`. The audit binds fills through the OID from that IOC's
  `orderStatus` lookups. It sums `fee` per `feeToken` only when the fills
  cover the exact reconciled size. The declared fee already includes any
  builder fee. Short coverage leaves the fee unattributed. Excess,
  conflicting or foreign fills invalidate the attribution.
- For the Lighter leg of a schema 2 journal, an optional `lighter` section
  in the same evidence file takes `account_index`, `market_id`, `symbol`,
  the session's `client_order_ids`, raw `accountOrders` responses in
  `order_histories` (`observed_at_ms`, `response`) and a later raw
  `account?by=index` response in `account`. The report's `primary_leg`
  checks terminal orders for the named account and market. The account
  watermark must cover the highest fill watermark before its position
  counts. The report gives each order's state, the required and observed
  watermarks and residual position for the supplied IDs. Offline
  schema 3 trials also sync `lighter_create_batch` records before a
  create batch is sent. The audit checks the IDs from both the journal
  and evidence. A batch record establishes preparation. Order history
  and account observations establish the venue outcome.
- To check the declared Lighter fee rate for a terminal order, add raw
  `/api/v1/trades` responses to the `lighter` section as `trades`
  (`observed_at_ms`, `response`). Each order's `fee` view binds trades only
  through the `order_index` in that order's own history entry, and needs one
  trade that alone covers the exact filled size. It takes the account's
  maker or taker role from the trade's side and `is_maker_ask`, checked
  against the order's time in force, and reads only that role's rate in
  parts per million. `estimated_fee` is `rate / 1e6 * price * size`, exact.
  Lighter omits zero or empty fields, so an absent or null rate is
  `unknown` with no amount. An explicit `0` records a verifiable zero. Wrong
  account, market, side or client order, conflicting repeats of a trade ID
  and fills above the order's size are invalid. A short or split fill, a
  declared integrator fee or a liquidation trade stays `unknown`. The
  estimate applies the declared rate and carries no fee asset. Actual
  charged fees require separate evidence.

## Bind and measure fee evidence

`hedge-fee-view` binds a Hyperliquid telemetry estimate and the venue fee
through one exact CLOID. The telemetry session header must agree with the
journal's session ID, network, Lighter account and market, Hyperliquid
account and symbol. Schema 1 and legacy journals lack the Lighter market
and stay unresolved. The writer exports telemetry after persisting a
positive terminal. The fill must match the journal's size, position and
price source. `fill` uses the reported average and `limit` uses the
journaled IOC limit.

A journal with a torn tail withholds fee binding. When a complete intent
survives and its terminal is missing, the terminal may be in the cut line.
That gap yields exit 2. Independently verifiable contradictions, including
a foreign session, conflicting telemetry or a side that disagrees with
the intent, still yield exit 3. In a complete journal, a telemetry fill
with no matching terminal is a contradiction. The estimate names no fee
asset, so the view labels the two amounts `not_comparable`.

`hedge-fee-coverage` reads and hashes each input once. Each row carries the
single-IOC verdict and one class from `bound`, `missing_evidence`,
`evidence_only`, `unknown_outcome`, `no_fill` or `contradiction`. The
denominator is every IOC with a positive fill and a journaled terminal.
Venue-only confirmed fills keep their declared fees under `evidence_only`.
The ratio is null when an archive finding, contradiction, orphan telemetry,
unknown outcome, evidence-only fill or empty denominator applies.
`withheld` lists the reasons. Exact repeats count once. Costs remain per
IOC and fee asset. A complete ratio covers the supplied files.

Run both fee commands on the synthetic archive with CLOID
`0x0123456789abcdef0123456789abcdef`.
[Fee audit fixture](../examples/hedge_fee_view/).
