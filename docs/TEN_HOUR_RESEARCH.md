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
| Incentive turnover | Prioritize Robinhood/Core; retain Extended/Core as a historical control. Compare `neutral`, `long_first`, `short_first` at frozen cost budgets | Target-venue volume, complete net cost, natural Maker exits, forced exits and exposure duration; points and unconfirmed rebates remain zero |

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

### Core local participation probe

Build the offline engine with `cargo build --locked --release --features
research-replay --bin maker_replay`, then run:

```sh
python3 scripts/replay_local_participation.py \
  /path/to/maker_replay /path/to/ten-hour-scenarios /path/to/new-comparison

# Separate fixed sensitivity: retain an entry permit for at most one second.
python3 scripts/replay_local_participation.py \
  /path/to/maker_replay /path/to/ten-hour-scenarios /path/to/new-hold-comparison \
  --entry-hold-ms 1000

# Six arms: original control, Core-only control, Core-only reversal gate.
python3 scripts/replay_local_participation.py \
  /path/to/maker_replay /path/to/ten-hour-scenarios /path/to/new-core-comparison \
  --core-only
```

The runner freezes four independent ledgers before executing any arm:
$500/$1,000 ETH controls and the same budgets with a local reversal entry
gate. The inspected input window is explicitly historical selection, not
a holdout. No fitted model or parameter search is claimed. Controls must
reproduce the parent net PnL and both turnover totals before the comparison
can complete. Source, engine and config hashes bind the experiment.

By default, only the entry gate uses Core-local features; baseline fair
value and risk still use their original reference. With `--core-only`,
each size instead has three arms: the original external-reference control,
a Core-only fair-value control, and that same Core-only control with the
entry gate. The engine does not publish external books in Core-only mode,
allows them to be absent, and rejects external OBI or reference-dependent
reserve/regime overlays. Synthetic tests require identical reports with
ordinary, displaced and absent external books. The report records
`fair_value_mode`; the same-fair-value comparison isolates the entry gate.

The gate uses a two-second lookback,
500 ms recovery interval, 500 ms source-age ceiling, 0.5 bps initial price
move, 0.1 bps recovery, 0.60 trade-flow imbalance and $1,000 initial flow.
Buy entries require selling pressure followed by price and flow recovery;
sell entries mirror those conditions. These thresholds are hypotheses.

A bounded ring retains 100 ms receipt buckets; the current incomplete flow
bucket is excluded. Future, pre-subscription and over-age trade timestamps
cannot supply predictors. A disconnect clears history and requires fresh
warm-up. Insufficient or non-reversing evidence withdraws entry targets.
Both confirmed fills and account publications must agree a side cannot
reduce inventory before the gate withdraws it. Existing cancellation,
pending exposure, exit, funding and loss controls remain responsible for
execution. The gate is available only under `research-replay`.

The default `entry_hold_ms = 0` withdraws an entry when the reversal
condition disappears. The separate 1,000 ms sensitivity retains each
side's permit until its original expiry while evidence remains ready.
Repeated positive samples before expiry do not extend that permit. Unknown
or stale evidence clears it immediately. Holding participation does not
impose a minimum order lifetime: the baseline may still reprice, cancel,
reduce risk or exit. Reports count targets admitted only by a held permit
separately. Both policies remain historical selection experiments.

Reports expose signal-ready decisions and admitted/withdrawn entry targets.
Optional execution diagnostics now measure Maker markout at 1, 5 and 30
seconds after simulated fill receipt, with separate cancel-pending cohorts
and turnover denominators. A label requires a book source time at or after
its horizon and receipt within the following one second. Expired and
unresolved end-of-tape labels remain explicit; they are not zero returns.
These are fill-conditioned diagnostics, not training labels for every
decision or realized round-trip PnL. Any subsequent fitted model needs an
earlier training period and an additional continuous ten-hour validation
window; splitting this ten-hour input into two five-hour runs would not
satisfy the research contract.

### Turnover cost versus observed volume

```sh
python3 scripts/replay_turnover_cost_curve.py \
  /path/to/cross_venue /path/to/prepared-rh-core /path/to/new-curve
```

The prepared manifest binds BTC/ETH tapes and native pair metadata by hash,
with explicit gaps, receipt ordering, window and source lineage. The runner
requires the RH/Core route and an explicit **unverified USDG/USDC parity**
research assumption. It does not qualify collateral conversion, account fees,
funding terms, incentive eligibility or executable profit.

Before starting an engine, freeze all 72 independent ledgers: two assets,
two capital/order sizes, three opening postures, and 0/0.5/1/2/4/8 bps
admission budgets. These correspond to $0/$50/$100/$200/$400/$800 per
million dollars of target-venue volume. A budget is a modeled admission
allowance, not a cap on realized loss or an observed cost estimate. Preserve
the original independent turnover, fee and loss limits. Do not pool volume
across arms or discard feed-gap failures. The 750 ms trade preflight runs
once per input; passing timing does not certify feed continuity.

`diagnose_cycles` adds optional offline attribution without changing orders:

- Entry Maker, entry hedge, Maker close, hedge close and forced-close
  execution costs use each leg's fresh fill-time midpoint. Their signed sum
  plus both legs' price drift reconciles exactly to flat-cycle price loss.
- Explicit fees and the 0.25 bps modeled overhead on gross turnover are
  separate. Overhead is an uncalibrated research assumption, not an exchange
  charge. Actual target-venue cost per million is `-net / target_volume * 1e6`.
- Entry 1 s/5 s mid-price moves and hedge slippage against submission touch
  are overlapping diagnostics, **not additional cash costs**. Missing,
  expired and end-of-tape labels remain explicit.

Each completed cycle must reconcile in Rust's exact decimal cash ledger.
New diagnostic amounts serialize as decimal strings. Historical float JSON
can pass aggregate comparison only within the sum of individual float ULPs,
and only when every engine-level exact reconciliation already passed.
Incomplete ledgers have unknown full-window net cost; zero volume has no
cost-per-volume estimate. Report sparse samples and all tested variants.
Raw per-cycle reports remain outside Git. An observed curve on the selected
window is not an estimate of production capacity or a holdout result.

Quant reports also expose pre-overhead net PnL and fixed-ledger sensitivities
at 0/0.1/0.25/0.5 bps. This reprices the same executions; it does not simulate
how different fees or latency could change order decisions.

### Cancellation, account-tier and reward sensitivities

```sh
python3 scripts/replay_cancel_guard.py \
  /path/to/cross_venue /path/to/rh-turnover-cost-curve-job /path/to/new-guard-study
python3 scripts/replay_quant_execution_sensitivity.py \
  /path/to/maker_replay /path/to/core-only-comparison \
  /path/to/quant-ETH.ndjson /path/to/new-execution-study
```

The optional paper-only `conservative_cancel_guard` preserves the old
strict-before-cancel model by default. With it enabled, a print can match a
cancelled quote when `source_ms - guard <= cancel_effective_ms`, instead of
requiring `source_ms + guard < cancel_effective_ms`. Activation, source-time
validity, queue consumption and delivery bounds still apply. The tombstone
remains reserved through the last admissible receipt, including equality;
confirmed inventory and pending fill notices survive its eventual removal.
Reports count overlap prints, actual overlap fills/turnover and fills after
a cancellation request. Extra fills are not automatically worse PnL, so
neither matching mode is claimed to bound realized profitability.

The guard runner freezes all 72 conservative RH/Core arms and 12 strict
2 bps controls before execution. Strict controls must reproduce the parent
cycle counts, target turnover and full-ledger net cash. Original gaps and
failed runs remain visible; no gap is repaired by a clock-policy change.

The separate quant runner freezes eight Core-only ETH ledgers: Standard,
Premium counterfactual, Standard TTL 15 s and Standard TTL 30 s, at each of
two sizes. Premium uses current zero-stake public assumptions of 0.4/2.8 bps
Maker/Taker fees and 0/140 ms additional cancel/Taker latency. Transport
remains 250 ms. TTL changes the existing replacement-lifetime knob; risk
withdrawals and exits retain their immediate authority. These are historical
selection experiments and do not switch any account or modify an existing
prospective window. [Published fee/latency schedule](https://docs.lighter.xyz/trading/trading-fees)

RH **target turnover is not confirmed reward-eligible turnover**. Its
official program excludes Standard-to-Standard activity; other exclusions
and venue discretion still apply even if one side is Premium. The public
execution tape does not certify counterparty tiers. Reports keep confirmed
eligibility unknown and provide separate 0/20/50/80/100% denominator scenarios
on the rule-window candidate volume. They hold orders and cash fixed, never
value points, and do not retest admission with discounted eligible volume.
Zero assumed eligible volume has undefined cost per eligible dollar, not
zero cost. [RH points rules](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points)

Forced-close attribution reports cycle/fill counts and signed execution cost.
Its fraction of total cost is defined only for positive full-ledger cost and
can exceed 100% when other components offset it. Price drift, fees and overhead
remain separate. This descriptive fraction is not a calibrated rule for
relaxing unhedged timeouts, basis stops or funding guards.

New cycle diagnostics also serialize whole-ledger net amounts as decimal
strings and check the sum of completed cycles against the engine's exact
cash, before and after overhead. Python requires that proof when present;
it does not fall back to a float tolerance if the exact proof fails. Legacy
reports retain their original representation and bounded rounding check.
A representation failure is preserved and reviewed separately with an
unchanged-parameter replay and economic/cycle parity, rather than loosening
the tolerance after observing a failed result.

### Execution lifecycle

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

### Independent scheduling and input admission

`replay_ten_hour_scenarios.py --program quant` and `--program turnover`
select independent programs; `--symbols ETH` restricts the asset. Defaults
retain both programs and both assets. A turnover-only job does not load the
quant engine or strategy configuration, although their positional arguments
remain for CLI compatibility. It still uses the hash-bound baseline capture
profile for the Core market mapping. A failed turnover input cannot force a
repeat of completed quant ledgers.

Before a turnover matrix runs, the runner freezes all configurations and
audits the common input once. The audit binds the exact input bytes and
timing policy, consumes the terminal marker, rejects changes during reading,
and counts future or over-bound source times per leg. It follows the engine's
Maker-leg funding check: a print strictly older than the flat-start ledger is
excluded; a print at the start remains subject to the bound. It preserves
disconnects and delayed prints. Hedge-leg delay statistics remain visible,
but only Maker-leg timing failures reject this particular gate.

```sh
python3 scripts/replay_trade_preflight.py /path/to/maker-config.json \
  /path/to/normalized-input.ndjson /path/to/new-timing-audit.json
```

Standalone exit code 2 means timing admission failed. The matrix runner
records every rejected arm, its config hash, the shared audit hash and
`execution_started: false`; PnL stays unknown. The runner exits 1 for rejected
or incomplete runs, even if its report was successfully written. Existing
reports and prior failed executions are never reclassified by a new audit.
Passing this gate does not prove feed continuity, book quality, settlement,
fees, completed execution or profitability. Exposure-dependent gap checks
remain in the engine.

The default delivery allowance remains **750 ms**. An explicit
`--turnover-trade-delivery-ms 2000` is a separately archived historical
sensitivity. The same late prints now remain eligible for a longer cancel
and funding drain lifecycle; they are not deleted or retimestamped. The
15-second drain and all notional/loss limits remain unchanged. A bound chosen
after inspecting a sample cannot make that sample untouched holdout evidence.

The runners preserve source hashes, configuration and executable identity,
receipt-time controls, negative results and incomplete executions. Their
execution/production qualification remains false until separate source
quality and account-cost evidence are reviewed. Successful simulation does
not establish actual venue fills or incentive credit.

### Paired experiments

For ETH quant, use the same 12 bps Core Standard baseline and tape in both
arms. The experimental arm withdraws an inventory-increasing quote when
the native Hyperliquid best-level size imbalance reaches 0.60 against
that side and the source timestamp is no older than 250 ms. It preserves
quotes that may close inventory and uses the existing cancel/fill lifecycle.
The runner reports each ledger's net dollars per hour after modeled
overhead, quote withdrawals, late cancels, turnover and one-second Maker
markout. Compare complete ledgers on the same market interval.

```sh
python3 scripts/replay_quant_participation.py target/release/maker_replay \
  /path/to/ten-hour-scenario-output /path/to/new-eth-quant-study --capital 500
```

The paired report includes `fresh_reference_fraction`: fresh filter decisions
divided by all filter decisions. Missing coverage or a zero decision count
remains unknown. A complete cash ledger does not mean its signal received
sufficient fresh observations. The original filter keeps its **250 ms** age
limit. `--max-reference-age-ms 1000` creates a separately labeled historical
reference-age sensitivity with the same 0.60 imbalance threshold, fees,
latency, execution lifecycle and capital. Report both outcomes and signal
availability; do not select a profitable age retrospectively and call it a
holdout result.

For Extended × Lighter basis, compile a tape with both venue indices, an
independent Coinbase reference and a declared paper notional allowance.
Use `--route lighter-extended` with the archived compiled tape. The route
uses 0/2.5 bps reference taker fees, a 550 ms one-way stress scenario and
the same terminal drain as the native Hyperliquid baseline. A fee override
is recorded as a sensitivity case. Public fees are account assumptions.

```sh
python3 scripts/replay_ten_hour_arbitrage.py target/release/cross_venue \
  /path/to/ETH-lighter-extended.ndjson.zst /path/to/new-eth-basis-study \
  --route lighter-extended --public-archive /path/to/venue-archive --start-ms UTC_MS
```

Extended × Lighter turnover uses the same archived market data with the
`--turnover-venue extended --turnover-fees-bps 0 2.5` controls. Each opening
posture and cost allowance gets an independent ledger. Compare net cost
after both legs exit alongside target-venue volume and unhedged duration.
Points and unconfirmed rebates carry zero value. Keep Robinhood Chain data
under its own market domain and fee assumptions.
[Extended fee schedule](https://docs.extended.exchange/extended-resources/trading/trading-fees-and-rebates)

### Historical fee attribution

Oasis job `basis-fee-attribution-20261113T173628Z` replayed the original
ten-hour BTC/ETH interval with unchanged depth, signal and 550 ms delay.
Each arm consumed its tape and completed a 60-second terminal drain.

| Asset | Best modeled edge, reference fees | Best modeled edge, zero explicit fees | Entries in either arm |
| --- | ---: | ---: | ---: |
| BTC | −14.71 bps | −5.71 bps | 0 |
| ETH | −15.32 bps | −6.30 bps | 0 |

Reducing explicit fees recovers about 9 bps of modeled edge. The current
signal still falls short of its full execution and exit hurdle on this
historical window. Both arms have zero traded volume. The result isolates
the fee effect for this policy and window. The new ETH participation and
Extended route still require a complete real-market evaluation.

### Fresh Oasis runs

The 75-second Extended index smoke archived 51 BTC and 50 ETH index
messages. The compiler found 677 BTC and 671 ETH frames with fresh books,
venue indices and the independent reference. The smoke establishes source
compatibility and supplies no strategy PnL.

| Public source or run | Oasis research directory |
| --- | --- |
| Shared Core/Robinhood/Hyperliquid/Extended/Coinbase capture | `2026-11-13-continuous10h-153508` |
| Hyperliquid trades, books and hourly funding history | `hyperliquid-public-10h-20261113T173441Z` |
| Extended and Core Lighter with same-clock index evidence | `extended-lighter-index-10h-20261113T174747Z` |
| Frozen independent replay job | `independent-profit-validation-20261113T174809Z` |

The one-shot replay job waits for each sealed source. Quant and turnover
use a common ten-hour window on the shared capture. Extended basis uses a
later ten-hour window on the indexed capture. The job records the separate
decision boundaries and retains individual reports. Do not compare these
different windows as a paired experiment. The jobs were pending at this
writing. No profit or turnover qualification is inferred from capture status.

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
