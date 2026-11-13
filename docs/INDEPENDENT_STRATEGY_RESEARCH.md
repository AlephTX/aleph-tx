# Independent strategy research

The three objectives share data and execution primitives but have separate
profiles, ledgers, risk budgets and acceptance criteria. A profitable run
in one objective cannot subsidize a loss in another.

New validation uses [continuous ten-hour units](TEN_HOUR_RESEARCH.md).
The short-window experiments below retain their original diagnostic scope;
their total duration does not establish uninterrupted execution.

| Objective | Decision | Primary measurements |
| --- | --- | --- |
| Quant profit | Select economically valuable trading events | Full net profit/hour, net bps, drawdown, inventory, capacity |
| Hedge arbitrage | Admit matched products when executable cash flows cover full costs | Completed legs, net cash flows, funding separately, capital occupied, residual risk |
| Incentive turnover | Maximize real target-venue fills within an explicit cost budget | Target volume/hour, full net cost per target volume, exit quality, program eligibility |

Current perpetual basis reversion is statistical relative value, not a
locked terminal cash flow. Funding carry remains a separate, uncertain
cash-flow component. Neither is relabeled as risk-free arbitrage.

## Turnover postures

The user selects opening direction on the **target maker venue**. These are
three turnover variants, not three alpha models or different hedge ratios.

| Value | Opening decision | Closing and hedging |
| --- | --- | --- |
| `neutral` | Compare buy-first and sell-first candidates | Full hedge of confirmed fills; close both legs |
| `long_first` | Buy first on the target venue | Sell hedge; later sell target inventory and release hedge |
| `short_first` | Sell first on the target venue | Buy hedge; later buy target inventory and release hedge |

The replay holds one maker quote at a time. `neutral` admits both opening
directions; it does not imply simultaneous bid and ask orders. A directional
posture never blocks the opposite-side reduce order needed to close a cycle.
There is no intentional residual directional position. Partial fills, pending
cancels and hedge failures remain subject to the existing exposure guard.

Add this object to a `maker-first-replay` turnover configuration:

```json
"turnover_posture": {
  "initial": "neutral",
  "max_control_ttl_ms": 300000
}
```

This controller requires `pair.mode.kind = "turnover"` and `hedged_exit`;
it cannot be combined with `entry_side` or `passive_offset`. The first
experiment therefore compares immediate hedging and subsequent Maker
closing, with the same risk limits across all directions.

### External control contract

An external decision service may append a receipt-timed event to the replay
stream. The project contains no LLM signal generator or publicly exposed
control server. A future live adapter needs its own authentication,
durability and restart review before it can submit orders.

```json
{
  "kind": "turnover_control",
  "at_ms": 1800000000100,
  "control": {
    "revision": 1,
    "issued_at_ms": 1800000000000,
    "expires_at_ms": 1800000060000,
    "posture": "short_first"
  }
}
```

`at_ms` is the local availability time, not a backdated model decision.
Revisions must increase; future-issued, expired and overlong instructions
are rejected. Unknown fields are rejected, so a posture command cannot
raise size, leverage, loss, holding-time or exposure limits. Expiry returns
the posture to `neutral` on the next event.

Changing posture requests cancellation and drains the active cycle. The old
order retains its originating posture until terminal evidence and fill
delivery are resolved. A late first fill therefore starts an old-posture
cycle that must drain; it cannot become a fresh opposite-direction entry.
Only a flat, settled lifecycle can start the new posture. Reports preserve
started/completed cohorts and realized cash PnL by originating posture in
`neutral / long_first / short_first` order. Incomplete cohorts are retained.

## Research design and accounting

```sh
cargo build --locked --release --features research-replay \
  --bin cross_venue --bin maker_replay
python3 scripts/replay_turnover_postures.py \
  target/release/cross_venue /path/to/sealed-capture.ndjson.zst \
  /path/to/metadata-000 /path/to/new-study --symbol BTC
```

The fixed initial grid uses three postures, $50/$100 target order sizes and
0/4 bps projected-cost allowances. Each arm receives its own turnover,
fee and loss budget. The 4 bps arm measures paid turnover; it is not a
zero-cost or profitable configuration by construction. No winner is fitted
inside the runner. A $500/$1,000 capital scenario must still account for
cash split across venues, minimum order sizes and reserve capital; changing
the denominator alone does not create an execution advantage.

The baseline is Extended Maker with Lighter Core Standard hedging. Public
reference fees are Extended 0/2.5 bps and Core Standard 0/0 bps, with the
Core 300 ms action delay added to the 250 ms transport assumption. These
are research assumptions pending account-level fee and latency calibration.
All postures use a 2.5 s unhedged timeout, 60 s hedged holding limit, 15 s
drain, protected IOC execution and 0.25 bps overhead on both venues' gross
turnover. Windows avoid hourly funding boundaries; zero funding in these
windows is not a finding that longer holding periods have zero funding cost.

For each arm:

`target cost bps = -full net PnL / all target-venue turnover × 10000`.

Full PnL includes both legs, fees, exits and modeled overhead. Target volume
includes forced exits; hedge volume cannot enlarge its denominator. Points
and unconfirmed rebates have zero value. Venue-confirmed eligible volume
remains unknown without a program statement. Any incomplete window makes
the corresponding aggregate net cost unknown, rather than silently dropping
the window or marking residual inventory as flat.

Historical reused periods are regression evidence. Newly sealed periods are
reported separately; posture selection made after observing their outcome
is an oracle comparison, not an external signal with proven profitability.
Different arms on the same tape are paired experiments, not independent
market observations. Each shard restarts flat; boundary drains and warm-up
costs differ from a continuously running account.

## Industry mechanisms to test

Cross-exchange market making provides the first execution baseline: make
on the target venue and hedge actual fills on the other venue. Hummingbot's
Cross-Exchange Mining adds realized-profit feedback to seek more turnover
near breakeven. Its published implementation targets spot CLOB exchanges;
perpetual funding, independent margin and final exit still need explicit
modeling here.
[XEMM](https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/) ·
[Cross-Exchange Mining](https://hummingbot.org/strategies/v1-strategies/cross-exchange-mining/)

Single-venue spread capture and inventory-skewed quoting remain useful
comparators, but a buy-then-sell sequence is not itself an edge. Test queue
position, fill-conditioned markout and bounded inventory separately from
the chosen opening direction. A short passive-close window before hedging
has a different naked-exposure budget and must remain a separate execution
experiment. Increasing that window to hide exit losses is not an improvement.

## Lighter Robinhood Chain

`lighter_robinhood` has separate liquidity and source identity from Core.
Core historical profitability and account mechanics do not transfer to it.
The official announcement describes an 11 million LIT rewards pool and
different Wallet/web point multipliers. API eligibility, current campaign
terms and credited volume still need independent verification; the public
announcement does not establish an API multiplier. LIT already exists, so
this is an incentive-program case rather than an unissued-token assumption.
[Official domain documentation](https://docs.robinhood.com/chain/lighter-domains/) ·
[Official rewards announcement](https://robinhood.com/newsroom/robinhood-accelerates-global-expansion-robinhood-chain-mainnet-stock-tokens-agentic-trading/)

The collector and compiler are read-only. Live signing-domain, collateral,
fee-tier and account integration are not promoted by these changes. Raw
public archives and reproducibility hashes stay in the Oasis research
directory; authenticated records and credentials never enter this report
or Git.

## Verified run — 2026-11-13

Oasis run `independent-657d408146c7` used an isolated snapshot based on
`2692e2d` plus the posture/domain changes. All 124 Rust/Python build and
research dependency hashes matched the submitted working-tree snapshot
before compilation. The run identity hash is
`657d408146c7e079d43a3527fd1f5aca0091225c219d47f78f8744951592e0f1`.
Two additional synthetic regression tests and decimal-report formatting
were added afterwards; execution decisions were unchanged.

| Independent experiment | Data and execution scope | Result |
| --- | --- | --- |
| Quant | BTC and ETH, approximately 20 h each; 22 windows × 2 sizes × 2 assets = 88 runs | All windows completed; all four aggregate baselines lost money |
| Basis arbitrage | Approximately 6.50 h per asset; 377,527 total BTC/ETH frames; Core Standard / native Hyperliquid | No entries; best modeled net edge −13.90 / −16.11 bps |
| Extended turnover | 15 sealed public shards per asset; 336 posture/size/cost replay windows | A 90 s initial shard could not fit a full cycle; later feed gaps prevent some whole-campaign cost claims |
| Arcus turnover | 4.83 h per asset; 60 posture/size replay windows | ETH completed all arms; some BTC arms retained minimum-size residues |
| Robinhood capture | Separate 5 min public capture of Core/RH BTC and ETH | Domain-specific books, prints and clock refresh validated; no profitability inference |

Repeated configurations on the same market interval do not multiply its
independent sample size. The quant and Arcus periods were already studied
historical periods, not untouched out-of-sample evidence. The Extended
cohort is from the ongoing capture, with no fitting inside this grid.

### Quant: capital and size do not fix a negative edge

The fixed Standard 12 bps baseline includes current zero public fees and
the 300 ms venue action delay. The table deducts an additional 0.25 bps
model overhead from total maker plus taker turnover.

| Asset | Order size / simulated equity | Gross turnover | Net after overhead | Net bps of gross turnover |
| --- | --- | ---: | ---: | ---: |
| BTC | $50 / $500 | $4,866.07 | −$0.7361 | −1.513 |
| BTC | $100 / $1,000 | $8,657.08 | −$1.3301 | −1.536 |
| ETH | $50 / $500 | $7,669.78 | −$0.7972 | −1.039 |
| ETH | $100 / $1,000 | $10,667.47 | −$2.0552 | −1.927 |

Some individual windows were profitable, but their longer combined result
was negative. These are checks of a fixed baseline, not a proof that every
quant strategy or every 50–200 ms strategy is unprofitable. Larger capital
alone did not improve this policy's net bps; this run does not justify scaling it.

### Turnover: opening direction changes outcomes, not the fee structure

The following Arcus ETH comparison has five complete windows and 4.83 h
per arm, $50 target size and an 8 bps experimental cost allowance. Every
arm includes both legs, final exits and model overhead. Points are zero.

| Posture | Target turnover | Net cash after overhead | Cost per target volume | Maker / forced IOC completed cycles |
| --- | ---: | ---: | ---: | ---: |
| `neutral` | $1,721.40 | −$0.9263 | 5.381 bps | 2 / 23 |
| `long_first` | $1,097.45 | −$0.7790 | 7.098 bps | 1 / 13 |
| `short_first` | $1,732.34 | −$0.9255 | 5.342 bps | 1 / 20 |

At $100 target size, the same three ETH costs were 5.363, 6.130 and
5.282 bps. For BTC, short-first completed all windows at 4.211 / 4.332 bps
for $50 / $100; the other BTC arms left residues in one or two windows,
so their full aggregate net costs remain unknown. A residue is not a
zero-cost close. Short-first's lower historical cost is not evidence that
it should always be selected or that an external predictor can select it
profitably in advance.

Extended had 227,307 synchronized pair frames and 13,220 usable target
trade events across the compiled asset/shard studies. After the capture
repair boundary, 12 windows per asset covered 2.98 h. One window per asset
had a resting order during a trade-feed gap. Full-cohort claims remain
qualified by those gaps. For a **diagnostic paired comparison**, retaining
only the 11 windows where every posture/size/cost arm had complete evidence
gives 2.73 h per asset:

| Extended posture, $50 | BTC target volume / cost | ETH target volume / cost |
| --- | ---: | ---: |
| `neutral` | $30.49 / 3.052 bps | $196.39 / 6.079 bps |
| `long_first` | $0 / undefined | $97.81 / 4.051 bps |
| `short_first` | $129.48 / 4.687 bps | $97.61 / 2.595 bps |

These 4 bps admission-budget arms realized as much as 6.079 bps cost at
$50: admission estimates are not guarantees against asynchronous execution
loss. Each cell contains at most two completed cycles, so it is unsuitable
for selecting a production posture. All zero-cost admission arms produced
zero target turnover. The dominant rejection was `turnover_cost`, rather
than an instruction to trade a preferred side being ignored.

The immediate economic bottleneck remains completion quality: most tested
cycles required forced IOC exits. Further research should compare executable
passive-close opportunities and queue retention under the same complete-cost
ledger, then evaluate externally selected entry directions on new periods.
Increasing the allowable loss or retrospectively picking the best direction
cannot establish a profitable turnover policy.

### Robinhood evidence and validation

Capture `robinhood-domain-20261113T145647Z` archived 41.7 MB of public
records into a 5.36 MB compressed file. Each asset produced 1,163 compiler
frames, 1,160 synchronized frames and zero nonce gaps or clock rejections.
Both domains renewed clock evidence at four minutes. The compiler profiles
used zero capacity, disabled paper entries and an explicit synthetic common
unit solely to exercise ingestion; settlement equivalence is unverified.
The compressed public archive SHA-256 is
`72e8ca25244113d713a88c3552b3cded80da1f7d484fa4a0988599af48ed9f9e`.

Validation: `make fmt-check check test build`, research-feature Clippy,
`cargo test --locked --lib --bins --features research-replay`, and the
turnover Python accounting tests passed. New synthetic cases verify all
three full-hedge cycles, compatibility with legacy entry-side execution,
expiry, replayed controls, future controls, risk-field rejection and late
fills after a posture change. Existing Lighter and Hyperliquid regressions
remain enabled. These checks did not start a live trading session.
