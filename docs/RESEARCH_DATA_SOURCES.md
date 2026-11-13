# Research data-source registry

This is the durable index of sources used or explicitly considered by
AlephTX. It records provenance and intended use. It does not certify that
all sources are recording, contain a complete history, or support profitable
execution. Consult the sealed campaign inventory before each new study.

The [November 14 checkpoint](RESEARCH_PROGRESS_2026_11_14.md) records the
expanded Entropy/TradeXYZ/Arcus/BULK collection, fast-book supplement,
remaining channel gaps and updated immutable inventory. It distinguishes
strategy-independent capture from consolidation of older campaign files.

Public research archives and their detailed manifests stay on Oasis under
`~/aleph-tx-research`, outside Git. Private account material stays in protected
storage and out of public inventories and tool output. Commit only reviewed
source specifications, aggregate findings and synthetic fixtures.
[Recording contract](PERP_RECORDING.md) · [Shared public archive](PUBLIC_MARKET_ARCHIVE.md) ·
[Independent objectives](INDEPENDENT_STRATEGY_RESEARCH.md)

## Independent objectives and ten-hour units

Quant profit, hedge arbitrage and incentive turnover each have independent
configurations, execution state, cash ledgers, capital and loss budgets, and
reports. Turnover's `neutral`, `long_first` and `short_first` variants also
have separate experimental ledgers. Sharing a source file does not create
independent market observations or permit one strategy to subsidize another.

A validation unit is a continuous **10-hour interval**, with explicit
start/end timestamps and separately recorded warm-up and terminal drain.
State and inventory carry across storage-shard and funding boundaries.
Ten one-hour runs restarted flat are ten diagnostics, not one continuous
10-hour validation. Preserve earlier short experiments under their original
scope; do not relabel their aggregate duration.

Record source gaps, market-closed periods, actual funding for held positions
and executable final exits. Missing prices, funding or terminal evidence
cannot be replaced with zero or a reset. For RWA, retain the actual calendar
and reference-age policy; separate regular sessions cannot be concatenated
into a fictitious continuous interval. Freeze parameters and boundaries
before running. Distinguish research, selection, regression and untouched
holdout use. One passing 10-hour unit does not establish stable future profit.

## Execution-market sources

Availability below describes documented implementation and historical use.
Current process health and exact usable coverage come from the campaign
inventory; an adapter is not proof of a complete dataset.

| Stable source family | Identity and data | Documented availability | Evidence still required for an economic validation |
| --- | --- | --- | --- |
| `lighter.core` | `lighter`; Core public L2, statistics, separate public trades and subscription clock evidence | Historical crypto/RWA inputs and Extended paired capture | Market terms, account fee/delay assumptions, book/trade coverage and held-position funding |
| `lighter.robinhood` | `lighter_robinhood`; independent Robinhood Chain L2, statistics, public trades and clocks | Five-minute ingestion check documented on 2026-11-13; separate longer collection started | Domain-specific history, collateral/settlement, fees, delays, funding and API incentive eligibility; Core history cannot substitute |
| `hyperliquid.core` | `hyperliquid`; native WebSocket/REST books, contexts, public trades and polled funding history | Historical BTC/ETH and RWA campaigns; both transports supported | Native instrument, account fees, oracle methodology, transport gaps and settled funding |
| `hyperliquid.entropy` | `entropy`, native `io:SYMBOL`; HIP-3 books and contexts through Hyperliquid | Public RWA pair studies; ongoing coverage is campaign-specific | Deployer-specific terms, fees, oracle, funding, actual trade coverage and incentive rules |
| `hyperliquid.tradexyz` | `trade_xyz`, native `xyz:SYMBOL`; HIP-3 books and contexts | Public RWA pair studies | Product-specific economics, independent stock/session evidence, fees and funding; native Hyperliquid terms do not automatically apply |
| `arcus` | Public REST metadata/BBO, REST and WebSocket L2; archived trades where present | Historical BTC/ETH maker-first inputs and RWA coverage studies | Trade coverage, price/time units, oracle contexts, fees, funding and full-cycle exits |
| `extended` | Active-CLOB metadata, sequenced per-market WebSocket depth and public trades | BTC/ETH paired collection started 2026-11-13; collector transition recorded at shard 003 | Correct metadata version, book/trade gaps, account fees, settled funding and confirmed cash rebates |
| `bulk` | Public REST/WebSocket books and metadata | RWA coverage studies and read-only adapter | Tradable product, precision/time units, trades and queue evidence, fees and funding; snapshots alone cannot establish Maker fills |
| `variational` | Public v2 perpetual/swap statistics and source times; indicative marks | Read-only research adapter; executable API work deferred by the user | Executable quotes/orders and correct contract cash flows; indicative marks cannot serve as fillable L2 |

Lighter Core and Robinhood have independent liquidity domains. Native
Hyperliquid, Entropy and TradeXYZ have distinct books but shared
infrastructure risk. Retain the domain with every native market ID and
symbol. See [pair admission](CROSS_VENUE.md#pair-admission) and
[Robinhood capture](PERP_RECORDING.md#lighter-core-and-robinhood-chain).

BTC and ETH are comparable baseline assets, not a documented top-volume
ranking across every venue. A new universe selection must preserve the dated
market list and volume/depth snapshot, ranking rule, minimum-order filter
and exclusions. RWA candidates also need corporate-action and session checks.

## References and product probes

| Source family | Role | Availability and limits |
| --- | --- | --- |
| `coinbase.spot_reference` | BTC/ETH spot ticker and receipt-time cross-market reference | Used in crypto captures; reference only, excluded from execution volume and execution-venue byte targets. A ticker is not an order-level execution tape. |
| `nasdaq.equity_minutes` | Independent NVDA/TSLA and other reviewed equity minute prices | Archived-chart importer and first-observed bar schema exist. Verify symbol/date, timezone and coverage. Archived-chart publication delay is an explicit model assumption. |
| `nasdaq.sessions` | Calendar, regular/early close, halt and resumption evidence | Reviewed session inputs required; retain effective and availability times. A weekday alone does not certify a normal session. |
| `nasdaq.corporate_actions` | Dividend, split and other payoff adjustments | Required over the proposed equity window. Missing evidence does not mean no action occurred. |
| `pyth.equity_reference_attempt` | Earlier candidate independent equity source | Initial recordings documented unavailable independent equity/action coverage. Preserve the failure record; it is not a usable reference series. |
| `deribit.public_product_probe` | Public USDC contracts, native combos/books and fees for fixed-cash-flow research | `scripts/probe_deribit_box.py` provides a limited probe. A snapshot does not establish continuous execution, account margin or a positive executable Box. |

Reference availability and calendar semantics are specified in
[equity replay preparation](PERP_RECORDING.md#archived-equity-reference-and-sessions).
Keep product-probe evidence separate from the perp execution tape. Existing
adapters for other exchanges do not imply those exchanges supplied a given
research run; add a source record when their data actually enters research.

## Funding, fees and contract evidence

These need their own versioned entries even when they share a connection
with a price feed:

- **Forecasts:** observed funding estimate, native interval, next payment,
  source/publication time when supplied, and first receipt. A forecast is not
  a settled cash flow. Preserve negative and unknown values.
- **History:** published payment rate/slot, rate scale, query range, response
  availability and hash. Do not backdate downloaded history into a predictor.
- **Settlement:** reviewed mark/oracle used for payment and any reconciled
  public witness evidence. Core Lighter and native Hyperliquid proof paths
  do not automatically certify Robinhood, HIP-3 or other venues. A held leg
  crossing an unverified payment remains an accounting failure.
- **Fees and delays:** venue/product/account tier, Maker/Taker and builder
  charges, action-specific delays, effective period and archived schedule or
  endpoint version. Public assumptions and account-confirmed values differ.
- **Rebates/incentives:** confirmed cash receipts need attribution. Points,
  expected airdrops, unearned tiers and unconfirmed API eligibility retain
  zero baseline cash value. Separate eligible, target and hedge volume.
- **Contract terms:** native IDs, underlying/payoff identity, multiplier,
  quote/settlement/collateral units, tick/lot/minimums, margin, funding cadence
  and expiry/settlement rules. Preserve revisions and their effective times.

The [funding proof/audit workflow](PERP_RECORDING.md#audited-funding-cashflows)
separates forecasts, rates and witnessed unit cash flows. Private fee and
account evidence stays protected; only irreversibly redacted provenance and
aggregate findings belong in a public report.

## Archive inventory snapshot

The following public research locations were inventoried on 2026-11-13 at
15:25:58 UTC / 23:25:58 Beijing time. Directory sizes include derived work;
they must not be described as unique raw-market bytes. A running collector
or a first/last timestamp does not certify uninterrupted ten-hour coverage.

| Archive/campaign | Inventoried state | Interpretation |
| --- | --- | --- |
| `2026-11-11-us-session` | About 0.833 GiB | Historical US-session study; inspect its source manifests for usable intervals |
| `2026-11-12-20h` | About 0.016 GiB | Separate historical directory; retain provenance instead of assuming equivalence to the full campaign |
| `2026-11-12-20h-full` | About 19.275 GiB; 101 raw archives plus derived studies | Main historical corpus; derived replay count and bytes are not unique market-time coverage |
| `2026-11-12-quality-15gb` | About 0.848 GiB; 56 raw archives | Quality campaign name is a target, not proof that 15 GiB were collected |
| `extended-lighter-20261113T111337Z-cd4a1633` | 17 sealed shards; collection in progress | Under ten hours at this audit; preserve collector versions and trade gaps |
| `robinhood-12h-20261113T151725Z` | Separate 12-hour target collection just started | No completed ten-hour validation available at this audit |
| `robinhood-domain-20261113T145647Z` | Completed five-minute public capture/ingestion check | Domain compatibility evidence only |
| `robinhood-public-20261113T143824Z` | Public metadata probe | Terms discovery, not a continuous market sample |

An earlier Extended transport-check directory contains a short shard and a
stale status record. Retain it in the detailed inventory; do not infer an
active collector from the status label alone. Earlier local RWA/crypto
recordings remain discoverable through [the recording guide](PERP_RECORDING.md#first-recordings)
and their local manifests; they are not silently merged with Oasis history.

## Historical trade-stream limitation

The full 2026-11-12 campaign has twenty-hour public trade envelopes, but
its Lighter, Arcus and Hyperliquid trade-only archives contain no connection
or disconnection records. The continuous study on 2026-11-14 therefore does
not certify Maker profitability from them. Preserve these archives for
prices, trade-flow diagnostics and explicitly qualified historical work;
do not infer a connected feed from a cached trade. The new continuous
capture retains actual subscription acknowledgments and disconnect events.
See [the independent ten-hour results](TEN_HOUR_RESEARCH.md#completed-independent-diagnostic-matrix).

## Inventory fields and maintenance

Before consumption, each imported or newly sealed archive must have:

| Group | Required information |
| --- | --- |
| Identity | Stable source family, native venue/domain/market, role, public endpoint/channel, schema and collector version |
| Time | Requested/observed intervals, source-time meaning, receipt/availability clock, calibration policy/evidence |
| Integrity | Immutable hash, byte/event counts, seal/completion evidence, sequence checks, duplicates and disconnects |
| Coverage | Depth/trade/reference/funding intervals, missing ranges/reasons, session limits, metadata/fee validity |
| Lineage | Parent hashes, importer/compiler version, transformations and exact output hash; source identity survives derivation |
| Research use | Scenario, config/executable hashes, capital/fee assumptions, ten-hour window ID, warm-up/drain, selection/holdout status |
| Outcome | Complete/partial/blocked status, cash-flow reconciliation, final positions and unknown amounts; preserve failed runs |

Append these records with the archives outside Git. Do not put credential
URLs, signatures, authenticated payloads, private activity identifiers or
raw private orders/fills into the public inventory. Reconcile sealed files
and active collectors before downloading again. Recompute coverage for the
exact proposed ten-hour interval rather than copying a campaign-wide rate.
Track reused periods separately from replay-configuration counts.

Adding, replacing or retiring a source requires updating this index and
appending its manifest record. Preserve retired archives and failures so the
next agent can find them. Update the owning runbook when schema or time
assumptions change. Review this registry and inventory at each handoff.

The objective is positive net execution economics under independently
verified assumptions. Missing data, zero trades and negative runs are valid
findings. Neither theoretical plausibility nor a selected winning window is
a promise of profitable live trading.
