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
per-interval rates, schedule times and actual settlement oracle prices.
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
