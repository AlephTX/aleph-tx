# Strategy research under finite latency and $500–$1,000 capital

Research decision, 2026-11-13. Public data only. This note narrows the next
experiments; it does not promote a strategy to live execution.

## What the negative results establish

The archived policies have not established repeatable profit after complete
execution and exit costs. They do not prove that every strategy at the same
capital or latency has negative expectation. Several comparisons need a
consistent economic interpretation:

- The earlier 23 bps stress cost is a configuration assumption, not a universal
  cost of trading a perp DEX. Current venue, product, account and execution
  roles determine the applicable fees.
- Zero-valued points remove speculative subsidy. They do not rule out positive
  spread capture or relative-value PnL. A positive reward valuation would not
  turn negative trading PnL into demonstrated cash profitability.
- The reported Extended BTC 2.82 bps is a complete-cycle loss divided by target
  opening **and closing** turnover. It is not one leg's fee or a universal
  round-trip hurdle. At that denominator, 1 bp costs $100 per $1 million.
- More capital can reduce minimum-order problems and fund paired inventory.
  It cannot change negative unit economics by multiplication. The existing
  production-quote replay already defaults to $1,000 initial equity; its $50
  order target is a different parameter.
- Negative one-second markout and fills during pending cancellation diagnose
  selection risk. Neither statistic alone determines complete-cycle PnL.
  Price effects already in fill cash flows must not be deducted twice.

See the [profitability postmortem](../PERP_RELATIVE_VALUE.md#profitability-postmortem-2026-11-13)
for archived execution results and their limitations.

## What the current strategies actually test

The production-quote replay runs the inventory strategy. Its cross-market reserve
widens the inventory-increasing side and competes with other protections.
It does not implement a learned decision to participate based on expected
complete-cycle PnL. The underlying strategy already has external fair value,
momentum and order-flow components; describing it as having no signals would
also be inaccurate.

In `maker_first.rs`, `opening_edge` values entry, reverse maker execution and
hedge exit using the current books. The turnover path additionally budgets
the adverse current-book fallback. It has no forecast of future cross-venue
basis convergence. This is an intentional cost control, not a forecast of
all opportunities. `relative_value.rs` separately implements causal basis
estimation and reversion credit. A maker-first zero-entry result cannot be
used as a test of that different strategy.

For equal base quantity, a persistent level difference can cancel between
entry and exit. If `d = price_short_venue - price_long_venue`, paired PnL is
approximately `q * (d_entry - d_exit) + funding - costs`. A large `d_entry`
alone is insufficient. For pure common-price alpha, simultaneously holding
equal long and short positions also offsets the common directional gain.
Neutral prediction must target relative returns, execution quality or carry.

## Additional public-data screen on Oasis

The current compiler processed the corrected collector's eight sealed shards
003–010, covering **2026-11-13 11:45:11–13:45:18 UTC**. BTC and ETH each had
about 64,000 eligible paired frames under the existing age/skew limits.
The executable compiler is from the `2692e2d` research tree plus the local
closing-quote attribution change. The screen is additional Python analysis
of its output, **not a replay of order decisions or realized profit**.

The first screen used 100 ms sampling, maximum book age 2,200 ms, maximum
inter-venue skew 750 ms, 20% displayed-depth participation, matched base
quantity rounded down to common lots, and entry minimums. Fee assumptions
were Lighter Standard regular taker 0 bps and Extended crypto taker 2.5 bps,
without additional account-specific charges or rebates.

| Diagnostic | BTC | ETH |
| --- | ---: | ---: |
| Median `(Extended mid − Lighter mid) / Lighter mid` | −7.285 bps | −8.437 bps |
| Median opening-only cash advantage after entry fees, $50 per leg | 4.450 bps | 5.396 bps |
| Same diagnostic, $200 per leg | 4.431 bps | 5.363 bps |
| 99th percentile absolute basis change over 5 seconds | 3.178 bps | 3.475 bps |
| 99th percentile absolute basis change over 60 seconds | 3.753 bps | 3.729 bps |

The opening diagnostic includes no exit, funding, arrival execution or margin
cash flow. It is not PnL. Its persistent sign makes the exit basis especially
important. Absolute basis changes are hindsight magnitudes, not predictable
returns. The small $50-to-$200 displayed-depth deterioration identifies no
immediate depth barrier at those sizes in this sample; it does not establish
capacity, fill probability or a capital allocation.

A second screen required book age at most **250 ms** and skew at most
**100 ms**. It calculated residuals against a causal EWMA with 180-second
half-life, 60-second warmup and reset after a one-second valid-data gap or
shard boundary. A trigger occurs above 3 bps absolute residual and rearms
below 1 bp. These fixed screening choices were not fitted to PnL.

| Strict freshness diagnostic | BTC | ETH |
| --- | ---: | ---: |
| Eligible paired frames | 49,850 | 48,903 |
| Warmed frames | 23,342 | 19,461 |
| 99th percentile absolute residual | 2.865 bps | 2.863 bps |
| Hysteresis triggers | 21 | 22 |
| Still beyond 3 bps in the original direction at 500 ms | 3 / 19 observable | 5 / 22 observable |
| Same test at 1 second | 3 / 20 observable | 5 / 22 observable |

The denominator excludes unavailable future observations; those are unknown,
not successful fills. The observation tolerance is 150 ms. Triggers can remain
serially correlated and are not independent statistical trials. Neither a
single later observation nor a positive opening cash difference establishes
continuous executability. At 100 ms sampling, this screen cannot validate a
50 ms strategy. Extreme observations require packet-level review before use.

This sample supports looking for conditional residual opportunities, while
showing that many naive 3 bps triggers disappear rapidly. It does not support
repeatedly monetizing the persistent 7–8 bps level difference.

At capture start, Lighter's reported daily quote-volume ranking was BTC
($1.582 billion), ETH ($802 million), NVDA ($101 million), HYPE ($89 million),
SOL ($61 million), ZEC ($60 million) and XAU ($52 million). BTC/ETH are thus
liquid controls on that venue. These are venue-reported volumes at one time,
not independently verified turnover or a ranking across every venue.

A separate public market-list query at **14:00:05 UTC** found 42 active
same-symbol perpetual CLOB candidates shared by Extended and Lighter.
Ranking by the smaller of the two reported daily quote volumes produced:

| Candidate | Smaller venue's reported daily volume |
| --- | ---: |
| BTC | $100.39 million |
| ETH | $41.50 million |
| HYPE | $19.96 million |
| XAU | $13.50 million |
| SOL | $7.18 million |
| XAG | $5.62 million |

HYPE and SOL are plausible next crypto data candidates. XAU merits a separate
RWA specification review, including oracle units and market hours, and the
discounted RWA fee schedule. This query verifies market presence, not economic
equivalence, order-size eligibility, volume authenticity or profitability.
The responses and aggregate are archived as `*-universe-public.json` and
`universe.json`. No additional collection service was launched by this screen.

The scripts, parameters, compiled frames and reports are archived under the
Oasis research job's `latency-opportunity-screen/` directory, outside Git.

| Artifact | SHA-256 |
| --- | --- |
| Depth and duration screen, `report.json` | `c7115aa4505c5e13b0c4a40aa7be34bf088452ef3b8e5f4ee9f3444b8ed39ba3` |
| Causal residual screen, `freshness-v2.json` | `e2eb54b420409a72125998b4f4c2a11a370cde467015b5dff099cbf9d591c15c` |

## Industry evidence and its limits

- [Albers et al., cross-market Bitcoin price formation](https://arxiv.org/abs/2108.09750)
  studied 500 ms forecasts and deployed a maker experiment. This supports
  researching cross-market predictive features at subsecond horizons. The
  historical live result depended on its fee regime and does not establish
  profitability on today's DEXs or at our end-to-end latency.
- [Bieganowski and Ślepaczuk, 2026](https://arxiv.org/html/2602.00776v1)
  study a three-second target with order-book and trade features. Their taker
  backtest explicitly omits execution latency. Use its feature hypotheses,
  not its returns as evidence of executable profit here.
- [Headlands' trading-system description](https://blog.headlandstech.com/2017/08/03/quantitative-trading-summary/)
  connects market pressure, trade impact and related-market signals with
  trading decisions and production reconciliation.
- [Fundamentals of Perpetual Futures](https://arxiv.org/abs/2212.06888)
  distinguishes perpetual pricing and trading-cost bounds from dated futures.
  A perpetual has no guaranteed maturity convergence.
- [Hummingbot Cross-Exchange Mining](https://hummingbot.org/strategies/v1-strategies/cross-exchange-mining/)
  provides a cost-feedback reference. Its documented implementation is spot
  CLOB, so derivative funding, margin and closing inventory need explicit
  treatment here.

[Lighter's official schedule](https://docs.lighter.xyz/trading/trading-fees)
lists Standard regular orders at 0/0 fees with an additional 300 ms on taker
and cancellation/modification actions. Transport of 50–200 ms therefore
does not mean a 50–200 ms completed hedge. Model receipt, dispatch, venue
delay, matching and fill notification separately. Faster Premium adds costs
and needs its own economic comparison.

[Extended's current schedule](https://docs.extended.exchange/extended-resources/trading/trading-fees-and-rebates)
lists 0/2.5 bps maker/taker for ordinary markets and discounted RWA taker
fees. [Hyperliquid's schedule](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees)
has product/deployer and growth-mode adjustments. A fixed 9 bps HIP-3 fee
must not be applied to every product. Archive actual fee applicability,
request quotas and any builder charges before execution comparisons.

## Research portfolio and capital contract

Treat $500–$1,000 as **total capital across venues and reserve**, not each
order's notional. An illustrative paper allocation is $200 + $200 + $100
reserve at $500, or $400 + $400 + $200 at $1,000. Begin with $25/$50/$100
matched-leg notionals; $200 is a capacity experiment. These allocations
must pass each venue's margin and stressed-loss calculation. Reserve outside
a venue cannot instantly prevent that venue's liquidation.

| Priority | Hypothesis | Prediction / holding horizon to study | Required evidence |
| --- | --- | --- | --- |
| 1 | Event-conditioned maker with immediate confirmed-size hedge | 0.5–5 s prediction; separate paired exit horizon | Conditional fill PnL exceeds all hedge, exit and failure costs; model changes actual participation |
| 1 | Same-asset cross-venue residual reversion | 5–180 s, then minutes where supported | Causal residual predicts executable convergence after fees; stable result on unseen sessions |
| 2 | Selective liquidity provision after temporary flow shocks | Seconds to minutes | Replenishment and multi-venue evidence distinguish temporary pressure from a continuing repricing |
| 2 | Funding/basis carry with infrequent switching | 1/3/7/14 days | Actual settlement cash flows, entry/exit basis and per-venue capital stress; full switching costs |

The first route can serve profitable quantitative trading. Only its proven
nonpositive-cost subset should serve the turnover objective. Count genuine
external fills; keep points and unconfirmed rebates at zero in the cash
ledger. Do not use profits from another strategy to subsidize that ledger.

For neutral strategies, predict each venue's return relative to the common
market move. Features should distinguish local depth imbalance, flow changes,
aggressive prints, replenishment, reference lead/lag, basis residual, data age
and volatility. Bars can describe regime and longer pressure; one-minute bars
cannot calibrate a 500 ms order race. Start with bucketed statistics or a
regularized linear model before adding model complexity.

Retain BTC/ETH as controls. Rank additional *matched* products by executable
depth, real contra-flow, minimum order/exit sizes, full costs and opportunity
duration. A wide spread or high reported volume alone does not qualify a
market. RWA routes additionally require compatible economic terms, trading
sessions, oracle behavior, dividends and split treatment.

## Next executable experiment

1. Preserve the existing controls. Review late-print/cancel handling and
   reduce-only minima in the quant simulator before claiming improved
   executable returns. Include actual request-budget scheduling.
2. Label opportunities with information available at receipt time. Measure
   forward relative return and complete-cycle value at actual order arrival,
   not only future mid-price. Train on earlier sessions and purge overlapping
   labels at split boundaries. Deduplicate opportunity episodes.
3. Add an independently switchable entry-participation policy to the shared
   execution lifecycle. Compare no filter, event filter, then conditional
   pricing; change one decision component at a time. Keep naked-exposure
   limits unchanged and paired-inventory limits explicit.
4. Replay 50/100/200 ms transport as research scenarios, plus actual venue
   delays and a slower stress case. Use full partial-fill, queue, cancellation,
   fallback and end-of-window liquidation behavior. Retain failures and
   unknown inventory in the denominator of reported runs.
5. Report complete net dollars/hour, target-turnover bps, capital employed,
   net/gross exposure time and capacity. Use held-out days and block-based
   uncertainty; repeated snapshots and variants are not independent trials.

The next development milestone is a measured change in **which events are
traded and why their complete cash flows are favorable**. Faster networking,
larger capital and wider parameter searches are separate hypotheses.
