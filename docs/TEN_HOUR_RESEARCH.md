# Continuous ten-hour research

Quant profit, hedge arbitrage and incentive turnover are separate research
programs. Each owns its configuration, execution state, capital allocation,
cash ledger and loss budget. Profit in one program cannot offset another
program's reported cost. The [source registry](RESEARCH_DATA_SOURCES.md)
preserves every known source family and the evidence needed to use it.

## Measurement contract

One validation unit has exactly 36,000,000 ms of decision time. Record
warm-up and terminal drain separately. Carry strategy state, open orders,
inventory, cash, loss controls and funding across the entire interval.
Storage shards may change; trading state must not restart with each shard.
Ten flat-start hourly runs remain ten short diagnostics.

Freeze the configuration and window before execution. Previously inspected
history is historical validation, not untouched out-of-sample evidence.
Paired variants may share tape, but repeated configurations do not create
additional independent market time. Source gaps remain visible, including
gaps caused by collector restart or subscription refresh.

| Program | Next controlled comparison | Required economic result |
| --- | --- | --- |
| Quant profit | Existing ETH/BTC baseline versus event participation filters; inspect fill-conditioned markout, actual quote changes and queue loss | Net dollars/hour, net bps and capacity after fees, funding and final exits; isolate directional inventory PnL |
| Hedge arbitrage | Basis-reversion baseline; funding carry and fixed-settlement products remain separately labeled hypotheses | Executable full-cycle cash flows, leg completion, capital requirements and rejection reasons; forecasts cannot become settled income |
| Incentive turnover | Extended/Core and separate Robinhood-domain research; compare `neutral`, `long_first`, `short_first` with the same risk limits | Target-venue volume, complete net cost, natural Maker exits, forced exits and exposure duration; points and unconfirmed rebates remain zero |

The turnover postures select which side opens on the target venue. Confirmed
fills remain fully hedged; `long_first` does not authorize residual net-long
inventory. Each posture has its own ledger. External instructions retain
their actual availability time, revision and expiry. See the
[control contract](INDEPENDENT_STRATEGY_RESEARCH.md#external-control-contract).

The $500 and $1,000 scenarios must include both venues' collateral and
external reserves. Initial $50/$100 orders are size controls, not total
deployed capital. Returns cannot use gross turnover as the capital
denominator, and changing capital alone cannot repair negative net bps.

## Inventory and qualification tools

Run these commands only against reviewed public research directories:

The research scripts require Python 3.11 or newer. `make test-research`
runs their offline synthetic regressions and is included in `make test`.

```sh
python3 scripts/inventory_public_sources.py /path/to/public-research \
  --output /path/to/new-inventory.json
python3 scripts/research_windows.py /path/to/source-catalog.json \
  /path/to/run-declarations.json --as-of-ms OBSERVATION_TIME_MS
target/release/cross_venue maker-first-preflight /path/to/config.json
```

The inventory hashes stable files, retains duplicate aliases and refuses to
overwrite an existing catalog. Its allowlist covers known raw captures and
specific public market/fee metadata. Compiled outputs, independent equity
evidence and unknown directories require their own lineage manifests; they
are not silently certified by this tool. Active partial files are excluded.
Content hashes prove identity, not stream completeness or tradability.

The qualification tool consumes a reviewed source catalog and run
declarations; it does not derive those facts from raw tape. The synthetic
schema and examples are in
[`test_research_windows.py`](../scripts/test_research_windows.py).
It checks exact duration, per-stream coverage and explicit gaps, domain and
instrument identity, fee/funding evidence, state continuity, final exits,
selection timing and independent ledgers. Exit code 2 means qualification
failed. Unverified coverage remains unverified even when file timestamps
span ten hours.

For explicit funding avoidance, the declaration uses
`execution.funding_mode = "funding_avoidance"`. It requires reviewed public
`funding_schedule` metadata, run-bound audit hashes and every expected
settlement slot. Each proof covers maximum absolute positions, outstanding
orders/IOCs/fill notices and active cycles throughout the guard, plus source
watermarks and guard-interval coverage. A flat snapshot or matching count is
insufficient. `avoidance_fixture()` in the schema test is an executable
example. The default path still requires actual settlement evidence.

`maker-first-preflight` checks configuration capability only. Exit code 0
means that the inspection ran; the caller must read `execution_supported`.
An unsupported ten-hour window returns its funding-slot counts and missing
engine/evidence requirements. It does not start a replay or submit orders.

## Continuous execution modes

All three modes below are offline research paths. Existing live execution
and account permissions are unchanged.

**Quant:** the recorded production strategy accepts an explicit
`flat_at_funding` policy and optional `trade_from_ms`. Initialization runs
once; signals, cash, risk high watermarks and the daily loss baseline survive
all funding pauses. A new order is checked at its actual simulated arrival,
including batches that awaited across a pause boundary. Explicit trade-feed
state, source-time delivery bounds and the full expected settlement-slot
set are required. Missing or skipped boundaries invalidate the result.

```json
"flat_at_funding": {
  "interval_ms": 3600000,
  "offset_ms": 0,
  "drain_before_ms": 15000,
  "guard_ms": 1000,
  "max_event_gap_ms": 5000
}
```

Cancelled orders retain their remaining quantity and queue until the
bounded late-print window expires. Cleanup then waits for an account
publication beyond that watermark, so an old zero-position snapshot cannot
clear a newly received fill. Prices and receipt/source times remain
separate. Future source times and trades potentially affecting our orders
beyond the delivery bound cannot be silently dropped.

**Turnover:** Maker-first accepts
`"flat_at_funding": {"settlement_guard_ms": 1000}`. It drains before each
leg's configured payment, resolves all orders and notices, and requires
both positions to be zero through the guard. Timers fire independently of
market events. Resumption requires fresh post-guard books. Cash, fees,
turnover budgets and realized-loss limits remain cumulative. Default
configurations still reject funding crossings; the explicit mode proves
zero exposure at payment, rather than estimating a missing payment.

**Arbitrage:** optional `trade_until_ms` and `drain_until_ms` define the
stop-entry boundary and terminal execution interval. Existing submissions
must resolve against real subsequent depth. Late partial fills are hedged
by actual quantity before both legs exit. The report's
`terminal_drain.complete` requires cutoff coverage, no active lifecycle,
zero positions and no execution failure. An EOF liquidation estimate
cannot substitute for this check. Funding evidence remains mandatory for
any leg held across settlement.

Reproduce the independent historical programs with reviewed public paths:

```sh
cargo build --locked --release --features research-replay \
  --bin cross_venue --bin maker_replay
python3 scripts/replay_ten_hour_scenarios.py /path/to/campaign \
  target/release/maker_replay target/release/cross_venue \
  config.example.toml /path/to/public-market-terms.json /path/to/new-study
python3 scripts/replay_ten_hour_arbitrage.py target/release/cross_venue \
  /path/to/BTC-freshest.ndjson.zst /path/to/new-arbitrage-study
```

The turnover capital cases set order, loss, fee and volume budgets; they
are not an account-level margin stress test. No return on collateral or
annual return is inferred from those labels.

The runners preserve source hashes, configuration and executable identity,
receipt-time controls, negative results and incomplete executions. Their
execution/production qualification remains false until separate source
quality and account-cost evidence are reviewed. Successful simulation does
not establish actual venue fills or incentive credit.

## Oasis evidence — 2026-11-13

An allowlisted content inventory found 340 unique public market archives
and 12 unique market/fee metadata artifacts: 352 unique artifacts totaling
1,758,840,452 bytes (about 1.64 GiB). Two duplicate aliases are retained.
These figures exclude derived studies, active partials and evidence outside
the allowlist. The earlier roughly 21 GiB directory footprint includes
derived data and is not unique raw-market volume.

The immutable inventory is stored outside Git at
`~/aleph-tx-research/public-inventory-aab0989623c6/catalog.json`, hash
`3d1b1d7ba445af4762be377c0c618244a9e1e4325515c5862215d727b4428d4c`.
It does not claim verified depth/trade/funding coverage.

Initial basis diagnostics selected the same ten-hour historical interval
for BTC and ETH, from `1794413350543` to `1794449350543` ms, with a separate
60-second warm-up. Each asset used one uninterrupted ledger. Both had zero
entries and zero positions; best modeled net edge was approximately
−14.71 bps for BTC and −15.32 bps for ETH. These are assumptions-based
Core Standard/native Hyperliquid results, not evidence against every
possible strategy or fee structure.

The initial driver is retained as `tenhour-8ad3bc00685c` under the historical
campaign's `results` directory. Its source cutoff flag included an
unexecuted boundary frame. Consequently it is **not accepted as a complete
ten-hour execution**. The corrected driver distinguishes consumed frames
and cutoff evidence, caps the source's available budget rather than
overwriting it, and supplies real terminal-drain frames.
The historical tapes contain no realized funding events in this interval;
zero funding cash flow is explained by zero holdings, not by free funding.

### Completed independent diagnostic matrix

Oasis run `continuous10h-d4f1873ca8b3` used the new continuous engines and
completed all 30 configured executions. Its 148-file source snapshot and
executable hashes are retained in `identity.json` and `status.json`.
Additional test cases and a source-admission check were added afterwards;
the executed Rust strategy and matching code are unchanged.

| Program | Independent cases | Observed result | Accepted interpretation |
| --- | ---: | --- | --- |
| Quant | BTC/ETH × $500/$1,000 capital, $50/$100 orders: 4 | No Maker fills; the funding guard stopped because trade-feed evidence was unknown | Data-blocked; no valid profitability measurement |
| Basis arbitrage | BTC/ETH: 2 | Ten-hour decisions plus 60 s real drain completed; zero entries; best net edge −14.71 / −15.32 bps | No economic entry under the declared fees/latency; account fees and complete source quality remain unverified |
| Turnover | BTC/ETH × three postures × two capital/size cases × 0/4 bps cost allowances: 24 | No Maker fills or target volume; target trade feed remained unknown | Data-blocked; zero engine cash movement is not verified zero-cost turnover |

Quant/turnover use decision interval `1794413890543..1794449890543` ms,
with 60 s prior quant warm-up and 15 s terminal drain. Arbitrage uses the
interval above, beginning nine minutes earlier, with its own 60 s warm-up
and 60 s drain. They reuse the same historical corpus, not identical
independent observations. No result is labeled untouched out-of-sample.

The data blocker is now explicit. The old campaign contains **617,932
Lighter, 85,296 Arcus and 705,621 Hyperliquid public trade records across
its recorded instruments**, but no `feed_state` records for any of those
three sources. A cached print cannot establish that a disconnected feed
has recovered. Earlier input builders inferred connectivity from prints;
the strict continuous builder no longer does so. It also retains future
source timestamps for rejection and verifies the actual Arcus depth/trade
envelope before creating an end marker.

For the 10-hour turnover cases, the `trade_feed` gate rejected 220,736 BTC
and 157,362 ETH event decisions per arm. The two-leg flat checks passed at
ten scheduled slots because no orders were admitted. That proves neither
profitable execution nor usable trade-stream coverage. The next runner
revision rejects missing lifecycle evidence during source admission, before
wasting a parameter matrix on an unavailable feed.

`qualification-audit.json` preserves all original results but sets accepted
quant/turnover profit and target cost to unknown. It binds the parent report
hash and public feed audit. The separate arbitrage diagnostics remain zero
entry with explicit cost and coverage limitations. No cross-program PnL is
combined and no annual return is inferred.

Linux research regression passed after restoring four omitted, checked-in
synthetic telemetry fixtures to the isolated source snapshot. The initial
failure log and supplemental fixture hashes are retained. This was a
packaging omission; execution source was unchanged.

The new public campaign `2026-11-13-continuous10h-153508` began around
2026-11-13 23:35 Beijing time. It requests 10 hours 15 minutes for warm-up,
ten-hour evaluation and final drain, ending around 2026-11-14 09:50.
Actual first/last usable events determine qualification after sealing.
It captures Core and Robinhood Lighter BTC/ETH books and trades, native
Hyperliquid WebSocket books with one-second REST snapshots, Extended
BTC/ETH depth/trades and Coinbase reference prices. Lighter clock evidence
refreshes every four minutes; those refreshes do not waive gap checks.

A read-only inspection of the first approximately 38.5 minutes found two
subscription acknowledgments each for Core trades, Robinhood trades and
Extended trades, with no trade-feed disconnect event in that inspected
prefix. Clock-refresh events are recorded separately. This confirms that
the new format retains lifecycle evidence missing from the historical
trade-only archives; it is not a sealed ten-hour coverage certificate.

This collector streams gzip in one process, avoiding the previous
15-minute process-roll boundaries. It runs at low priority with disk and
file-size guards. Existing collectors remain in place. Early termination,
disk guard activation or CRC failure leaves an incomplete artifact; a
`collector:end` marker alone does not establish the requested duration.

## Continuous compressed capture

The recorder accepts `-gzip` independently of the output filename:

```sh
(cd feeder && go build -o /tmp/perp-capture ./cmd/perp-capture)
/tmp/perp-capture -lighter 1,0 -lighter-trades \
  -lighter-clock-refresh 4m -hyperliquid BTC,ETH \
  -hl-snapshot-interval 1s -extended BTC-USD,ETH-USD \
  -duration 10h15m -gzip -output /path/to/new-public.ndjson.gz.partial
```

Preserve partial files until the complete gzip stream,
duration, exit reason, source identity and per-stream continuity are checked.
Only then hash and seal. Compilation must consume the decompressor to EOF
and check its return status/CRC; stopping at the final JSON event is
insufficient. Raw public data and detailed manifests stay outside Git.

The next acceptance milestone is one independently qualified ten-hour unit
per program, followed by untouched periods under the same frozen policy.
Positive net economics remains the objective. Zero trades, missing evidence
and losing results remain visible rather than being optimized out of the
report.
