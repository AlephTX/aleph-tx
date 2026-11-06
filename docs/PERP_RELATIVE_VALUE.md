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

Paper fills still assume the discounted displayed liquidity is available to
this order. The replay models one pair at a time and does not simulate venue
queue priority, liquidations, collateral transfers or intraframe price paths.
The capital charge is an entry reserve; realized PnL accounts for simulated
fills, fees and settled funding. A time or session cutoff without sufficient
depth can leave positions open. Preserve those cases when comparing strategies.

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
