# Perpetual relative value and paper execution

The `relative_value` module trades a matched long and short in the same linear
perpetual exposure. It evaluates both directions against executable book
depth and keeps the strategy separate from the exchange transports. The
research runner sends no orders. The existing BBO scanner and market makers
retain their own entry points.

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

## Contracts, markets and sizing

- Configure the venue's native market, canonical underlying identity and
  reviewed payoff convention on each leg. For an equity, the identity includes
  share class and the payoff convention includes splits, dividends and
  adjustments. A ticker alone is insufficient. Each contract has a multiplier
  and a native lot; the hedge balances normalized shares or coins using their
  common lot. Quotes and cashflows must already be in the same settlement unit.
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
same configuration object without `frames`; following lines contain one frame
each. The reader streams in strictly increasing `now_ms` order. Capture source
timestamps separately from arrival time, and include every realized payment on
held legs. An optional `trade_from_ms` lets earlier frames train the estimator
without placing orders. Fix strategy parameters on an earlier period and
evaluate them on a later chronological holdout.

An IOC can consume only a newer book generated after the configured modeled
latency. The simulator walks displayed prices up to the order limit and then
passes the confirmed fill to the existing hedge state machine. The hedge can
partially fill or expire. The report separates realized PnL from the last
estimated liquidation mark, and includes fees, funding, turnover, missing
marks, peak observed drawdown, maximum single-leg exposure and time exposed.
An expired intent with no usable post-arrival book is counted separately.
Funding at a frame time is booked before simulated fills at that time.

Each `market_costs` row names the venue and contract and totals simulated
filled turnover and fees for both directions. `execution_cost_bps` divides
fees plus signed slippage from the submit-time touch by filled turnover.
The field is null when there are no fills or any fill lacks that benchmark.
Cash PnL uses fill prices, fees and settled funding, so it also captures the
spread paid at the touch.

Paper fills still assume the discounted displayed liquidity is available to
this order. The replay models one pair at a time and does not simulate venue
queue priority, liquidations, collateral transfers or intraframe price paths.
The capital charge is an entry reserve; realized PnL accounts for simulated
fills, fees and settled funding. A time or session cutoff without sufficient
depth can leave positions open. Preserve those cases when comparing strategies.

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

The next validation step is synchronized venue L2, external reference, contract
terms and funding recordings for verified matched markets. Use a chronological
holdout across liquid and stressed sessions and check expected versus realized
spread, hedge failures and unresolved exposure before any live integration.

References: [Perpetual Futures Pricing](https://www.nber.org/papers/w32936),
[Fundamentals of Perpetual Futures](https://arxiv.org/abs/2212.06888),
[Hyperliquid funding](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding),
[HIP-3 market and oracle settings](https://hyperliquid.gitbook.io/hyperliquid-docs/hyperliquid-improvement-proposals-hips/hip-3-builder-deployed-perpetuals),
[Arcus market metadata](https://docs.arcus.xyz/api-reference/public/get-markets),
[NYSE hours and holidays](https://www.nyse.com/markets/hours-calendars).
