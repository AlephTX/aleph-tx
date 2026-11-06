# Public perpetual data recording and replay

AlephTX can record Lighter L2 and market statistics, Hyperliquid core and
HIP-3 L2 and asset contexts, and Coinbase spot ticker quotes. The collector
writes public messages with their local receipt and availability times.
The compiler preserves the exchange publication time for books and joins only
observations available at the replay clock. Both tools run without trading keys.

```sh
make build
mkdir -p recordings
feeder/perp-capture -lighter 0 -hyperliquid ETH -coinbase ETH-USD \
  -hl-snapshot-interval 1s -duration 5m -output recordings/eth-raw.ndjson
target/release/cross_venue perp-compile examples/eth_capture.json \
  recordings/eth-raw.ndjson recordings/eth-replay.ndjson
target/release/cross_venue perp-replay recordings/eth-replay.ndjson
```

The example compiles a coverage check with zero order capacity. It selects
Hyperliquid WebSocket books by default. Set `legs[1].use_hyperliquid_snapshots`
to `true` in a copy of the configuration to select the REST books from the
same recording. The optional polling mode supports at most four markets,
with a minimum interval of one second. The exchange applies request weights
across clients on the same IP.
[Hyperliquid rate limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits)

```sh
feeder/perp-capture -lighter 110 -hyperliquid xyz:NVDA \
  -duration 5m -output recordings/nvda-raw.ndjson
target/release/cross_venue perp-compile examples/nvda_capture.json \
  recordings/nvda-raw.ndjson recordings/nvda-replay.ndjson
```

## Timing and dataset coverage

- The compiler reconstructs Lighter's exact decimal price levels. It
  invalidates the book after a nonce gap until a new snapshot arrives. The
  collector reconnects on a detected gap and records the disconnection.
- The default clock policy rejects a source timestamp ahead of receipt.
  A rejected clock observation never contributes an executable quote.
  Lighter's nonce-valid deltas still update its internal book, so a later
  timestamp-valid publication can restore execution coverage. If clocks are
  independently calibrated, `source_clock_ahead_bound_ms` subtracts a
  conservative upper bound from each exchange publication time.
- Hyperliquid's active asset context omits exchange publication time.
  `accept_receipt_time_for_oracle` opts into a receipt-time proxy for that
  context. Its L2 books always use their own source timestamps. Classify
  a venue oracle as external only for periods where its methodology is known.
- `perp-compile` reports book freshness and skew, reference and venue oracle
  coverage, sequence gaps, clock rejections and cashflow records. Joined
  inputs still pass the existing strategy checks for economics and risk.
  Compiling requires a complete raw footer and writes a new output file only.
- Supply an independent equity reference as `reference.kind = "external"`
  with observations containing `available_at_ms` and an `oracle` object
  from the replay schema. Include ordered `equity_sessions` with the
  availability time of each calendar or halt update and the checked company
  action window. A later publication cannot refresh an earlier frame.

## Research orders and funding

Review each venue's native market, contract multiplier, payout convention,
price grid, fees and collateral. The example configurations contain illustrative
economic assumptions. They do not certify live market equivalence. To enable
paper orders, explicitly set `paper_entries` and the two paper capacities.
Choose a chronological holdout via `trade_from_ms`.

Each optional `legs[i].funding` rule specifies `raw_rate_scale` and
`uncertainty_bps`. The compiler uses Lighter's current estimate and
Hyperliquid's context funding field to build at most one forecast for the next
configured payment. Once that payment time passes, a stale estimate cannot
become the next forecast. Archive settled rates separately. Supply
`settlements` as two arrays of reviewed payment records with normalized
per-interval rates, schedule times and the actual notional price used for each
payment in `oracle_price`.
The replay fails on missing cashflows for held legs. The public Hyperliquid
funding history supplies rates and timestamps, and settlement prices require
additional data. [Lighter SDK](https://github.com/elliottech/lighter-python),
[Hyperliquid funding history](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)

A capture folder is excluded from Git. Preserve the raw recording, coverage
report, exact configuration and replay report together to reproduce a run.

## First recordings

The 2026-11-06 UTC recordings contain about five minutes of ETH/BTC and
NVDA/TSLA data, plus a two minute ETH comparison with both book transports.
The table uses a 750 ms book skew limit and 2.2 s maximum book age. Joined
frames have both synchronized books and the required fresh reference inputs.

| Market and book transport | Frames | Synchronized | Joined |
| --- | ---: | ---: | ---: |
| ETH WebSocket | 1096 | 43 | 32 |
| BTC WebSocket | 1096 | 46 | 32 |
| NVDA WebSocket | 1085 | 60 | 0 |
| TSLA WebSocket | 1085 | 66 | 0 |
| ETH comparison WebSocket | 420 | 12 | 5 |
| ETH comparison REST | 420 | 40 | 5 |
| ETH comparison REST with a 50 ms clock bound | 420 | 139 | 139 |

The 50 ms bound is a sensitivity assumption. The largest rejected lead in
that recording was 31 ms. The assumed bound has no independent calibration.
ETH and BTC basis replays submitted zero orders under the recorded parameters.
The recordings did not span a settlement for a held position. The public
stock books were available, while the independent equity price check returned
HTTP 401 and company action coverage was unavailable. The RWA runs exercised
coverage checks only. [Pyth price endpoint](https://docs.pyth.network/api-reference/hermes/latest-price-updates)

## Audited funding cashflows

Public payment evidence is available for Lighter and Hyperliquid core USDC
perpetuals. A proof archives the venue responses, contract and witness policy,
the result for each requested hourly slot, and the accepted unit cashflow
intervals. The audit command reconstructs those intervals from the archive.

```sh
slot=1793991600000
target/release/cross_venue perp-funding-proof lighter \
  examples/eth_lighter_funding_evidence.json "$slot" "$slot" > recordings/llp-proof.json
target/release/cross_venue perp-funding-proof hyperliquid \
  examples/eth_funding_evidence.json "$slot" "$slot" > recordings/hlp-proof.json
target/release/cross_venue perp-funding-audit lighter recordings/llp-proof.json
target/release/cross_venue perp-funding-audit hyperliquid recordings/hlp-proof.json
target/release/cross_venue perp-compile examples/eth_capture.json \
  recordings/eth-raw.ndjson recordings/eth-settlement-replay.ndjson \
  recordings/llp-proof.json recordings/hlp-proof.json
target/release/cross_venue perp-replay recordings/eth-settlement-replay.ndjson
```

Use nominal UTC payment slots in milliseconds. A request can cover one slot
or an inclusive range of up to 24 hours. Select public witnesses with verified
market positions. Lighter checks the market's reported value and percentage
rate against signed, undiscounted position receipts from public pools. Its
funding notional uses the mark price. Hyperliquid matches the published
market payment time and rate to at least two distinct ledger witnesses and
uses the intersection of their cash per base unit intervals. Its funding
notional uses the spot oracle. The example's receipt tolerances are explicit
model inputs at the displayed quote precision. When a Hyperliquid ledger
response reaches 500 rows, the collector subdivides its time window and
retries within its public request budget.
[Lighter funding prefix sum](https://assets.lighter.xyz/whitepaper.pdf),
[Lighter position funding schema](https://github.com/elliottech/lighter-python/blob/main/docs/PositionFunding.md),
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)

An attached archive is reconciled again and matched to the replay's venue,
native market, base units, settlement currency and cadence. Pass a hyphen
in place of one proof when only the other venue is available. Only events
inside the captured replay are emitted. The replay requires evidence for
every funding slot crossed by a held leg and rejects duplicate exact-price
and witnessed records for the same slot.

Witness cash bounds are expressed for one long base unit. A short reverses
the sign and the order of the bounds. The report's funding and realized PnL
use the lower cash outcome and include separate upper bounds. The
`funding_guard_ms` interval reserves time on either side of the nominal slot.
Paper fills pause in that window and new entries require an execution
window outside it. This separates nominal replay booking time from the
exchange event time and the second precision of Lighter's public receipts.

Funding forecasts still use only observations available at the signal.
Lighter's context supplies its last settled epoch and mark price. Repeated
context messages from a completed epoch cannot become next-slot forecasts.
The witnessed payment archive contributes to cash accounting during replay.

The 2026-11-06 ETH capture crossed the 20:00 UTC payment in 33 minutes of
public data. Both clock policies emitted one audited settlement per leg in
the first frame after the slot. The strict replay joined 443 of 6,952 frames,
and the replay with an assumed 50 ms clock bound joined 2,094. Both paper
books remained flat under the configured fee and edge policy. The conditional
funding for one ETH long on Lighter and short on Hyperliquid was
0.00111796–0.00111894 USDC before trading costs. The local manifest at
`recordings/2026-11-06-settlement/manifest.json` indexes the evidence,
parameters, coverage and replay reports by SHA-256.
