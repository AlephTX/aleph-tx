# Perpetual relative value and paper execution

The `relative_value` module simulates a matched long and short on the same
linear perpetual exposure. The planner tests both directions against
executable depth. The paper runner applies order latency, partial fills,
hedge recovery and an independent cash and position ledger. Its order
intents stay inside the simulation.

| Strategy | Entry signal | Exits |
| --- | --- | --- |
| Funding carry | Conservative net funding over a scheduled hold covers both round trips, basis risk and the cost of capital. Entry assumes zero basis convergence. | Time limit, adverse basis, lost reference or insufficient or negative remaining funding forecast. |
| Basis reversion | A gap between synchronized mids exceeds the entry band around its trailing mean. The budget credits a configured fraction of reversion and charges possible funding debits. | Reversion to the exit band, adverse basis, lost reference, session change or time limit. |

Perpetuals have no expiry that forces prices together. The funding and basis
models are hypotheses to measure on held-out data. The time-weighted mean and
variance use only earlier paired observations when scoring an entry. The
estimator requires fresh source timestamps on both legs, resets after data
gaps or a new equity session, and limits the effect of extreme observations.
The open position keeps its entry baseline for exit decisions.

Set `pair.basis.min_exit_profit_bps` to a decimal string to require
cycle profit at both protective close limits before a discretionary
reversion exit. The bound includes realized entry fills and fees, booked
funding and both projected closing fees. Profit is normalized by one
leg's reference notional. The runner checks again on later frames and
continues to honor risk and time exits. The report counts executable
close proposals that fell below the floor.

## Contracts, markets and sizing

- Configure the venue's native market, canonical underlying identity and
  reviewed payoff convention on each leg. For an equity, the identity includes
  share class and the payoff convention includes splits, dividends and
  adjustments. A ticker alone is insufficient. Each contract has a multiplier
  and a native lot. The hedge balances normalized shares or coins using
  their common lot. Quotes and cashflows must already be in the same settlement unit.
  Cross-collateral FX and inverse or quanto payoff models need explicit adapters.
- Supply a live external reference and each venue's oracle with source times.
  The strategy checks book and oracle deviation against the reference. Equity
  frames also need the applicable regular session in UTC, halt status and the
  time through which corporate actions and terms have been checked. The
  planned hold and unwind must fit before the earliest cutoff with a buffer.
  Holidays, early closes and daylight saving time belong in the data source.
- The entry solver prices up to 20 levels per side. It discounts displayed size
  by participation, compares common-lot sizes at depth changes, and models the
  full size at each marginal protective limit. It charges both directions on
  both venues, reserves basis volatility and constrains worst single-leg
  notional against explicit approved capacities. The two strategy legs cannot
  share a matching book. Shared infrastructure requires explicit opt-in.
- Funding input is a timestamped forecast for each payment on a verified UTC
  schedule. Rates are fractions per payment and positive rates charge longs.
  Credit requires a payment after the entry window and before the planned
  exit. Possible debits count until the end of the unwind window. Feed realized
  rates separately. Unknown schedules, incomplete forecasts and read-only
  indicative quotes cannot support an entry.

Set `pair.optimize_funding_horizon` to `true` for funding carry to make
`planner.funding_horizon_ms` the maximum hold. The planner evaluates closes
after scheduled payments, with any configured funding guard and the full
unwind window. It ranks the feasible sizes and holds by conservative net
edge and favors the shorter hold at equal edge. Capital is charged for the
candidate's own hold and unwind. A later missing forecast only rules out
holds whose unwind window reaches that payment. Existing configurations use
the fixed horizon.

Use the venue's current price grid for the applicable price band. A fixture's
tick, fee, contract multiplier, settlement mapping or event schedule does not
establish live venue terms. This path supports constant funding cadence within
the traded session. A venue with irregular payment times needs a calendar
adapter before onboarding.

## Replay

A pair uses `"mode": {"kind": "pair_hedge"}` by default. For a session
that measures trading activity within a budget, set `pair.mode` to

```json
{"kind":"turnover","max_turnover_quote":"10000","max_fees_quote":"10","max_realized_loss_quote":"2"}
```

The caps use the common quote unit. The solver deducts filled turnover
and simulated fees from their respective session budgets. It then
estimates both entry and both exit IOCs at their protective limits and
reduces the candidate to the largest affordable common lot within each
depth band. The smaller plan must still clear both venues' minimums,
exposure limits and the positive net-edge threshold. If the remaining
budget cannot support a legal, profitable size, the planner reports
`turnover_budget`. Risk exits and hedge repairs proceed even if their
execution takes realized costs above the entry budget. The optional
`max_realized_loss_quote` stops new entries when flat cash PnL reaches
the loss cap after fees and booked funding. The report records
`turnover_loss_limit`. Prospective rewards carry zero value in both modes.

### Incentive turnover

Add `incentive` to a turnover profile with `strategy: "basis_reversion"`.
The solver compares both hedge directions on protective limits, with
zero convergence credit and a reserve for any funding debits. It ranks
by lowest modeled cash cost per dollar of target venue fills. It reserves
the modeled loss for the full cycle and starts the close as soon as the
legs are matched and usable books are available. For example, this
synthetic policy targets the second leg in the stated UTC window.

```json
{
  "kind": "turnover",
  "max_turnover_quote": "2000",
  "max_fees_quote": "1",
  "max_realized_loss_quote": "0.50",
  "incentive": {
    "eligible_leg": 1,
    "eligible_from_ms": 1800000060000,
    "eligible_until_ms": 1800000062800,
    "max_cost_bps": "2"
  }
}
```

Record the source and coverage period for the token issuance status and
program terms before assigning an eligibility window. The planner
requires the modeled cycle to fit inside the window. Lighter and native
Hyperliquid are excluded as targets. Either can provide the hedge.
`incentive_turnover.filled_turnover_quote` counts fills on the target
leg within the window for completed paired cycles. Once flat,
`realized_cost_bps` divides the whole
cycle ledger cost by that turnover. If losses exceed the unit-cost
limit, the runner blocks new entries. Positions already opened continue
through the hedge and close path. Venue program eligibility and rewards
require separate settlement evidence.

For carry, `funding_no_credit` means no direction has a positive funding
lower bound for a reachable hold. `net_edge` means the priced candidate
failed the net threshold. `best_net_edge_bps` records the best priced edge
before turnover caps, so it can be positive in a run with zero entries.

`pair.funding_carry_policy` enables an optional forecast credit haircut and
capital-time hurdle. `credit_half_life_ms` discounts positive lower-bound
cash flows from forecast observation to settlement. Potential debits retain
their full modeled cost. `min_net_return_bps_per_day` is a decimal-string
entry floor. Optimized horizons rank by modeled net return per day on the
sum of both legs' risk notionals, including the unwind time. This denominator
is a gross notional proxy. Portfolio selection still needs separately
verified collateral, venue margin and switching costs. The settlement ledger
books actual witnessed cash flows independently of forecast haircuts.
[Perpetual funding and transaction costs](https://arxiv.org/abs/2212.06888) ·
[Matched terminal settlement](CROSS_VENUE.md#dated-settlement-and-combination-orders)

The [public recording workflow](PERP_RECORDING.md) creates this input format
from venue data and reports timestamp and reference coverage.

```sh
make build
target/release/cross_venue perp-replay examples/crypto_funding_paper.json
target/release/cross_venue perp-replay examples/equity_basis_paper.json
```

Both fixtures are synthetic. The first holds a Lighter/Hyperliquid pair through
a funding event and charges four taker fills. The second uses a fictional
matched equity on Lighter and Entropy. It selects half a share because the
second ask level consumes the modeled edge, then closes when the basis reverts.
Both finish with zero position. They verify the accounting and execution path.

Larger recordings can use `perp-replay recording.ndjson`. The first line is the
same configuration object without `frames`. Each following line contains
one frame. The reader streams in strictly increasing `now_ms` order. Capture source
timestamps separately from arrival time, and include every realized payment on
held legs. An optional `trade_from_ms` lets earlier frames train the estimator
without placing orders. Fix strategy parameters on an earlier period and
evaluate them on a later chronological holdout.

## Read the execution report

An IOC can consume only a newer book generated after the configured modeled
latency. The simulator walks displayed prices up to the order limit and
passes confirmed fills to the hedge state machine. Funding at a frame time
is booked before simulated fills at that time.

Set `pair.recover_single_leg` to enable paper-only recovery of a
confirmed single-leg residual after a failed execution. Once all
original orders are terminal, it can submit a protected IOC on the
venue holding the residual, within the original unwind deadline.
Each fill must reduce the confirmed position and uses the same depth,
lot, fee and arrival rules as the pair. The runner halts new entries
after a failed cycle. The report preserves recovery PnL and any
remaining exposure, and charges rescue fees without crediting the
aborted cycle's volume to the incentive denominator.

| Question | Report fields |
| --- | --- |
| Was the strategy ready? | Training and holdout frames, basis samples and `entry_diagnostics.blocked_by_gate` |
| Did the legs execute? | Entry signals and submissions, opening and closing leg execution, partial and expired IOCs |
| What did execution cost? | Filled turnover, fees, per-market slippage and separately booked funding |
| How much risk remained? | Maximum unhedged size and duration, observed drawdown, halt reason and final positions |
| What value can be measured? | Simulated realized PnL, upper bounds for incomplete funding evidence, and the final liquidation estimate when the book supports it |

Each `market_costs` row names the venue and contract and totals simulated
filled turnover and fees for both directions. `execution_cost_bps` divides
fees plus signed slippage from the submit-time touch by filled turnover.
The field is null when there are no fills or any fill lacks that benchmark.
Cash PnL uses fill prices, fees and settled funding, so it also captures the
spread paid at the touch.

Paper fills still assume the discounted displayed liquidity is available to
this order. The replay models one pair at a time and does not simulate venue
queue priority, liquidations, collateral transfers or intraframe price paths.
The capital charge is an entry reserve. Realized PnL accounts for
simulated fills, fees and settled funding. A time or session cutoff without sufficient
depth can leave positions open. Preserve those cases when comparing strategies.

## Maker-first replay

`cross_venue maker-first-replay CONFIG TAPE` consumes ordered public trade
prints, two compiled books, trade-feed status and an explicit end record.
The campaign driver builds this tape from `perp-compile` output and sealed,
normalized public trades. The input binds each print to its native market
and retains exchange and receipt time. It deduplicates subscription replay
by venue trade ID. Only a trade can advance the modeled passive queue.

The touch control selects one side at the best displayed price. The edge
policy also prices both hedge IOC limits, both maker fills, all trading
fees and an adverse-price reserve before placing a quote. A resting edge
quote requests cancellation if its hedge deteriorates beyond that reserve.
The simulator applies post-only validation at arrival, reserves the order
through cancel latency, and advances displayed queue ahead with aggressor
prints. A partial becomes hedgeable after the modeled fill notice. The
hedge sends the confirmed net quantity rounded down to the hedge lot, up
to the protected depth. Smaller residuals stay visible. The simulator
drains the pair on timeout and preserves any residual at the cutoff.
[Queue-reactive order books](https://arxiv.org/abs/1312.0563) ·
[Optimal order placement](https://arxiv.org/abs/1210.1625) ·
[Cross-exchange market making](https://hummingbot.org/strategies/v1-strategies/cross-exchange-market-making/) ·
[Quoting under adverse selection](https://arxiv.org/abs/2508.20225)

Set `entry_side` to `"buy"` or `"sell"` to isolate an opening direction.
Omit it to compare both directions at each decision. The optional
`passive_offset` policy waits for a same-venue post-only reduction while
confirmed inventory fits `max_net_notional` and `max_wait_ms`. The
entry check still requires protective hedge depth for the opening side.
An edge-gated reduction prices the remaining close against the cash
already booked. The wait budget includes maker transport, cancel delay
and the late-print window. At the boundary the runner cancels the
reduction, lets its delivery window expire and sends a protected IOC
for the confirmed residual. The hold and exposure stops still govern
the emergency close. Any remaining position stays visible in the report.
[Inventory internalization and external hedging](https://doi.org/10.1111/mafi.12367)

An edge-gated offset may supply `close_probability` as `[buy, sell]`
for the two opening directions. The expected cash after the first
fill weights the two-maker close and the fallback using these inputs.
The fallback prices a taker close on the maker leg and both protected
hedge IOCs. The runner waits for a passive reduction when its expected
cash beats immediate hedging and passes the entry floor. The counts
of initiated and completed passive cycles include open attempts in
the denominator. Estimate probabilities on earlier windows for the
same route, side, wait budget and latency.
[State-dependent limit-order fills](https://arxiv.org/abs/2403.02572)

Maker-first replay also accepts `pair.mode.kind = "turnover"` with
the edge policy enabled. Opening checks apply the remaining gross
turnover, fee and cash-loss budgets to the modeled round trip and IOC
fallback. The incentive variant ranks by the conservative cost per
unit of turnover on the reviewed venue. It credits simulated fills
within the eligibility interval after the cycle finishes flat. If
actual simulated costs breach the budget, entry stops and the existing
exit and recovery rules manage any remaining exposure. The projected
budgets can be exceeded by later price moves. The report keeps cost
unavailable for unresolved windows and assigns zero value to rewards.

An optional `turnover_feedback` object enables cost-based quote selection
when the maker venue is the incentive target. The replay searches up to eight
displayed prices per side within `max_quote_distance_bps` and starts with the
closest price that passes the budget. A resting entry rechecks executable
hedge depth and its cost allowance. `min_reserve_bps` sets a positive safety
margin and `max_reserve_bps` caps it. `alpha` in [0, 1] sets the weight of the
latest completed cycle. Use zero for a fixed-reserve control.
`overhead_bps` charges modeled operating costs on both venues' gross
turnover. Fee-inclusive cash after overhead updates the reserve only from
evidence-complete, flat cycles. Exits use the existing exposure controls.
The report distinguishes target-venue turnover, modeled eligible turnover and
unverified venue credit. Points and prospective rebates have zero value.
[Cross-Exchange Mining feedback](https://hummingbot.org/strategies/v1-strategies/cross-exchange-mining/) ·
[Queue position valuation](https://moallemi.com/ciamac/papers/queue-value-2016.pdf)

The report counts maker and IOC fills separately, records signed IOC
slippage against the submit touch, separates depth impact and fees, and
measures time with net exposure from the maker print's receipt. Public L2
provides displayed size ahead, with unknown cancellation priority and
exact order rank. The tape cannot reproduce intraframe quote transitions.
PnL for a window with an unresolved position or a trade gap while an
order is resting is unavailable. Closed-cycle cash and the marked
residual are reported independently. The maker study uses windows between
settlements. A separate carry run consumes verified payment events for
any position held across the settlement boundary.

Run the study with a directory of paired `.ndjson.zst` captures, the
archived public market terms and UTC hour boundaries in milliseconds.

```sh
python3 scripts/replay_maker_first_campaign.py target/release/cross_venue \
  "$campaign" "$compiled" "$market_terms" "$study/maker-first" \
  --begin-ms "$begin" --holdout-ms "$holdout" --end-ms "$end"
python3 scripts/replay_witnessed_carry.py target/release/cross_venue \
  "$campaign" "$compiled" "$study/carry" --start-ms "$begin" --end-ms "$end"
```

The funding run projects each leg from three prior published hourly rates
as available at the decision time. It accounts for signed cash using
the bounded settlement receipts attached by `perp-compile`. The separate
per-slot report gives the witnessed long-Lighter/short-Hyperliquid cash
interval per unit of base before fees. The native engine requires a verified
event for every held settlement.
[Perpetual futures pricing](https://www.nber.org/papers/w32936) ·
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)

### Oasis BTC and ETH execution study

The replay spans 03:00–09:00 Beijing time on November 13, 2026. The
final four hours are the holdout. It uses 100 ms compiled books, public
aggressor prints, a $100 quote target, 20% displayed-depth participation,
and 250 or 500 ms modeled one-way transport. The edge gate requires
0.5 bps after both trading legs and a 1 bps adverse-price reserve.
The model applies the published Lighter action delays, base-tier
Hyperliquid fees and the archived Arcus effective fees. Arcus USD quotes
are converted to USDC at par. Coinbase supplies the compiler's independent
reference price.
[Lighter fees and delays](https://docs.lighter.xyz/trading/trading-fees) ·
[Hyperliquid fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees) ·
[Arcus market metadata](https://docs.arcus.xyz/api-reference/public/get-markets)

Oasis run `maker-first-62e4ba4eadda` completed 480 windows across both
markets and both maker assignments for each pair. All 160 edge-policy
holdout windows stayed flat. The only submissions were six BTC quotes
on Hyperliquid with Lighter as hedge. All six were cancelled before
activation after the hedge check changed. The best protected edge for
that pair was 5.66 bps. ETH peaked at 0.345 bps, below the 0.5 bps gate.

The table summarizes the touch controls across fee tiers, maker
assignments and modeled transport delays. Fill rates include maker and
IOC fills over all evaluated time. Net PnL sums only windows that
finished flat with usable trade coverage. Each row aggregates independent
counterfactual runs on the same market history.

| Market | Venue pair | Fills per hour | IOC slippage (bps) | Longest net exposure | Complete windows | Net PnL of complete windows |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| BTC | Lighter / Hyperliquid | 36.3–154.4 | −0.042 to +0.166 | 59.8 min | 4 / 32 | −$12.42 |
| BTC | Lighter / Arcus | 36.5–133.9 | −0.100 to +0.194 | 59.9 min | 0 / 32 | Unavailable |
| BTC | Hyperliquid / Arcus | 28.3–130.9 | −0.061 to +0.157 | 56.9 min | 4 / 16 | −$19.60 |
| ETH | Lighter / Hyperliquid | 37.5–105.9 | −0.223 to +0.185 | 59.7 min | 12 / 32 | −$22.48 |
| ETH | Lighter / Arcus | 3.0–68.3 | −0.111 to +0.225 | 59.6 min | 0 / 32 | Unavailable |
| ETH | Hyperliquid / Arcus | 3.5–85.6 | −0.067 to +0.082 | 58.4 min | 5 / 16 | −$15.72 |

The 25 complete touch windows lost $10.91 before fees and $70.22
after fees, with hourly losses from $0.65 to $6.62. Fees contributed
$59.30. Across all touch runs, 1,531 of 2,083 closed cycles used a
forced close. Residual exposure persisted when a fill or an unwind
fell below venue minimums or could not execute within the limit.
The full-close fallback reduced cutoff residuals from 107 to 90 windows.
Trade-feed gaps while orders were resting also invalidated windows.
Arcus maker controls therefore provide no verified complete-window
cost for turnover against Lighter. Prospective rewards retain zero
value in the comparison.

Entropy and TradeXYZ mainnet metadata returned no matching BTC or ETH
perpetual in their deployer namespaces. The previous campaign has BULK
public books and trades. Those archived prints lack stable trade IDs,
and the current official market-metadata request returned HTTP 403 from
Oasis. BULK is excluded from the fee- and precision-qualified matrix.
[BULK contract specifications](https://docs.bulk.trade/bulk-exchange/Contract-Specifications) ·
[BULK exchange info](https://docs.bulk.trade/api-reference/getExchangeInfo)

#### Witnessed settlement

Six joint hourly settlements per market were reconciled from Lighter
and Hyperliquid protocol witnesses. Holding one unit of base long on
Lighter and short on Hyperliquid through all six would receive
0.92938–0.93018 USDC per BTC and pay 0.03970–0.03971 USDC per ETH before
trading costs. The eight funding carry runs had zero entries. The best
Standard net entry edges were −15.23 bps on BTC and −16.60 bps on ETH.
Premium edges were −20.83 and −22.20 bps. Arcus public funding history
matched six BTC and six ETH timestamps in the window. Its payment
valuation is unavailable.

The compiled tapes and per-window reports are archived on Oasis.

| Aggregate report | SHA-256 |
| --- | --- |
| Maker-first execution | `f93dfc060fac81d13cad47b41840f0bbb7b04927b1108fa9d9ae4dd5b43e0c7e` |
| Witnessed carry | `cb9d28c8390c5556b2816c92fed53ddaeaeac417209f46e1e4ffb444a5e314d6` |
| Capture and contract evidence | `c4b118e90fde5cd186f489c4f3980db0c117489e8145e2860aec0e030d533604` |

### Passive inventory offset, 2026-11-13

Oasis run `passive-offset-aa493f40a3f7` evaluated 09:00–12:00 Beijing
time, with 10:00–12:00 as holdout. It used fresh public prints and
100 ms books for BTC and ETH on Lighter–Hyperliquid and Lighter–Arcus.
The $100 quote target, $105 passive inventory limit, four-second wait,
seven-second exposure stop and 250 ms modeled transport were fixed.
The comparison kept the fees, risk limits and emergency timeouts
identical between immediate and passive-offset touch policies. The research
also replayed the two opening directions independently.

The 60 holdout windows include 12 flat windows with usable coverage.
Eight had fills and lost $11.01 in aggregate. The other four were
the Arcus edge cases and had zero fills. Each Arcus direction below
had one complete hour. Its other hour ended with residual inventory.

| Arcus maker and Lighter hedge | Long-first cost (bps of Arcus maker turnover) | Short-first cost (bps of Arcus maker turnover) |
| --- | ---: | ---: |
| BTC | 8.57 | 7.91 |
| ETH | 11.87 | 8.20 |

On Lighter, the edge-gated passive policy booked 13 two-maker cycles
across the four holdout routes. Those cycles earned $0.237 on $2,415
of maker turnover. The same policies lost $7.17 across all closed
cycles and left residuals in every full window. Forced exits accounted
for 79 of 94 closed cycles. The BTC Lighter maker with Arcus hedge and
the passive touch policy finished both holdout hours with matched base
positions still open. It recorded 396 fills per hour and $10.59 of
losses on closed cycles. The edge-gated Lighter policies finished with
single-leg residuals worth up to $4.83 at the last displayed mid.
The two-maker cycles were 13.8% of closed cycles. At the two groups'
observed average gains and losses, break-even on closed cycles alone
would require 83.4% two-maker cycles. Improving the entry estimate
requires predicting the chance of a passive offset within the wait
budget and pricing the fallback using executable depth and fees.

For the preceding six witnessed funding slots, BTC's positive funding
direction was long Lighter and short Hyperliquid in all six. ETH
favored short Lighter and long Hyperliquid in four. A direction selected
after settlement is a diagnostic. The causal funding replay retained
the executable fee and depth gates and placed zero entries.

The aggregate report SHA-256 is
`78e55272464ab6c071de1b4317eeab38afc72b24af36cfac76a4d7fdabd5e8bc`.

## Entropy and venue rewards

- Entropy is the Hyperliquid HIP-3 deployer `io`. It shares Hyperliquid's
  infrastructure risk. `cross_venue markets entropy` shows the native
  collateral ID, delisting status and fee mode. Check the contract and
  account fee tier for the observed period. The equity oracle references the
  public listing during regular trading hours and can blend Entropy's own
  book outside that session. Keep a separate underlying reference. Entropy's
  rebate terms apply to its share of eligible fees and require an Entropy
  account for the trading address.
  [Entropy fees](https://docs.entropy.io/equity-perp-mechanics/fees),
  [equity oracle](https://docs.entropy.io/equity-perp-mechanics/oracle-price),
  [rebates](https://docs.entropy.io/about-entropy/referrals).
- Arcus publishes Season 1 points rules and considers trading quality and
  liquidity. Self-matching and wash trading can reduce or forfeit points.
  Use the venue's posted account points when reconciling eligible activity.
  [Arcus Points](https://arcus.xyz/blog/introducing-arcus-points),
  [Protocol Terms](https://arcus.xyz/legal/protocol-terms).
- BULK publishes Trading AURA and weekly Challenger Series rules. The
  series excludes traders BULK classifies as market makers and screens
  activity for wash trading and Sybil behavior. Use `cross_venue markets bulk`
  and `cross_venue bbo bulk BTC-USD` to inspect its public markets. The
  integration is read-only.
  [Challenger Series](https://docs.bulk.trade/bulk-exchange/aura),
  [BULK exchange info](https://docs.bulk.trade/api-reference/getExchangeInfo).

The planner values future points and airdrops at zero. Paper turnover
measures execution economics. Reconcile live fills and any rewards posted
by the venue separately from strategy PnL.

## Oasis holdout, 2026-11-12

The sealed campaign contains 20 hours of public observations. Crypto uses
the final eight hours for evaluation. NVDA and TSLA use 14:00–16:00 New York
time on November 11, with the first receipt of each independent Nasdaq
minute bar. Equity results assume matched one-share linear payoffs and
the reviewed regular-session calendar. Coinbase supplies the crypto
reference. The collector and the research build are based on
`3182f3710c7521789822938b30ea53d5bb23ba0f`.

### Pair economics

The table shows the best recorded market per venue pair under the public
fee scenario. The screen uses $100 maximum per leg, $200 of pair capital,
20% of displayed depth, 250 or 500 ms modeled one-way latency and a
1–60 second hold within a single funding hour. It charges four taker fills.
The optimizer selects nonoverlapping round trips with hindsight. The
percentages scale the resulting ceiling linearly to 24 crypto hours or a
6.5-hour equity session. Each ceiling assumes both orders fill together.

| Pair | Market | 250 ms daily ceiling | 500 ms daily ceiling |
| --- | --- | ---: | ---: |
| Lighter × Hyperliquid | ETH | 0.3492% | 0.3180% |
| Lighter × Arcus | NVDA | 0.0287% | 0.0287% |
| Lighter × Entropy | NVDA | 2.7894% | 2.7556% |
| Lighter × TradeXYZ | NVDA | 4.6425% | 4.4872% |
| Arcus × Hyperliquid | ETH | 0.1156% | 0.1156% |
| Arcus × Entropy | NVDA | 0.4733% | 0.4733% |
| Arcus × TradeXYZ | NVDA | 0.5110% | 0.5110% |
| Entropy × TradeXYZ | NVDA | 0.3135% | 0.3135% |

Native Hyperliquid had no matched contract with Entropy or TradeXYZ in
this archive. The public taker scenario uses 0 bps for Lighter, 4.5 bps
for native Hyperliquid, 0.9 bps for the captured HIP-3 growth markets, and
4.5 or 2.25 bps for Arcus crypto or equity markets respectively. Raising
Lighter to 4 bps and the HIP-3 markets to 9 bps eliminated the corresponding
positive ceilings. Arcus × Hyperliquid ETH retained its 0.1156% ceiling.
[Hyperliquid fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees) ·
[Entropy fees](https://docs.entropy.io/equity-perp-mechanics/fees) ·
[Arcus public markets](https://api.arcus.xyz/v1/markets)

Rust `perp-replay` produced zero entry signals and zero fills across 24
crypto and 48 equity holdout trials, including the lower entry band and
turnover variants. The most frequent pricing gate was `basis_below_entry`.
`net_edge` also rejected priced candidates, especially on the equity
routes. Arcus lacked point-in-time oracle context in this sample, so its
results use the depth screen. Entropy and TradeXYZ share an infrastructure
risk domain in the conditional paper scenario.

### Funding carry

The public-rate screen tested 59 executable-window estimates across the
two latencies and six contract routes. It forecasts from three already
published payment rates and measures later rates using the last fresh
public midpoint as a valuation proxy. It charges delayed entry and exit
depth, four taker fees and a five-basis-point reserve on one leg's
notional. Zero estimates passed the entry cost gate. The best net estimate
was −$0.0455 on Lighter × TradeXYZ TSLA. The published rate and a depth
midpoint leave the actual funding cash amount unverified.

The Rust carry replay uses the range of each leg's last three available
payment rates and the recorded oracle price at each decision. It evaluates
one through eight reachable hourly payments for crypto and one for
equities, including the settlement guard and unwind window. All 12
holdout runs finished flat with zero signals and fills. `net_edge`
rejected 159,450 candidate evaluations across routes and latency runs.
`funding_no_credit` rejected another 87,216. The best priced edge was
−11.34 bps on TSLA. The engine requires verified cashflow evidence for
any held leg that crosses a payment. [Hyperliquid funding valuation](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)

### Lighter maker and turnover

`maker_replay` runs the production quote loop with a virtual clock, real
public L2 and prints, modeled order acknowledgments and a price-time queue
estimate. The test uses $1,000 of capital and a fixed $50 quote target.
Each replay ends flat before the hourly payment boundary. Aggregated L2
leaves exact queue rank and cancellation priority unobserved. The table
shows independent 250 and 500 ms runs for the zero-fee turnover candidate
over approximately eight hours, at the modeled observed turnover pace.
These historical cases use a uniform simulated order delay. A tier-specific
replay can model transport with `order_entry_ms` and add venue delays with
`venue_cancel_delay_ms` and `venue_taker_delay_ms`. A mixed batch becomes
eligible at the latest constituent arrival. The published Standard account
schedule adds 300 ms to cancels and taker orders. Premium adds 140 ms to
taker orders and publishes 0.4/2.8 bps maker/taker without staking.
[Lighter trading fees](https://docs.lighter.xyz/trading/trading-fees)

| Market | Filled turnover | Net PnL | Net cost per $1M traded | Simple annual extrapolation |
| --- | ---: | ---: | ---: | ---: |
| BTC | $75,312–$78,457 | −$2.76 to −$2.43 | $30.93–$36.62 | −302% to −266% |
| ETH | $89,379–$92,569 | −$4.67 to −$4.45 | $48.12–$52.24 | −511% to −488% |

The annual column scales short-window PnL on fixed capital without
compounding. The cost per traded dollar supports budgeting for a planned
turnover rate. The training selection found no qualifying maker policy.
In the 0.38/4 bps fee stress, extending the inventory timeout from
30 to 120 seconds reduced aggressive exit fills from 327–346 to 81–86 on BTC
and from 318–343 to 64–65 on ETH. BTC's best candidate in that scenario lost
$4.00 and ETH's lost $4.95. No simulated rewards offset those costs.

### Reproduce

Run on the campaign host with Python 3.11 or newer, `gzip` and `zstd`.
The campaign directory contains `campaign.json`, public archives in
`history/` and the sealed inputs under `results/final/`. Use the full
recorded code identity, including the research overlay hash when applicable.

```sh
cargo build --locked --release --features research-replay --bin cross_venue --bin maker_replay
campaign="$HOME/aleph-tx-research/2026-11-12-20h-full"
terms="$campaign/results/production-study-2effb88e8816/public-market-terms.json"
study=$(mktemp -d "$campaign/results/replay-XXXXXXXX")
revision=$(git rev-parse HEAD)
python3 scripts/replay_maker_campaign.py "$campaign" target/release/maker_replay \
  config.example.toml "$study/maker" --revision "$revision"
python3 scripts/replay_pair_campaign.py "$campaign" "$terms" target/release/cross_venue \
  "$study/pair" --revision "$revision"
python3 scripts/replay_pair_campaign.py "$campaign" "$terms" target/release/cross_venue \
  "$study/equity" --revision "$revision" --equity-template examples/nvda_capture.json
python3 scripts/replay_funding_campaign.py "$campaign" "$terms" "$study/funding.json"
python3 scripts/replay_funding_campaign.py "$campaign" "$terms" "$study/carry" \
  --engine target/release/cross_venue --equity-results "$study/equity" --revision "$revision"
```

The archived aggregate report checksums are SHA-256.

| Report | SHA-256 |
| --- | --- |
| Pair economics and crypto paper | `e640bbdca9d0cc0886599ca01aefffa9bcf51ac13db5a1a274976e73ab8a4eb6` |
| Equity paper | `10f0db89b01972d1261953d539e7942cf204777467123738f48f6891cfe28f72` |
| Funding rate screen | `4b410c6479e457f26e86ffac68e46b593cad6dde7ac5790d589fd557f141f3f4` |
| Rust funding carry | `7e0ad7a56bcec00afede02da02fd1f68c708283182e29b3038b31278a65cd07b` |
| Maker and turnover | `3947c1f25b94a54f502e153dee73628057c351f8672c3939bf318a2d1eb650aa` |

## Market selection and execution sensitivity, 2026-11-13

The 15 GB campaign reused BTC, ETH, NVDA and TSLA from the prior cross-venue
transport preflight. Market overlap and independent references determined
the initial universe. At approximately 17:32 UTC on November 12, venue
APIs reported the following ranks and shares of active perpetual volume.

| Venue | Captured markets and volume ranks | Captured share |
| --- | --- | ---: |
| Lighter | BTC 1, ETH 2, NVDA 3, TSLA 24 | 75.1% |
| Native Hyperliquid | BTC 1, ETH 2 | 75.4% |
| Arcus | NVDA 1, BTC 3, ETH 6, TSLA 25 | 38.5% |
| Entropy | NVDA 1 | 41.7% |
| TradeXYZ | NVDA 1, TSLA 26 | 35.4% |

SNDK ranked 12 on Lighter, 5 on Arcus, 3 on Entropy and 7 on TradeXYZ.
HYPE and SOL ranked in the top six on Lighter and native Hyperliquid.
Screen a new route by matched contract terms, synchronized executable
depth on the smaller leg, and the cost of completing both round trips.
[Lighter market data](https://mainnet.zklighter.elliot.ai/api/v1/orderBookDetails) ·
[Hyperliquid asset contexts](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals) ·
[Arcus public markets](https://api.arcus.xyz/v1/markets)

The NVDA Lighter × TradeXYZ depth ceiling at 500 ms dropped from 4.49%
per equity session at $100 per leg to 4.08% at $250 and 2.66% at $1,000.
The $1,000 case had a $16.36 hindsight ceiling over the two-hour
holdout. The corresponding $1,000 NVDA Lighter × Entropy ceiling was
$8.32. These figures assume both legs fill together. Another 48 Rust
replays lowered the basis entry threshold to 0.75 and compared partial
and full reversion credits. All had zero entry signals, with a best
holdout edge of −4.31 bps at a 2 bps entry floor. All six subsequent
holdout replays with a common 800 ms taker delay also finished flat.

BTC and ETH had 20 matched public hourly payments in the sealed
campaign. The largest ex-post eight-hour gross carry was 0.669 bps on
BTC and 0.442 bps on ETH. The Lighter Standard × native Hyperliquid
round-trip fee floor was 9 bps on one leg's notional, before depth and
risk costs. Recompute the hurdle from the actual fee tier and contract
settlement model for each route.
[Lighter funding](https://docs.lighter.xyz/trading/funding) ·
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)

The calibrated maker study uses 250 and 500 ms transport scenarios and
the published order-specific Lighter delays. None of the five policies
qualified in the earlier training period. With $1,000 capital and a $50
quote target, the Standard policy with a 12 bps round-trip profit floor
lost $0.08–$0.12 on BTC in the older holdout and gained $0.29–$0.32 in
the first independently sealed hour of the new campaign. ETH gained
$0.02–$0.11 in the older holdout and lost $0.19–$0.20 in that later hour.
The high-turnover Standard baseline incurred $36–$48 on BTC and $50–$78
on ETH per $1 million of turnover across those evaluation windows.
The narrow exposure sample supports testing conditional quoting over
additional sessions. Turnover economics use completed cycle PnL and
realized rewards, with zero cash value assigned to speculative points.

Oasis study `economics-cd85571a0ef5` archives the policy, aggregate reports
and event-driven trials. The capacity report is in `capacity-5be957b08c46`
under the same production-study directory. The tier-specific results and
binary identity are under `calibrated-b68667a96c27`.

## Objective-specific replay, 2026-11-13

Oasis study `objectives-ccb0485e68ad` tested the directional markout
reserve against the calibrated maker controls. It used the earlier
20-hour archive and the second sealed hour of the new campaign, with
250 and 500 ms transport delays, tier-specific action delays, $1,000
capital and a $50 quote target. At 500 ms in the new hour, the Premium
ETH candidate with the reserve lost $0.75 on $9,388 of turnover.
Its control lost $1.45 on $12,402. Maximum drawdown fell from $1.41
to $0.74. All eight comparisons on the 2 bps profit-floor policy had
smaller losses in that hour. The 12 bps profiles had the same outcomes
at the reported precision.

All eight equity profit-exit trials had zero entry signals at 500 and
800 ms. The best priced entry was −4.31 bps. The synthetic regression
exercised the discretionary exit floor and completed a time-driven
exit after the floor blocked the earlier close.

For the conditional Entropy incentive study, neither the Lighter nor
TradeXYZ hedge entered at 0, 2 or 4 bps of cost per target turnover.
At 8 bps, each of four trials filled only the Entropy leg and missed
its hedge deadline. Each full run stopped when the open short reached
a settlement without verified cashflow. The truncated risk reports
preserve the short exposure before that payment.

A separate one-leg sensitivity found public depth for full protected
covers 1.45–2.35 seconds after the missed deadlines, for simulated
losses of $0.014–$0.046 per attempt after fees. This sensitivity
establishes a candidate reduce-only recovery path. Qualifying activity
and realized rewards still depend on the venue's program terms.
[Entropy fee rebates](https://docs.entropy.io/about-entropy/referrals) ·
[BULK Challenger Series](https://docs.bulk.trade/bulk-exchange/aura)

## Quote cadence and hedge recovery, 2026-11-13

Oasis study `neutral-hft-c8966c633245` reran the NVDA holdout with
`recover_single_leg` enabled. All four trials at the 8 bps target-turnover
cost cap missed the second-leg deadline. Each closed the single open leg
on Entropy with a protected IOC after the modeled transport delay.

| Hedge venue | One-way latency | Time with net exposure | Net PnL after fees |
| --- | ---: | ---: | ---: |
| Lighter | 500 ms | 1,613 ms | −$0.0142 |
| Lighter | 800 ms | 1,588 ms | −$0.0349 |
| TradeXYZ | 500 ms | 2,480 ms | −$0.0349 |
| TradeXYZ | 800 ms | 2,518 ms | −$0.0462 |

All four finished flat with zero completed pairs and zero eligible
turnover. At the 4 bps cap, all four trials had zero entries and
`turnover_cost` was the leading entry rejection. The replay stopped
new entries after the first failed hedge. The archived `recovery.json`
SHA-256 is `dc177226ba6972e88e93711044abfccce46191b179bc54ad75e475ba4a0b5d74`.

The maker study replayed 100 ms book frames and Lighter public
trades through the production quoting loop. The second sealed hour
was the comparison period. The next hour began at 17:51 UTC on
November 12 and served as the chronological holdout. The fixed
profiles used $1,000 starting capital, a $50 quote target, a 2 bps
profit floor and Standard or Premium fees and action delays. The
replay compared 50 and 250 ms minimum cycle intervals, at 250 and
500 ms modeled one-way transport. Average inventory is the
time-weighted absolute marked position from the account samples.
[Lighter trading fees](https://docs.lighter.xyz/trading/trading-fees)

The 128 windows finished flat. All 32 holdout configurations lost
$0.31–$1.20 per hour after fees and had negative PnL before fees.
Adding the order-flow reserve to the directional markout control
improved eight of 16 matched comparisons. A separate linear fit
using the comparison hour's Hyperliquid best-book imbalance had
holdout 500 ms forecast RMSE of 0.46161 bps on BTC and 0.63196 bps on
ETH. The constant baselines were 0.46157 and 0.63232 bps respectively.
The 50 ms interval improved PnL in seven of eight BTC comparisons and
two of eight ETH comparisons.
[Order flow imbalance](https://doi.org/10.1093/jjfinec/nbt003) ·
[Queue position and latency](https://arxiv.org/abs/1610.00261)

| Market and fee tier | Modeled transport | Order-flow reserve | Minimum cycle | Fills per hour | Avg. inventory | Net PnL per hour |
| --- | ---: | --- | ---: | ---: | ---: | ---: |
| BTC Standard | 500 ms | Off | 50 ms | 186 | $38.42 | −$0.356 |
| BTC Standard | 500 ms | On | 50 ms | 180 | $34.83 | −$0.312 |
| ETH Premium | 250 ms | On | 50 ms | 158 | $42.22 | −$1.203 |
| ETH Premium | 250 ms | On | 250 ms | 146 | $31.87 | −$0.977 |

These results measure modeled execution on one holdout hour.
Inventory is for the Lighter maker leg. The maker trial had no
external hedge leg and closed each window before its funding boundary.
The cadence parameter controls quote-loop wakeups. Actual queue position,
venue latency and sub-100 ms book transitions require finer evidence.
The study archives configs, binary hashes and trial reports on Oasis.
The SHA-256 values for `maker-report.json` and `signal.json` are
`a0b3c940262cab1c121d982d5631efaba94558152ca3573ca19ff8e1bd275ceb` and
`acc4e811b8596abf4252c8024478254bea46dbef8e621d2d2ac38ff7ef84d013`.

## Adaptive quoting and turnover replay, 2026-11-13

Oasis study `adaptive-strategy-07480bdf7830` used BTC and ETH public
trades and 100 ms compiled depth. Comparison data spans 09:00–12:00
Beijing time. The chronological holdout spans 12:00–13:50. The cutoff
uses the common sealed depth and trade coverage. The script verifies
trade receipts through the cutoff for Lighter, Hyperliquid and Arcus.
Each execution window is bounded by the venues' hourly settlements.
The study uses 250 and 500 ms one-way transport scenarios, plus the
Lighter tier's venue action delays. All PnL is in modeled quote units,
with Arcus USD treated at par with USDC for the crypto pair.
[Lighter fees and action delays](https://docs.lighter.xyz/trading/trading-fees) ·
[Hyperliquid fees](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees)

### Inventory-neutral quant

`maker_replay` runs the production quote loop with the public books,
aggressor trades, a simulated order tracker and a $50 quote target on
$1,000 starting equity. The minute-range candidate uses five completed
external midpoint ranges, an 8 bps trigger, a 3 bps maximum widening and
a 0.5 minimum size scale on the inventory-adding side. The comparison
keeps the corresponding fee tier and 2 or 12 bps profit floor fixed.
[Crypto volatility and liquidity cycles](https://arxiv.org/abs/2109.12142) ·
[Configuration](CONFIGURATION.md)

All 64 holdout windows finished flat. The minute-range policy reduced
time-weighted absolute inventory by 3.2–55.7% across the 16 matched
comparisons and improved PnL in eight. The figures below use the Standard
tier and a 12 bps floor. Each row combines two windows totaling 1.831 hours.

| Market | Transport | Control PnL/h | Minute range PnL/h | Minute range fills/h | Mean inventory control → minute range |
| --- | ---: | ---: | ---: | ---: | ---: |
| BTC | 250 ms | −$0.008 | −$0.035 | 22.4 | $18.29 → $11.95 |
| BTC | 500 ms | −$0.065 | −$0.147 | 23.5 | $18.33 → $11.24 |
| ETH | 250 ms | +$0.215 | +$0.160 | 29.5 | $21.63 → $12.32 |
| ETH | 500 ms | −$0.011 | +$0.014 | 36.6 | $23.02 → $12.53 |

The ETH candidate is positive at both modeled delays in this sample.
Its matched control earns more at 250 ms. All Premium candidates and
all 2 bps candidates lost money. The volatility policy remains opt-in.

### Cross-venue arbitrage

The $100 maker-first profiles compare an immediate hedge, a passive
offset scored at displayed prices, and the same offset scored with
estimated completion and protected fallback costs. The estimate uses
only comparison-window starts and full passive closes by route, opening
direction and latency. It uses a one-sided Wilson lower estimate with
zero credit below 20 attempts. Serially dependent orders make this a
conservative scoring heuristic. The Lighter maker estimates ranged
from 1.3% to 11.1%. Arcus and Hyperliquid maker samples fell below the
minimum for both directions. The maximum passive wait is five seconds.
[Cross-venue price formation](https://arxiv.org/abs/2108.09750) ·
[Queue state and limit-order fills](https://arxiv.org/abs/2403.02572)

The calibrated variant and the immediate-hedge control each finished
32 holdout windows flat with zero fills. Lighter maker estimates of the
best net entry ranged from −6.51 to −2.13 bps across the four market
routes. Some Hyperliquid maker quotes cleared the price gate and had no
simulated executions. The displayed-price offset generated 490 maker
and 782 IOC fills across its 32 windows. Fifteen ended with residuals.
Its 303 closed cycles lost $22.45, including cycles in the incomplete
windows. The 17 complete windows lost $4.06 in aggregate. Its largest
residual exposure persisted for about 60 minutes, so the exit timer
alone cannot bound realized exposure.

The sequential IOC basis control also finished all 16 windows without
entries. On Lighter–Hyperliquid the best priced edge was −14.11 bps for
BTC and −14.44 bps for ETH. The Arcus routes failed the fresh venue
oracle gate before pricing an entry. The same evidence gap blocked all
24 Arcus sequential IOC turnover trials. The maker-first Arcus rows
measure short-horizon depth and trade execution between settlements.

A separate six-hour funding run verified six Lighter–Hyperliquid
settlements for each asset. All eight configurations finished with
zero entries. The best Standard net entry estimates were −15.23 bps
for BTC and −16.60 bps for ETH with execution costs and the basis
reserve. The archived funding report retains each settlement.
[Perpetual funding and transaction-cost bounds](https://arxiv.org/abs/2212.06888)

### Turnover with an Arcus maker

The replay tests a $2,000 gross turnover budget, a $1 projected fee
budget and a $0.50 cash-loss entry stop per window. The unit cost uses
cash PnL across both legs and simulated Arcus turnover in flat cycles.
At 0 and 4 bps caps all windows finished without fills. The best
projected maker-first cost was 5.51 bps for BTC and 5.57 bps for ETH.
At 8 bps both maker-first variants produced identical executions in
this sample. All trading windows finished flat.

| Market | Transport | Modeled Arcus turnover | Net PnL | Fees | Cost per Arcus turnover | Fills/h | Longest net exposure |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| BTC | 250 ms | $1,897 | −$0.665 | $0.569 | 3.50 bps | 30.0 | 9.56 s |
| BTC | 500 ms | $1,824 | −$0.771 | $0.547 | 4.22 bps | 30.0 | 12.49 s |
| ETH | 250 ms | $892 | −$0.382 | $0.268 | 4.28 bps | 15.8 | 9.44 s |
| ETH | 500 ms | $1,220 | −$0.814 | $0.366 | 6.67 bps | 22.9 | 10.30 s |

All 46 completed cycles in these four configurations used a timed or
risk-driven close. The seven-second exposure stop starts draining.
Cancel and IOC completion can extend the actual exposure past that time.
The model assigns zero value to points. Live reward eligibility depends
on Arcus program rules and venue-reported credit.
[Arcus Points](https://arcus.xyz/blog/introducing-arcus-points) ·
[Protocol Terms](https://arcus.xyz/legal/protocol-terms)

The separate NVDA regression on archived Entropy–Lighter and
Entropy–TradeXYZ books reproduced zero completed pairs at 500 and 800 ms.
At an 8 bps turnover cap, four cases recovered the first fill after the
second leg missed its deadline. Recovery took 1.59–2.52 seconds of net
exposure and lost $0.014–$0.046 per case, with zero modeled eligible
turnover. The current BTC/ETH study uses the four qualified Lighter
routes. Entropy and TradeXYZ require a matching listed contract, and BULK
needs qualified market terms and public prints for a maker comparison.

### Reproduce

The study directory on Oasis contains the compiled tapes, frozen per-run
configs, reports and binary identities. Its comparison tape is under the
earlier `passive-offset-56c7dc666408/compiled` directory. Run the study with
Python 3.11 or newer and `zstd`.

```sh
cargo build --locked --release --features research-replay --bin cross_venue --bin maker_replay
quality="$HOME/aleph-tx-research/2026-11-12-quality-15gb"
parent="$HOME/aleph-tx-research/2026-11-12-20h-full/results/production-study-2effb88e8816"
study="$parent/adaptive-strategy-07480bdf7830"
result=$(mktemp -d "$parent/replay-XXXXXXXX")
python3 scripts/replay_strategy_campaign.py "$quality" "$study/market-terms.json" \
  "$parent/passive-offset-56c7dc666408/compiled" "$study/compiled" \
  target/release/cross_venue target/release/maker_replay config.example.toml "$result/strategy" \
  --begin-ms 1794531600000 --holdout-ms 1794542400000 --end-ms 1794549000000
```

The archived aggregate report checksums are SHA-256.

| Report | SHA-256 |
| --- | --- |
| Production maker replay | `2f4b74370e416d2a688278815c0362d13151c1d80055f4f23299f073923110c0` |
| Maker-first arbitrage and turnover | `a725178638f4720add4e6989af2b4f65eb993e27e906021cb0665b2f6f737242` |
| Sequential IOC controls | `049ebf499869a273b22b1d008ed4d62434e23f2c6822a82e385bad3002e3c15b` |
| Completion probability calibration | `c7e1be25d074f6136b728445837891e0cb48e2b1c89588ccd66766335713133f` |
| Witnessed funding | `0aa133bfee6b72b5c9ab397cd10bcfd812e57e296a8cc39830f9d4b8f22634b4` |
| NVDA recovery regression | `888adb4e271fa1d093c6fe7f22fa14aee60918f4ead3ee3aed2cbfa991ca535a` |
| Input and binary evidence | `84239d0b136539cdbd555be4ed22b74d1c1eaac63ac8ea6dcb2848908fe2ae79` |

## Execution policy comparison, 2026-11-13

Oasis study `execution-policies-0d7f570b8753` reused the sealed 12:00–13:50
Beijing evaluation inputs. This is a matched comparison on an already
studied session. It has two independent execution windows per arm, 250
and 500 ms one-way transport scenarios, and the recorded fee schedules.
Public L2 and trade replay models queue position and partial fills with
displayed liquidity. It has no counterfactual market impact or actual
exchange priority evidence.
[Queue position valuation](https://moallemi.com/ciamac/papers/queue-value-2016.pdf) ·
[Replay fill assumptions](https://hftbacktest.readthedocs.io/en/latest/order_fill.html)

### Quant market making

The production quote loop adds an optional directional reserve from
Hyperliquid microprice and short-lived Hyperliquid–Lighter basis. The
signal decays over book age plus modeled placement delay. The comparison
uses a 500 ms half-life, a 750 ms book skew limit and a 3 bps reserve cap.
Both arms keep the same minute-range policy, $50 target, $1,000 equity,
fees and inventory bounds. All 64 windows finished flat. The new reserve
improved net PnL in 10 of 16 matched profiles, often by less than one
cent per hour. The Standard ETH 12 bps floor stayed positive at both
delays. The larger favorable and adverse changes occurred in Premium
2 bps profiles that still lost money.
[Configuration](CONFIGURATION.md) ·
[Cross-market Bitcoin price discovery](https://arxiv.org/abs/2108.09750) ·
[Latency and inventory research](https://arxiv.org/abs/2505.12465)

| Market and profile | Transport | Control PnL/h | Cross-market reserve PnL/h |
| --- | ---: | ---: | ---: |
| ETH Standard 12 bps | 250 ms | +$0.160 | +$0.147 |
| ETH Standard 12 bps | 500 ms | +$0.014 | +$0.017 |
| ETH Premium 2 bps | 250 ms | −$1.527 | −$1.380 |
| BTC Premium 2 bps | 250 ms | −$1.316 | −$1.444 |

The reserve stays opt-in. A profitability decision needs fresh sessions
and post-fill calibration at the actual placement and cancellation delays.

### Arcus turnover and cost

The touch control uses the existing entry gate. The fixed arm searches
for the nearest passing displayed price with a 0.25 bps reserve. The
feedback arm uses the same search with a 0.25 EWMA weight and a 4 bps
reserve cap. Both search arms limit price distance to 12 bps and add
0.25 bps of modeled overhead on gross turnover across both venues in
the entry gate. Reported results charge that overhead once in every arm.
The simulation gives zero value to points and prospective rebates.
[Cross-Exchange Mining](https://hummingbot.org/strategies/v1-strategies/cross-exchange-mining/) ·
[Arcus Points](https://arcus.xyz/blog/introducing-arcus-points)

All 72 windows finished flat. At 0 and 4 bps caps every arm recorded
zero fills. Each cell below shows the modeled Arcus turnover in completed
cycles and the full modeled cost in bps of that turnover at the 8 bps
entry cap. The cost includes fees, realized execution prices and overhead.

| Market | Transport | Touch volume / cost | Fixed volume / cost | Feedback volume / cost |
| --- | ---: | ---: | ---: | ---: |
| BTC | 250 ms | $1,897 / 4.00 | $1,862 / 4.20 | $891 / 3.85 |
| BTC | 500 ms | $1,824 / 4.72 | $1,671 / 4.49 | $1,196 / 4.32 |
| ETH | 250 ms | $892 / 4.77 | $300 / 13.12 | $278 / 13.69 |
| ETH | 500 ms | $1,220 / 7.16 | $926 / 7.32 | $617 / 7.85 |

Feedback reduced absolute loss and turnover in all four comparisons
against touch. Its unit cost improved on BTC and rose on ETH. The
250 ms ETH realized cost exceeded the 8 bps projected entry cap after
execution. Feedback's maximum observed net exposure lasted 9.55 seconds
across the four profiles. All evaluated arms had zero turnover at
nonpositive complete modeled cost. Actual venue credit is unavailable.

### Conditional settlement and variable carry

`matched-settlement` verified the synthetic example with a common 0.12
base lot, 12.00 quote units of terminal spread and 7.20 of bounded costs.
The conditional net is 4.80, or 16 bps on 3,000 of specified capital.
This exercises matching fixing rules and contract multipliers. Real dated
opportunities need independently verified instruments, tradeable size
and clearing terms. [Combination execution](CROSS_VENUE.md#dated-settlement-and-combination-orders)

The six-hour Lighter–Hyperliquid carry comparison used six witnessed
settlements for each asset. Both the original policy and the one-hour
credit half-life with a 0.5 bps/day gross-notional hurdle placed zero
entries across their eight cases. The original Standard cases reached
−15.23 bps for BTC and −16.60 bps for ETH after modeled costs. Under the
haircut, no reachable candidate had both complete forecast inputs and
positive conservative funding credit. The planner therefore reported
no priced entry edge, and the daily return floor did not bind.

### Reproduce

Build `cross_venue` and `maker_replay` with `--features research-replay`.
On Oasis, use the prior study's sealed inputs and the current binary.

```sh
quality="$HOME/aleph-tx-research/2026-11-12-quality-15gb"
parent="$HOME/aleph-tx-research/2026-11-12-20h-full/results/production-study-2effb88e8816"
result=$(mktemp -d "$parent/replay-XXXXXXXX")
python3 scripts/replay_execution_policies.py "$parent/adaptive-strategy-07480bdf7830/study" \
  "$quality" config.example.toml target/release/cross_venue target/release/maker_replay "$result/policies"
python3 scripts/replay_witnessed_carry.py target/release/cross_venue "$quality" \
  "$parent/maker-first-62e4ba4eadda/compiled" "$result/carry" \
  --start-ms 1794510000000 --end-ms 1794531600000 \
  --credit-half-life-ms 3600000 --min-net-return-bps-per-day 0.5
```

The Oasis aggregate reports are identified by SHA-256.

| Report | SHA-256 |
| --- | --- |
| Quant | `50b220294b9dd79cadf6c5d4b7dec1d2c503f87f90eb34d6d8b011de08831f71` |
| Turnover | `616377e04d44b78ef63d50827499693f6f77b04b1dcc3e9683f0e428d851225f` |
| Carry control | `9b7409535571fea16a778b24720ec00efdf36fa87b2c96d270314fa872a09a74` |
| Carry with haircut | `510ececcfb2fcf1aa4671bcc283e9e9a3d078e242fe8f47afa79c675ce41426a` |
| Matched settlement example | `2cc15305d2cceca3c10cca7e44a1c1adab036743099106e29fc656b5f84d3898` |
| Input and binary evidence | `4345d0104e2204d49525744116b43d4d6acd57987532df1a831e209fff662bec` |

## Research acceptance

Archive the raw file, compile profile and report, simulation input and
report, code revision, and checksums. Freeze thresholds before the holdout.
Compare modeled edge with simulated round-trip execution across liquid
and stressed sessions. Include every zero-entry run and every residual
position in the results. A fill-free coverage run establishes the quality
of its inputs and is a separate result from strategy performance.

[Perpetual Futures Pricing](https://www.nber.org/papers/w32936) ·
[Fundamentals of Perpetual Futures](https://arxiv.org/abs/2212.06888) ·
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding) ·
[HIP-3 market and oracle settings](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals) ·
[Arcus market metadata](https://docs.arcus.xyz/api-reference/public/get-markets) ·
[NYSE hours and holidays](https://www.nyse.com/markets/hours-calendars).
