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
Hyperliquid WebSocket books by default. Compare transports on the same
recording by setting `legs[1].hyperliquid_book_mode` to `websocket`, `snapshot`
or `freshest`. Existing configurations with `use_hyperliquid_snapshots: true`
select REST and omit the new field. The `freshest` mode accepts a book from
either transport once it has arrived and its exchange timestamp exceeds the
last accepted publication. A disconnected transport invalidates its own
current book, and the next publication must still advance that timestamp.
The optional REST polling supports at most four markets with a minimum
interval of one second. The exchange applies request weights across clients
on the same IP.
[Hyperliquid rate limits](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/rate-limits-and-user-limits)

```sh
feeder/perp-capture -lighter 110 -hyperliquid xyz:NVDA \
  -duration 5m -output recordings/nvda-raw.ndjson
target/release/cross_venue perp-compile examples/nvda_capture.json \
  recordings/nvda-raw.ndjson recordings/nvda-replay.ndjson
```

To check the Entropy feed on the same Lighter market, copy the NVDA profile
and change `pair.legs[1].venue` to `entropy` and `pair.legs[1].market` to
`io:NVDA`. Set `legs[1].hyperliquid_book_mode` to `freshest` and leave
`paper_entries` disabled for the coverage check.

```sh
feeder/perp-capture -lighter 110 -hyperliquid io:NVDA \
  -lighter-clock-refresh 2m -hl-snapshot-interval 1s -duration 5m \
  -output recordings/entropy-nvda-raw.ndjson
target/release/cross_venue perp-compile recordings/entropy-nvda-config.json \
  recordings/entropy-nvda-raw.ndjson recordings/entropy-nvda-replay.ndjson
```

Enable Lighter calibration on the copied profile with `legs[0].lighter_clock`
as described below. Review Entropy's contract economics and session-specific
oracle before using the capture for paper entries.

## Timing and dataset coverage

- The recorder shares WebSocket keepalives with the production feeder.
  Hyperliquid also receives its application heartbeat. A failed transport
  probe ends the connection and triggers reconnect. Heartbeats are excluded
  from market publications and preserve the age of the last quote.
  [Hyperliquid heartbeats](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/timeouts-and-heartbeats)
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
  coverage, sequence gaps, clock rejections and cashflow records.
  `book_age_at_availability_ms` describes valid accepted books at the time
  they became available, including any configured clock correction.
  `fresh_book_skew_ms` describes the source-time difference on frames where
  both books are fresh, including frames beyond the skew limit. Both fields
  report percentile upper bounds from fixed buckets and the observed maximum.
  Joined inputs still pass the strategy checks for economics and risk.
  Compiling requires a complete raw footer and writes a new output file only.
- Supply an independent equity reference as `reference.kind = "external"`
  with observations containing `available_at_ms` and an `oracle` object
  from the replay schema. Include ordered `equity_sessions` with the
  availability time of each calendar or halt update and the checked company
  action window. A later publication cannot refresh an earlier frame.

## Lighter subscription clock evidence

New Lighter recordings include `request_sent_at_ms` on matched subscription
replies. With local send time `s`, receipt time `r`, and the venue publication
time `p`, the publication clock offset lies between `p - r` and `p - s`,
assuming the reply is stamped after the matching request. The compiler
expands each interval for millisecond precision and `drift_margin_ms`, then
intersects the eligible intervals for that leg and connection. It subtracts
the positive upper bound from the venue timestamp. The policy applies only
after the evidence arrives and expires after `valid_for_ms`. Missing or
conflicting evidence makes that leg unavailable, and disconnects reset it.

Set `legs[i].lighter_clock` on a Lighter leg to opt in. The measured run used
`{"max_rtt_ms":750,"valid_for_ms":360000,"drift_margin_ms":10}`. The compiler
checks that the committer stamp precedes the publication stamp and retains
that observed lag when shifting the book. This uses the assumption that the
publication clock bound also covers the committer clock. The request bounds
the publication clock directly. Older recordings without the probe field can
still use the strict clock policy or an explicit sensitivity bound.
[Lighter timestamp update](https://t.me/s/lighter_api_updates?before=119)

## Archived equity reference and sessions

For paper research, `scripts/prepare_equity_replay.py` attaches an archived
Nasdaq intraday chart and a reviewed session to a capture configuration.
It checks the equity and date, converts the chart's New York wall times to
UTC, and filters the regular session. Supply the open and close for the
specific day, including any early close, and review the Nasdaq holiday,
halt, dividend and split records for the proposed coverage window.
[Nasdaq market hours](https://www.nasdaqtrader.com/content/technicalsupport/nasdaq_sys_hours.pdf),
[trading calendar](https://nasdaqtrader.com/Trader.aspx?id=Calendar),
[halt RSS](https://www.nasdaqtrader.com/Trader.aspx?id=TradeHaltRSS),
[market data and corporate actions](https://www.nasdaq.com/market-activity/economic-calendar)

```json
{
  "underlying_id": "NASDAQ:NVDA",
  "trade_date": "2026-11-06",
  "opens_et": "09:30",
  "closes_et": "16:00",
  "actions_clear_through_et": "16:00",
  "halts": []
}
```

An entry in `halts` has `starts_et` and `resumes_et` local time strings.
Use `null` for a halt without an observed resumption. The compiler applies
each halt or resumption at its effective time in a historical run.

```sh
python3 scripts/prepare_equity_replay.py examples/nvda_capture.json \
  recordings/nasdaq-nvda-chart.json recordings/nvda-session.json recordings/nvda-reviewed.json
target/release/cross_venue perp-compile recordings/nvda-reviewed.json \
  recordings/nvda-raw.ndjson recordings/nvda-reviewed.ndjson
target/release/cross_venue perp-replay recordings/nvda-reviewed.ndjson
```

The importer keeps the base config's paper order policy and venue oracle
settings. Each minute price becomes an `equity_minute_close` only after the
minute ends and the modeled publication delay elapses. The default delay is
60 seconds. The separate reference age allows one minute between updates.
Venue book and oracle limits still apply. Archived charts carry trade-minute
labels, so the delay is a research assumption. Point-in-time quote archives
can use the `external` reference path with their observed arrival times.
[Nasdaq market data](https://www.nasdaq.com/market-activity)

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

## Recorder keepalive and transport comparison

The 2026-11-07 UTC capture recorded ETH, BTC and NVDA for six minutes with
zero disconnects and zero Lighter book nonce gaps. All comparisons below use
the same 1,323 frames per pair, the strict clock policy, a maximum book age of
2.2 seconds and a maximum book skew of 750 ms.

| Hyperliquid book mode | ETH joined | BTC joined | NVDA synchronized |
| --- | ---: | ---: | ---: |
| WebSocket | 3 | 6 | 144 |
| REST | 19 | 20 | 564 |
| Freshest | 21 | 24 | 606 |

The largest rejected Lighter clock lead was 29 ms. Under the separate 50 ms
sensitivity assumption, freshest joined 546 ETH and 562 BTC frames. Both
crypto pairs stayed flat in every paper holdout under the configured fees
and edge thresholds. NVDA was a book coverage check pending independent
equity reference and session data. The local manifest at
`recordings/2026-11-07-heartbeat/manifest.json` indexes the capture and all 18
replays by SHA-256.

## Measured clock and equity session replays

The five-minute clock capture on November 7 used the same Hyperliquid dual
book source and strategy settings across both clock policies. Two Lighter
replies per market gave padded publication clock intervals of −267 to 188 ms
for ETH and −266 to 190 ms for BTC. The compiler used the conservative upper
end of each interval. All 629 ETH and 985 BTC Lighter clock rejections seen
under the strict policy cleared with these connection-specific bounds.

| Market | Frames | Strict joined | Joined with subscription evidence |
| --- | ---: | ---: | ---: |
| ETH | 1,034 | 238 | 538 |
| BTC | 1,034 | 189 | 545 |

For the earlier November 6 equity capture, Nasdaq and the archived stock
charts put the regular session at 14:30–21:00 UTC. The halt, ex-dividend and
split records reviewed for that day did not list NVDA or TSLA. Each chart
supplied 390 regular-session minute prices. The older venue recording lacks
subscription probes, so its primary results use the strict clock policy.

| Market | Frames | Strict joined | Joined with a 50 ms sensitivity bound |
| --- | ---: | ---: | ---: |
| NVDA | 1,085 | 39 | 91 |
| TSLA | 1,085 | 40 | 101 |

The real after-hours NVDA control contributed zero eligible session frames
out of 1,323. Paper entries remained zero across all samples. Sparse equity
books and oracle coverage limited estimator warmup in the equity holdouts.
Contract equivalence and fees retain the replay configurations' research
assumptions. The local `recordings/2026-11-07-rwa/manifest.json` preserves
the chart and session sources, clock replies, configurations and reports.

## Longer RWA capture and execution checks

Choose the recording window from the reviewed underlying session. For an
hourly funding schedule, a 57-minute window from 10:01 to 10:58 New York time
fits between payments. Keep a fixed training cutoff and apply it through
`trade_from_ms`. The example below records the two equity markets with
independent per-market REST polling and the five-level WebSocket feed.

```sh
feeder/perp-capture -lighter 110,112 -lighter-clock-refresh 4m \
  -hyperliquid xyz:NVDA,xyz:TSLA -hl-fast-book -hl-snapshot-interval 1s \
  -duration 57m -output recordings/equity-session.ndjson
```

Use `hyperliquid_book_mode: "freshest"` on the Hyperliquid leg. An accepted
WebSocket book contributes its five displayed levels and a REST book
contributes the levels in that snapshot. The recorder polls the two REST
books concurrently. On Lighter, use a calibration lifetime longer than the
refresh interval. Each planned reconnect is counted in
`lighter_clock_refreshes` and invalidates the previous book and calibration
until new subscription evidence arrives. A 125-second transport check on
both equity markets received five-level WebSocket books about every 539 ms
at the median. Four scheduled Lighter refreshes yielded new clock evidence.
[Hyperliquid subscriptions](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/websocket/subscriptions)

The estimator skips samples with temporarily stale external oracles. It
resets after a gap beyond `basis.max_sample_gap_ms`, a session interruption,
an oracle mismatch or a regressed book. A 15-second sample gap policy on the
previous regular-session recording raised the longest uninterrupted sample
sequence from 2 to 23 for NVDA and from 2 to 22 for TSLA. The holdout reached
one basis-ready frame on NVDA and submitted zero paper orders on both names.

`holdout_basis_ready_frames` counts scored observations after training.
`entry_signals` counts executable plans evaluated while the runner is flat.
`paired_entries` counts balanced opens, including partial opens retained at a
valid close lot. The opening and closing execution arrays follow the pair's
leg order. Slippage is the signed cost against the same-side touch at IOC
submission. Depth impact uses the touch of the later book that simulated
the fill. The report records fills without a valid submission touch
separately. Unhedged size, duration and any halt remain visible beside the
fill counts.

For a replay with zero entries, inspect `entry_diagnostics.blocked_by_gate`.
It counts the first blocking check on each flat holdout frame. Per-leg
failures use the pair's leg order in `blocked_by_leg`. The reported maximum
absolute z-score is from flat scored frames. The best net edge is measured
after size and budget checks and includes candidates below the configured
edge threshold. A missing edge value means no candidate reached that check.
For an IOC that resolves without a fill, `unfilled_by_gate` records its
terminal price or depth check, or the last condition it was waiting for at
expiry. An outstanding IOC at the end of the recording stays in its open
phase.

On the five-minute November 6 recording, each 15-second gap replay had 648
holdout frames and zero submitted IOC orders. The first blocking checks were
as follows.

| First gate | NVDA | TSLA |
| --- | ---: | ---: |
| Lighter book missing | 335 | 270 |
| Hyperliquid book stale | 208 | 244 |
| Pair timestamp skew | 83 | 108 |
| Lighter oracle stale | 21 | 26 |
| Basis below entry threshold | 1 | 0 |

NVDA's one scored holdout frame had an absolute z-score of about 0.266,
below the configured 2.5 entry threshold. TSLA's holdout never reached
scoring. Neither sample reached the net edge calculation.
