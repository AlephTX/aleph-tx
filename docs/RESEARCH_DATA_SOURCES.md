# Research data-source registry

This is the durable index of sources used or explicitly considered by
AlephTX. It records provenance and intended use. It does not certify that
all sources are recording, contain a complete history, or support profitable
execution. Consult the sealed campaign inventory before each new study.

The [November 14 checkpoint](RESEARCH_PROGRESS_2026_11_14.md) records the
expanded Entropy/TradeXYZ/Arcus/BULK collection, fast-book supplement,
remaining channel gaps and updated immutable inventory. It distinguishes
strategy-independent capture from consolidation of older campaign files.
The main shared source has since sealed. Its new ten-hour execution results,
tail-delivery admission failures and reference-age diagnostics are recorded
in the same checkpoint. Source duration, timing admission and economic
qualification remain separate statuses.

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
| `lighter.core` | `lighter`; Core public L2, statistics, separate public trades and subscription clock evidence | Historical crypto/RWA inputs; new shared BTC/ETH source sealed November 14, with complete ten-hour quant diagnostic ledgers | Market terms, account fee/delay assumptions, book/trade coverage and held-position funding |
| `lighter.robinhood` | `lighter_robinhood`; independent Robinhood Chain L2, statistics, public trades and clocks | Shared ten-hour BTC/ETH source compiled for the priority cost curve; 750 ms timing passes but retains three disconnects per market; separate 12 h capture sealed | Domain-specific history, collateral/settlement, fees, gap exposure, funding and API incentive eligibility; Core history cannot substitute |
| `hyperliquid.core` | `hyperliquid`; native WebSocket/REST books, contexts, public trades and polled funding history | Historical BTC/ETH and RWA campaigns; shared reference-age audit and separate 90-second native fast-book timing probe on November 14 | Native instrument, account fees, oracle methodology, transport gaps and settled funding; fast-book timing is not ten-hour coverage |
| `hyperliquid.entropy` | `entropy`, native `io:SYMBOL`; HIP-3 books and contexts through Hyperliquid | Public RWA pair studies; ongoing coverage is campaign-specific | Deployer-specific terms, fees, oracle, funding, actual trade coverage and incentive rules |
| `hyperliquid.tradexyz` | `trade_xyz`, native `xyz:SYMBOL`; HIP-3 books and contexts | Public RWA pair studies | Product-specific economics, independent stock/session evidence, fees and funding; native Hyperliquid terms do not automatically apply |
| `arcus` | Public REST metadata/BBO, REST and WebSocket L2; archived trades where present | Historical BTC/ETH maker-first inputs; November 14 supplement has 336/336 hourly funding records for each of six markets | New trade-stream coverage, oracle/payment marks, effective historical terms, fees and full-cycle exits; published rates alone do not certify held-position cash flows |
| `extended` | Active-CLOB metadata, sequenced per-market WebSocket depth and public trades | Shared ten-hour source sealed; separate index capture still collecting at November 14 10:32 Beijing; 750 ms Maker timing rejected and 2 s historical sensitivity recorded independently | Correct metadata version, book/trade gaps, account fees, settled funding and confirmed cash rebates |
| `bulk` | Public REST/WebSocket books and metadata | RWA coverage studies and read-only adapter | Tradable product, precision/time units, trades and queue evidence, fees and funding; snapshots alone cannot establish Maker fills |
| `variational` | Public v2 perpetual/swap statistics and source times; indicative marks | Read-only research adapter; executable API work deferred by the user | Executable quotes/orders and correct contract cash flows; indicative marks cannot serve as fillable L2 |
| `backpack` | `backpack` per-market WebSocket depth deltas (`U`/`u` sequence), public trades without order IDs, mark/index/funding estimate; `backpack_snapshot` REST depth after every subscribe | Recorder added 2026-11-21. Three-minute Mac check (ANT.US, SPY.US, BTC perps) rebuilt books with zero gaps and zero crossed books after snapshot alignment. 104 perps, about $138M per 24 h at 2026-11-21 02:00 CST, of which ANT.US was $3.85M | Account fee tier, incentive rules, hourly settled funding, collateral and RWA session behaviour; no ten-hour coverage yet |

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
| `pendle.boros.candidate` | Fixed-versus-floating funding contracts and spread-market documentation | Considered November 14; documentation only, no API series or executable-price validation. Match underlying venue/index, settlement cadence, maturity, collateral, margin, liquidation and complete entry/exit costs before any replay. A fixed rate does not eliminate basis, execution or liquidation risk. |

Reference availability and calendar semantics are specified in
[equity replay preparation](PERP_RECORDING.md#archived-equity-reference-and-sessions).
Keep product-probe evidence separate from the perp execution tape. Existing
adapters for other exchanges do not imply those exchanges supplied a given
research run; add a source record when their data actually enters research.

## Funding, fees and contract evidence

The November 14 `public-fee-observability-20261114T134000Z` campaign adds
collector-versioned public Lighter fee integers, separately for Core and RH.
Its own metadata and 60 s smoke capture are sealed; the prospective ten-hour
measurement is scheduled, not complete. Raw/public evidence, all hashes and
append-only events stay outside Git. Its `arbitrage-funding-review` subdirectory
reuses the existing 14-day sources below, keeps download availability, and
reports all six NVDA/SNDK direction cases without changing funding multipliers.
See [current independent decisions](INDEPENDENT_STRATEGY_RESEARCH.md#current-research-decisions).

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

### November 14 funding supplement

The immutable `public-funding-review-20261114T025117Z` campaign stores
Hyperliquid `meta` for `io`/`xyz` and 14-day `fundingHistory` responses for
NVDA, SNDK and AAPL. The reviewed interval is October 31 10:00 through
November 14 10:00 Beijing, half-open. Five series have 336 hourly slots;
`io:AAPL` has 271. Current Growth Mode and delisting metadata have their
actual November 14 availability and are not backdated into earlier fees.
Published decimal rates are converted once by multiplying by 10,000;
deployer funding multipliers are not applied again to published history.

`arcus-funding-supplement-20261114T025903Z` stores `/v1/markets` and
`/v1/fundingRates?market=SYMBOL-USD&limit=1000` for BTC, SPY, QQQ, NVDA,
MU and SNDK. Each response contains 1,000 unique hourly records, including
336/336 slots in the same 14-day interval. Native market IDs and display
names match metadata. The archived official
[API specification](https://docs.arcus.xyz/api-reference/public/get-market-funding-rates)
defines `time` in epoch microseconds and `fundingRate` as an hourly decimal
rate. Conversion retains original source and download times. The
[funding mechanics](https://docs.arcus.xyz/concepts/perpetuals/funding)
also describe distinct off-hours RWA rules; current documentation alone
does not certify unchanged historical contract terms.

`public-funding-audit-20261114T030022Z` binds both captures and official
documents by hash. Its initial schema/coverage report is retained; the
separate `units-reviewed.json` records the subsequently verified units.
Each source campaign has an append-only `inventory-entry.json`, actual
request/response times and a saved capture driver. They supplement the old
raw-tape inventory rather than changing its totals. These rates support
historical screening, not account-confirmed payments or executable returns.
See the [new review results](RESEARCH_PROGRESS_2026_11_14.md#external-review-follow-up).

### November 14 RH/Core and prospective inputs

`rh-turnover-cost-curve-20261114T043334Z/prepared` binds the shared sealed
source to BTC/ETH RH/Core compiled tapes, native per-domain IDs and exact
input hashes. The original three disconnects per market and 750 ms delivery
test remain intact. Fees, funding cadence and RH action delays are explicit
research assumptions. Core USDC and RH USDG use an **unverified 1:1 research
numeraire**; conversion costs and account eligibility are not certified.
The manifest and `inventory-entry.json` preserve these limitations outside
Git. A complete paper ledger does not remove them.

`core-rh-prospective-10h-20261114T044946Z` is a public source for the fixed
November 14 13:00–23:00 Beijing (`05:00–15:00 UTC`) window. Six ETH policies
were frozen before recording. Raw Core/RH BTC/ETH books and acknowledged trades,
HL books and Coinbase references share original receipt ordering; native market
mappings come from that campaign's metadata. The capture has now naturally
completed and sealed (`manifest.json` SHA-256
`b5baf44aa18085da7b9778c457d3ec843a27df20b2b422982b59540ecd1124fe`,
`capture-inventory-entry.json` SHA-256
`c75cb1c93c9a0df2891be14a57ca6a3e887bdc765aceb4bde8b40eceb813f070`). Two
empirical source properties were discovered on holdout evaluation and preserved:
(1) Lighter Core `trade:0` (ETH) emitted one off-book zero-price bankruptcy
transfer (`trade_id: 40254475639`, `source_ms: 1794664911937`,
`at_ms: 1794664911987`, `price: "0.00"`, `size: "0.0102"`) while L2 bids at
`$1,865.79` remained untouched, causing raw unmodified ETH replay inputs to
fail closed at minute 541; BTC had zero such transfers (`210,837` trades).
(2) Robinhood Chain BTC public trades exceeded the `750 ms` delivery bound near
an hourly boundary (`funding_trade_delivery_bound_violated`), blocking strict
flat-verified BTC RH/Core replay while ETH RH/Core passed all 11 funding slots.
Both the raw fail-closed outcomes and the provenance-audited CLOB-filtered ETH
inputs (`excluded_zero_price_transfers` count = 1) are bound in
`~/aleph-tx-research/fifty-ms-strategy-20261114/manifest.json` (SHA-256
`a5d72c5a18812e4d524adfbfb5e0c8576dba894deb9f95d8e4ef3107a5b8add9`, frozen
policy SHA-256
`7a4a57fea7288b7646d9b454b49aa5fd3be27c198ff0d4491afac3e9b298fa4b`).

Phase 2 (`~/aleph-tx-research/fifty-ms-strategy-20261115-v2/`, `manifest.json`
SHA-256 `b59cc2415ab0809773bbe10038539922e67b0415b5ac23aabf440cd8be1dd708`,
`frozen_v2_policy.json` SHA-256
`a4e0aa9e8c287501a2df3ee8a8def6091e7e19e73581f712c13248b0414eb9ec`,
`inventory-entry.json` SHA-256
`2918d5c7a30bbae673417a85ff9620358b6a85e1bd9fe034a9aa53cad5ad80ff`) preserves
the original V1 directory untouched and records two additional 10-hour holdout
consumptions:
(1) `extended-lighter-index-10h-20261113T174747Z` (`W3_holdout`,
`2026-11-13 17:57:40–2026-11-14 03:57:40 UTC`, status SHA-256
`13add4cd211039660fb86d1e5197ee8e595dd2e0aefeebc6c4debebde264f3be`): all `32/32`
runs finished `incomplete = null`, `10/10` funding slots verified, and `100%`
terminal flat, while `lighter_trades` recorded `2` public WebSocket reconnects
(`trade_feed_gaps = 2`), so strict `net_liquidation_pnl` remains intentionally
uncertified (`null`).
(2) `public-venues-fast-10h-20261113T183526Z` (`80d94600...`) joined with
`2026-11-12-20h-full` HIP-3 public trades (`d2489f22...`) for `NVDA` selection
(`5b9e4a46...`) and untouched `SNDK` 10-hour MakerFirst holdout (`98c55de7...`,
summary `317d3bf3...`, `3` trade disconnects per leg preserved, `8/8` runs
`incomplete = null` and `10/10` slots verified across `0.001` vs. `0.0001` lot
asymmetry).
[Frozen study](RESEARCH_PROGRESS_2026_11_14.md#prospective-window-already-frozen) ·
[50 ms holdout results](FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md)

### November 14 public incentive and execution rules

`public-rules-review-20261114T052857Z` archives official RH points, Lighter
fee/latency and LPP documents with actual request/receipt times and hashes.
The RH points document excludes Standard-to-Standard activity from both
point types and specifies the November 6–19 weekly hedge-boost directions.
Other qualification exclusions and venue discretion still apply. Public
prints can contain optional `maker_fee` / `taker_fee` integers. Their matches
to a dated schedule are useful flow-class hypotheses, not account-tier or
reward attestations. Older sanitized tapes stripped these fields. Neither
Standard nor Premium simulated volume is labeled confirmed eligible volume.
[RH program](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points)

The archived general fee schedule supplies the Core Premium counterfactual
at zero LIT stake: 0.4/2.8 bps Maker/Taker, no additional cancel latency and
140 ms additional Taker latency. It is a current public assumption, not
historical account evidence. The supplied review's RH 300 ms claim cites our
own configured assumption; the RH API documentation probes returned identical
generic pages without domain-specific delay text, and index follow-ups failed.
RH-specific effective delays therefore remain unverified in this audit.
[General schedule](https://docs.lighter.xyz/trading/trading-fees)

LPP's official document requires Premium/Advanced status, eligible resting
quotes and asset-specific parameters. Its roughly one-minute randomized
snapshots score quoting liquidity; OI-hours alone are not that program's
published scoring model. No LPP receipts or account participation were
inferred. The archived documents do not replace receipt or settlement proof.
[LPP specification](https://docs.lighter.xyz/programs/liquidity-partner-program)

The execution follow-up reuses the already inventoried historical tapes;
it adds no independent market sample. `execution-measurement-20261114T053515Z`
freezes 84 cancellation arms and eight quant sensitivities, with the public
fees treated as counterfactual assumptions. Its append-only aggregate
inventory is `execution-review-20261114T060430Z`. The separate
`exact-cash-proof-20261114T055810Z` preserves the original reports and binds
two unchanged-parameter replays to their original configurations and raw
reports. It verifies economic/cycle parity and replaces one serialization
classification in a new review report; it does not repair feed gaps or
create holdout evidence. See the [results and remaining limits](RESEARCH_PROGRESS_2026_11_14.md#execution-and-incentive-review).
`execution-final-checkpoint-20261114T060808Z` binds the final curve,
source verification, rules and unchanged prospective policy; inventory
SHA-256 is `0a1284abc1acae274aedf3d35cb6ebd419a331e88b777f290af258f4485ab595`.

### November 14 12:57 inventory refresh

`public-inventory-20261114T045728Z/catalog.json` contains **448 unique
public archives**, **26 metadata objects** and **4,267,910,691 compressed
archive bytes**, with eight duplicate aliases. The inventory hash is
`2d5a73267a92dab8c8d2c801c4d55a0152944003835d128092c7e30ade9cc65e`.
It preserves the earlier catalogs and includes newly sealed capture files;
it excludes active partials, derived reports and sources outside its explicit
allowlist. Funding supplements and the new prospective campaign have the
separate manifests described above. Counts do not certify per-stream
continuity or represent total disk usage.

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

### November 14 basis-risk reuse

`rh-turnover-cost-curve-20261114T043334Z/prepared` was found through the
existing public campaign inventory and reused, without recapture, for
`basis-risk-selection-20261114/ablation-v2`. BTC and ETH each retain the
November 13 23:45:09.240–November 14 09:45:09.240 Beijing decision window
and 15-second terminal drain. This is historical selection, not another
independent sample. No new funding or fee observations were introduced.
The source manifest preserves public trade feed gaps, receipt-time ordering,
USDG/USDC parity uncertainty and historical latency assumptions.

The four-arm entry-only comparison has 48 separate configurations and ledgers;
policy SHA-256 is
`1c82d8e644b20312c5d5fb989324675781fe15c1380c61acee09854ea9b742e5`.
Source/config/executable hashes and outcomes are appended outside Git in
`basis-risk-selection-20261114/inventory-start.json` and its completion record.
The superseded first attempt is retained. See the
[mechanism and comparison design](archive/BASIS_RISK_SELECTION_2026_11_14.md).

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

## Binance 同机参考新增来源（2026-11-14）

- 公开 USD-M 单币 `bookTicker`：`binance/usdm/binance`，原生符号 ETHUSDT/BTCUSDT/NVDAUSDT/SNDKUSDT；四者均完成 90 秒同机探测。新的十小时主验证仅 ETH，BTC 的 Binance 覆盖仍只有本轮探测，不能当作十小时数据。
- 原始 E/T/u、精确字符串 BBO 与本机 receipt/available wall+monotonic ns、进程序列、重连 epoch 分开记录。全归档同进程身份检查；向上取整可用毫秒，25 ms 完结桶仅用于派生回放输入，原始消息不抽样。
- 公开合约元数据 `https://fapi.binance.com/fapi/v1/exchangeInfo` 于本轮探测保存，SHA-256 `a400fdd57eea6280e38a2d7dd27394c7e5093e9d85f69f998ae1eeaa0c9c91d7`。NVDA/SNDK 标为 TRADIFI_PERPETUAL，但跨平台单位/时段映射尚未通过，不进入套利收益。
- 主机外的历史 aggTrades 不具备 Oasis receipt time，不能替代此来源；本轮未下载或用于十小时验证。
- 同机目标流：RH/Core ETH 盘口与公开逐笔，Arcus/BULK BTC/ETH 盘口，Entropy/TradeXYZ NVDA/SNDK 盘口与公开逐笔。Arcus/BULK 逐笔与资金费缺口未因新增 Binance 而解决。
- 90 秒探测位于 Git 外 `~/aleph-tx-research/binance-reference-probe-20261114`；完整消息 127,842 条。时间字段全局序列、身份和归档校验通过。只有短回放链路验证，没有新十小时收益。
- [运行协议和可用性规则](BINANCE_REFERENCE_AB.md)。每轮 `capture-plan.json`、`campaign-events.ndjson`、封存 manifest、冻结 policy 和输入 hash 保留在独立 campaign，不进 Git。

## November 15 Step 1 & Step 2 Diagnostic and Multi-Venue Reuse

- **`public-fee-observability-20261114T134000Z/public.ndjson.gz.partial` (`21.41 min` diagnostic capture)**:
  Stopped cleanly by the Oasis disk-reserve guard after `21.41` minutes of live post-subscribe trade & order-book capture across `lighter` and `lighter_robinhood` (`BTC` and `ETH`, `36,043` deduplicated live trades, `$89.15M` public notional; `capture_sha256 = 84471a3bacd4050e6c3df3358403317760f3a8455c8322011f101c9fc5a4b21e`, `archive_manifest_sha256 = bddc0138675c413462270d57bd26485fd731681fb1197579c580fd54fce95684`). Used exclusively for public wire `maker_fee` / `taker_fee` tier-share and `1 s / 5 s` markout diagnostics (`scope = diagnostic_21m_public_fee_observability_not_10h_qualification`), never promoted as a 10-hour validation unit.
- **`binance-reference-ab-20261114T152000Z-v2/public.ndjson.gz.partial` (`6.63 h` total / `6.45 h` continuous interior diagnostic)**:
  Stopped by the Oasis disk-reserve guard at `6.63` hours (`26,531,201` records; `capture_sha256 = d3872fd7b224fb0c73001b173fbfbd9de5546612daa3655f9d5174def15af0ae`, `archive_manifest_sha256 = 84eef9e60e4cfd6cb47d54ff81b2e72ddbbd31cc4489598c8ca2923f1d32686c`). Evaluated over its continuous `6.45 h` interior (`1794669600000`–`1794692820000`, `6` verified hourly funding boundaries, `21,504,622` valid `ETHUSDT` BBOs, `785,699` valid `NVDAUSDT` BBOs, `2,452,224` valid `SNDKUSDT` BBOs) under `ceil_available_wall_ns_to_ms` and `25 ms` bucket coalescing (`lighter` input SHA-256 `f1ec10f2a08447e1a9fdd992afc93df62a27e8eea7429b73920eb8b8550cbc28`; `lighter_robinhood` input SHA-256 `5b0915d5cc408701ece77a9025c644b4e977d14c1a4dd6ffd0edf50110a46090`). Because the capture ended before `10.0 h` and recorded `1` Lighter public trade WebSocket reconnect (`trade_feed_gaps = 1`), `maker_replay` withholds `net_liquidation_pnl = None` across all 32 `2x2` factorial runs (`scope = diagnostic_6_43h_continuous_binance_2x2_not_10h_qualification`).
- **`public-venues-10h-20261113T183044Z/venue-archive` + `arcus-funding-supplement-20261114T025903Z` (`10.25 h` Arcus L2 & Funding Audit)**:
  Reused without recapture (`archive_manifest_sha256 = 32b9efc3bb97e49060fc0d81d94ee6a8e9c0f97dcb72a45728ab2db0d4633940`) to audit `10.25 h` L2 spreads, depths, `5 s` realized volatility, cross-venue HIP-3 basis (`NVDA-USD`, `SNDK-USD`), and `1,000` hourly funding intervals across `BTC-USD`, `MU-USD`, `NVDA-USD`, `QQQ-USD`, `SNDK-USD`, and `SPY-USD`. Because `known_missing = ["trade_stream", "funding_history"]` in the contemporaneous archive, `trade_fills_simulated = false` and `execution_qualified = false` are preserved. See [FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md](FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md#14-phase-3-step-1--step-2-execution-sensitivity-fee-dilution--multi-venue-audits-2026-11-15).

## November 15–16 Tokyo (`asia-northeast1-b`) Rolling 3-Stream `w01` Selection Captures

Recorded on `oasis-tokyo` (`20261115-6ebe647bc3e4`, `consistentNow() < 25 us` paired VDSO clock, `--precise-times` `ceil_available_wall_ns_to_ms`) across `2026-11-15 13:49:10 UTC` to `2026-11-16 00:04:10 UTC` (`36,900 s` = `10 h 15 m`; US Eastern Sunday `08:49–19:04 EST`, equities regular session closed), sealed into `tokyo_selection_dev` (`split_role = selection_dev`, never pooled with `taiwan_legacy_archive` or `tokyo_holdout_vault`), and normalized into `122,687,766` Silver rows (`119,272,410` `bbo_quotes`, `1,829,417` `public_trades`, `1,573,496` `funding_and_ctx`, `12,443` `feed_lifecycle`):

- **`tokyo-crypto-20261115T134910Z-w01.ndjson.gz`**: `3,469,026,719` bytes, `capture_sha256 = 6006aa3b0a95d2a9be38310a01a14866f2fc7f31e71599b19c7714d790563ed4`. Covers `BTC`, `ETH`, `SOL` across `lighter` (`p50 = 3.0 ms, p95 = 9.0 ms`), `lighter_robinhood` (`p50 = 3.0 ms, p95 = 10.0 ms`, `0` non-refresh BBO disconnects), `binance` (`p50 = 4.0 ms`, `0` disconnects, `max_gap = 389 ms`), `arcus` (`p50 = 86 ms`, `1` WS `EOF` at `22:01:29 UTC`), `arcus_trades` (`p50 = 9.7 ms`, `0` disconnects), `extended` (`0` disconnects), and `hyperliquid` (`p50 = 305 ms`). Non-refresh disconnects (`14` total, `100%` recovered in `~1.04–1.45 s` with causal `trade_feed: false` / book-reset barriers): `hyperliquid` BBO (`3`) and `hyperliquid_trades` (`2`) periodic ~3-hour upstream WebSocket session expiries (`StatusNormalClosure: "Expired"`), `hyperliquid_trades` (`2`) CloudFront TCP resets, `lighter` BBO (`1` at `19:24:12 UTC`) and `lighter_trades` (`1` at `17:41:01 UTC`) upstream rolling restarts (`StatusServiceRestart: "server shutting down"`), `lighter_trades` (`4`) CloudFront TCP resets, and `arcus` (`1`) `EOF`. Scheduled `-lighter-clock-refresh 4m` probes (`152` Core, `153` RH) are recorded separately as `clock_refresh` (`36–44 ms` median subscription confirm).
- **`tokyo-rwa-20261115T134910Z-w01.ndjson.gz`**: `4,241,856,197` bytes, `capture_sha256 = 90981342e87b922c1dd4d4a3420435003f4354aca2534762a46b945360838873`. Covers `18` `Arcus` RWA perps + `arcus_trades` (`0` BBO disconnects, `0` trade disconnects across `10.25 h`!), `lighter_robinhood` RWA (`0` non-refresh BBO disconnects, `1` `lighter_robinhood_trades` ping timeout at `23:43:45 UTC`), `entropy` (`io:*`) and `trade_xyz` (`xyz:*`) (`3` BBO + `3` trade upstream ~3h `StatusNormalClosure: "Expired"` reconnects), `lighter` Core RWA (`2` `StatusServiceRestart` BBO reconnects at `19:11:48` & `19:24:18 UTC`, `5` `lighter_trades` reconnects), and `binance` `BTCUSDT` (`0` disconnects). Strategy-scoped readiness certifies `arcus_single_venue` as `ready_strict_zero_disconnect` and cross-venue HIP-3 / Lighter pairs as `ready_with_causal_reconnect_barriers` (weekend US-equity off-hours session noted explicitly).
- **`tokyo-bulk-20261115T134910Z-w01.ndjson.gz`**: `2,552,508,358` bytes, `capture_sha256 = 74408307e40130e053ea956b1e3fbf6ee35bea3d63e994226dcd5982196468b1`. Physically isolated stream covering `16` `BULK` markets (`2,583,000` L2 snapshots) + `binance` `BTCUSDT`/`ETHUSDT` (`30,954,467` BBOs) with **`0` disconnects** (`ready_strict_zero_disconnect` for book/basis diagnostics; public trades and funding remain absent on `BULK` wire).

## November 16 Tokyo (`asia-northeast1-b`) Rolling 3-Stream `w02` Selection Captures (Monday US Overnight + Pre-Market)

Recorded continuously on `oasis-tokyo` (`20261115-6ebe647bc3e4`, seamless roll from `w01` at `2026-11-16 00:04:10 UTC` to `10:19:10 UTC`, `36,900 s` = `10 h 15 m`; US Eastern Sunday `19:04 EST` through Monday `05:19 EST`, covering the US Equities `24x5` overnight open at `20:00 EST` and Monday `04:00 EST` pre-market opening liquidity), independently sealed on both `oasis-tokyo` (`aleph-tx-flywheel.service`) and `aleph` (`181,032,736` total raw records across `13.80 GB`, `split_role = selection_dev`):

- **`tokyo-crypto-20261116T000410Z-w02.ndjson.gz`**: `4,427,110,106` bytes, `capture_sha256 = 5cbd3130c21b33de4c0fc9ac226b25e2a76e9d6581ed85f359f7ccc5fc70237a`, `stream_sha256 = e1c29f87f058ec50ba0486e4d6b75f63d432b91dd427df139be05c2ca35a1d99`, `62,314,962` records. Zero real disconnects on `lighter` Core BBO, `arcus` BBO, `arcus_trades` (`9,964` prints), `binance` (`55,497,217` BBOs), and `extended` (`841,054` BBOs + `10,033` trades). Non-refresh disconnects (`10` total, verified via `oasis-tokyo` `journalctl`): `hyperliquid` BBO (`3`) & `hyperliquid_trades` (`3`) ~3-hour upstream `StatusNormalClosure: "Expired"`, `lighter_trades` (`1` at `00:53:15 UTC`), `lighter_robinhood` (`1`) & `lighter_robinhood_trades` (`1` at `00:55:45 UTC`), and `coinbase` (`1` EOF at `10:15:04 UTC` inside terminal post-10h margin).
- **`tokyo-rwa-20261116T000410Z-w02.ndjson.gz`**: `5,809,092,465` bytes, `capture_sha256 = 65ef61eab1b988142538e3d7850ba39c0066039d2a987040ff8f9ea5290c574d`, `stream_sha256 = 730ea2332b3483869eda6cb683dea209b382c0b09eedf19762ffa06e6282e0e0`, `66,615,822` records. Captures active Monday overnight + pre-market equity perp trading (`76,437` `arcus_trades`, `212,220` `lighter_robinhood_trades`, `234,741` `trade_xyz` trades, `28,209` `entropy` trades, `116,669` `lighter` Core RWA trades). Zero real disconnects on `lighter` Core BBO, `lighter_robinhood` BBO, and `binance`; non-refresh disconnects (`10` total): `hyperliquid` BBO (`3`) & `hyperliquid_trades` (`3`) ~3-hour `StatusNormalClosure: "Expired"`, `lighter_trades` (`1` at `00:50:00 UTC`), `lighter_robinhood_trades` (`1`), `arcus_trades` (`1` EOF at `01:26:53 UTC`), and `arcus` BBO (`1` EOF at `10:15:04 UTC` after the 10h interior window).
- **`tokyo-bulk-20261116T000410Z-w02.ndjson.gz`**: `3,562,442,460` bytes, `capture_sha256 = 0e0fe87c39a655ab6ed246b3b832753150f1ab80c46a6e42e9b0dfa9aca81efc`, `stream_sha256 = a1208325ca52c605614d24605e4386f679a26ee7d9e325bd8bef9c48e97fe86b`, `52,101,952` records (`2,583,000` `BULK` L2 snapshots + `49,518,951` `binance` BBOs) with **`0` disconnects** (`ready_strict_zero_disconnect`).

## Market-making methodology reference (2026-11-17)

- Source: user-supplied local copy of [Vertox's market-making guide](https://www.vertoxquant.com/p/market-making-guide), domain `vertoxquant.com`; SHA-256 `d7079d1ef93fb375e13cc5f619b8a202ad28590525b8586744071f0a308d6aa6`.
- Classification: methodology literature, not a market-data campaign. Market, event-time range, funding coverage and executable fee coverage are **not applicable**; no new trading observation or 10-hour coverage is claimed. Fee examples in the article do not establish current venue/account terms.
- Transformation: extract the public article body and MathML/code from saved HTML without executing scripts; inspect the ten substantive figures. The saved page includes linked article cards whose full texts were not supplied. Preserve the saved-source hash and public-body extraction outside Git; do not archive browser/session metadata in the repository.
- Detailed synthesis, code mapping, primary-source checks and proposed experiments: [Vertox research and optimization plan](VERTOX_MARKET_MAKING_RESEARCH_2026_11_17.md). Supporting references and their read-access limits are listed in its §15.2; full source bytes were not archived for those online references, so their content hashes remain unknown.
- Local append-only literature record: `.agent-state/research/methodology_inventory.ndjson`; source manifest: `.agent-state/research/vertox-source-manifest-20261117.json`. This literature record supplements the campaign inventory and does not replace any sealed market-data manifest. Future experiments must register their actual consumed markets, windows, fees/funding, gaps and transformations separately.

## Three-source review and RWA rule references (2026-11-17)

- Literature: user-provided [Gemini R1](https://share.gemini.google/ylp0SvG6gPYa) and [Gemini R2](https://share.gemini.google/Kj4JQTZDZ874), domains `share.gemini.google` / `gemini.google.com`, reviewed alongside the Vertox source above. Read the rendered report bodies, tables and formulas; embedded Preview chart data and unpublished simulation implementations remain unverified. These are research hypotheses, not executable market or performance evidence. No full browser/session export is retained; report content hashes remain unknown.
- Public rule references: [NYSE calendar](https://www.nyse.com/trade/hours-calendars), [TradeXYZ perpetual specifications](https://docs.trade.xyz/perpetuals/specifications-and-schedules/specification-index/), [product definition](https://docs.trade.xyz/perpetuals/), [oracle](https://docs.trade.xyz/perpetuals/mechanics/oracle-price/), [external price](https://docs.trade.xyz/perpetuals/mechanics/external-price/), [Pre-IPO terms](https://docs.trade.xyz/legal-and-disclaimers/perpetuals-risks-and-disclaimers/pre-ipo-perpetual-markets-risks-and-disclaimers/), [Entropy oracle](https://docs.entropy.io/equity-perp-mechanics/oracle-price), and [Lighter funding](https://docs.lighter.xyz/trading/funding). Domains: `nyse.com`, `docs.trade.xyz`, `docs.entropy.io`, `docs.lighter.xyz`. These dated documentation observations supplement existing venue entries; they do not establish any historical contract version, real-time reference availability, account fee entitlement, or settled funding series.
- Transformation: manual source-to-code coverage audit and rule/assumption classification; no market observations synthesized or rewritten. Source event-time ranges and trading coverage are **not applicable**. Raw online content hashes remain unknown; future campaigns must archive the exact applicable terms, available/effective times and hashes, and register actual markets, gaps, transformations and fees/funding separately.
- Full coverage map, RWA code-path distinction and seven implementation contracts: [integrated research §15–16](DEEP_OPTIMIZATION_RESEARCH_2026_11_17.md). Local audit manifest/checks: `.agent-state/research/three-source-rwa-audit-20261117.json` and `.agent-state/research/three-source-rwa-checks-20261117.json`; new literature/rule consumption records append to `.agent-state/research/methodology_inventory.ndjson`, preserving the prior record. No sealed capture or campaign result is replaced by this review.

## November 16–17 Official Arcus Fee-Tier Schedule & Rolling 3-Stream `w03` Captures (Monday Regular US Cash Session)

- **Official `Arcus` Public Fee-Tier & Per-Market Schedule Snapshots (`2026-11-17`)**:
  Archived outside Git under `/mnt/c/AlephTX-Lakehouse/30_external_evidence/fee_and_incentive_rules/` from live public endpoints `https://api.arcus.xyz/v1/feetiers` (`sha256 = 0d608aa1eb377b41bb0caaf6e73c4b3772afbab599d4f8d876721d2446c30e0d`) and `https://api.arcus.xyz/v1/markets` (`sha256 = 5f5a5f0045c2aa80f2b43888e68e879dd41299efad531b0b942806b8d760e1ed`). Confirms Level 0 (`Base`, `$0` 30d volume) charges `0.00 bps` Maker / `2.25 bps` Taker on all RWA perpetuals (`SPY-USD`, `QQQ-USD`, `NVDA-USD`, `AMZN-USD`, `MU-USD`, `SNDK-USD`, `AAPL-USD`, `TSLA-USD`, ...) and `1.50 bps` Maker / `4.50 bps` Taker on Crypto perpetuals (`BTC-USD`, `ETH-USD`, `SOL-USD`), with shared wallet-wide 30-day volume tiers reaching `0.00 / 2.30 bps` on Crypto at Level 4 (`Platinum`). Account-specific tier entitlements remain unassumed until verified per account.
- **Tokyo `w03` (`2026-11-16 10:19:10 UTC` to `20:34:10 UTC`, `36,900 s` = `10 h 15 m`; US Eastern Monday `05:19–15:34 EST`, covering the full regular NYSE/NASDAQ cash open `09:30–15:34 EST`)**:
  Classified strictly as `split_role = historical_validation` because parameter freezing occurred after `w03` collection had started (whereas `w04`, `20261116T203410Z-w04`, was pre-registered `181.7 min` prior to start under `campaigns/frozen_tokyo_w04_prospective_oos_20261117.json`, `sha256 = 756fd62bec811c1add6cbb8e78f1e43a639cd9ee08c7b36a175fccc11de875bc`). Independently sealed on both `oasis-tokyo` and `aleph` (`15,480,890,218` compressed bytes = `15.48 GB`), normalized into `211,088,057` Silver rows (`206,940,712` `bbo_quotes`, `2,098,190` `public_trades`, `2,036,633` `funding_and_ctx`, `12,522` `feed_lifecycle`), with parent `manifest_sha256` bound in `readiness_{bulk,crypto,rwa}_w03.json`:
  - **`tokyo-crypto-20261116T101910Z-w03.ndjson.gz`**: `4,937,756,549` bytes, `capture_sha256 = 85b6b667d9eda2657c7b36775adf046b1fa48816023c3388663ceafc55f81b9f`, `stream_sha256 = 663e02e6119d87bfee674bfdf013ed53e8cbd94f9675ad98ac79e997183744b7`, `manifest_sha256 = 48cda64aa6b08fe786564708dd5ca10e39ffb55d12bf5fd17471d28233cab6a0`. Zero disconnects on `lighter` Core BBO (`2,111,191` messages) and `binance` (`65,402,770` messages, capturing the Monday `BTC` dip to `$63,871.05` / `-36.67 bps` worst 1m return); records non-refresh reconnect barriers on `arcus` (`3`), `arcus_trades` (`3`, plus US-open burst delivery lag exceeding `3,000 ms` `trade_delivery_grace_ms` on `BTC`), `lighter_robinhood` (`2`), `lighter_robinhood_trades` (`2`), `lighter_trades` (`7`), `hyperliquid` (`3`), `hyperliquid_trades` (`3`), `extended` (`1`), and `coinbase` (`2`).
  - **`tokyo-rwa-20261116T101910Z-w03.ndjson.gz`**: `6,493,455,922` bytes, `capture_sha256 = 12b7760863ea12af676636db319257ac9f9f19373b863fcb4cac0f128cff1e89`, `stream_sha256 = 222bd4ebcd284895acef0bdd943b1a3f51fa416003b9a77ec077765690b4b969`, `manifest_sha256 = 4f5790efca86b59270b5f88f164368195b676a8cf1177a1b63f290aadfe130f9`. Covers peak Monday US regular cash-session RWA volume (`xyz:XYZ100 $344.54M`, `xyz:NVDA $338.13M`, `xyz:SP500 $227.54M`, `xyz:SNDK $103.39M`, `lighter_robinhood SPY $66.78M`, `110,118` `arcus_trades`, `207,258` `lighter_robinhood_trades`, `291,379` `hyperliquid_trades`). Zero disconnects on `binance` (`58,103,815` BBOs); preserves reconnect and US-open `arcus_trades` delivery-grace barriers (`funding_trade_delivery_bound_violated` / `trade_delivery_grace_exceeded` withheld rather than synthesized into cash PnL).
  - **`tokyo-bulk-20261116T101910Z-w03.ndjson.gz`**: `4,049,677,747` bytes, `capture_sha256 = ac2a1eabc21786a5fa187bf74236ba1a0eb90734f7d285ac0042a9120138c59c`, `stream_sha256 = ddb8c2cf1831024eed763c50a663429fe114215126180c60d43b925bb47bc510`, `manifest_sha256 = be50b5a053b4aea7a8dac7e7bfcb2d8589d5c08c617b6c48e2d2aaa8f09be248` (`2,582,923` `BULK` L2 snapshots + `58,103,589` `binance` BBOs; `1` reconnect each).

## November 16–17 Tokyo (`asia-northeast1-b`) Quarantined `w04` Captures (`10 h 10 m 06 s`, Pre-Registered `v1` Prospective OOS Window)

- **Tokyo `w04` (`2026-11-16 20:34:10 UTC` to `2026-11-17 06:44:17 UTC`, `36,606.19–36,606.72 s` = `10 h 10 m 06 s`; US Eastern Monday `15:34 EST` regular-cash close through overnight to Tuesday `01:44 EST`)**:
  - **Quarantine Provenance & Integrity Audit**: At `2026-11-17 06:44:17 UTC` (`4 m 53 s` short of the `36,900 s` rolling timer, and **`216.69 s` AFTER** the pre-registered `10 h 00 m 00 s` decision interval plus `30 s` RWA / `15 s` crypto terminal drain completed), Ubuntu `apt-daily-upgrade.service` (`needrestart`) sent `SIGTERM` to `aleph-perp-capture-{bulk,crypto,rwa}.service`. Go `perp-capture` flushed and closed all three gzip files cleanly by `06:44:17.467 UTC` (`gzip_eof_clean = true`, `dropped_uncompressed_tail_bytes = 0`), `perp_rolling_runner.py` moved them into `/var/lib/aleph-tx-captures/quarantine/`, and `systemd` `TimeoutStopSec=30` interrupted Python SHA-256 manifest hashing before seamlessly starting `w05` (`20261117T064447Z-w05`) at `06:44:47 UTC`.
  - **Storage Isolation**: Stored outside `tokyo_selection_dev` in `/mnt/e/AlephTX-Lakehouse/10_bronze_raw/quarantine_partials/` (and Google Drive `10_bronze_raw/quarantine_partials/`) with `split_role = legacy_diagnostic` + `.quarantine_manifest.json` (`11,422,483,158` compressed bytes = `11.42 GB`, `141,198,601` raw JSON lines), and normalized into `141,797,911` Silver rows (`138,545,227` `bbo_quotes`, `1,500,815` `public_trades`, `1,739,438` `funding_and_ctx`, `12,431` `feed_lifecycle`) with parent `manifest_sha256` bound in `readiness_{bulk,crypto,rwa}_w04.json`:
  - **`tokyo-crypto-20261116T203410Z-w04.ndjson.gz.partial`**: `3,518,706,494` bytes (`48,216,355` intact lines, `36,606.186 s`, `max_overall_gap_ms = 120 ms`), `capture_sha256 = f6125617d5641ce227724766a6f4cad8bc175e61c1e787334703c9c2be8e814b`, `stream_sha256 = edd7a84c5f88f7bf50dfc488e66da8954e00e03bbcd0882af68d9a54218b2047`, `manifest_sha256 = 2bb6218ed90a2c83ffd272f6a7167e149880f9a6f0161243fa4a5d78b6345d1d`. Zero disconnects on `lighter` Core BBO and `arcus` BBO; records `arcus_trades` (`1`), `lighter_trades` (`1`), `lighter_robinhood` (`1`), `lighter_robinhood_trades` (`2`), `hyperliquid` (`4`), `hyperliquid_trades` (`5`), `extended_trades` (`1`), `extended_index` (`1`), and `coinbase` (`1`) causal reconnect barriers.
  - **`tokyo-rwa-20261116T203410Z-w04.ndjson.gz.partial`**: `4,899,733,855` bytes (`52,773,320` intact lines, `36,606.691 s`, `max_overall_gap_ms = 123 ms`), `capture_sha256 = c9ea3eec8b78385d669b76a770f2dbfd58f6653f5cf8ea859b05934a292a6ee9`, `stream_sha256 = ab0d569a59dbe32e920a7576809949fd87f706f05521ef07756d4fb035aace9e`, `manifest_sha256 = efdffd433e82faa370e90973de875d2a7824b3e9554641f7918156be2d161b19`. Certifies `arcus_single_venue` (`arcus` + `arcus_trades` + `binance`) as **`ready_strict_zero_disconnect`** (`0` disconnects across `10.17 h`), while preserving causal reconnect barriers on `lighter_robinhood_trades` (`1`), `lighter_trades` (`1`), `hyperliquid` (`4`), `hyperliquid_trades` (`4`), and `coinbase` (`1`).
  - **`tokyo-bulk-20261116T203410Z-w04.ndjson.gz.partial`**: `3,004,042,809` bytes (`40,208,926` intact lines, `36,606.724 s`, `max_overall_gap_ms = 205 ms`), `capture_sha256 = 92fcae779956d8a1ad65ab2efa12af7a84651f4585aec85f32c93e9fb5575810`, `stream_sha256 = c8973700faa5d376f27a3df7412e6abc47e73609a3b786f69934e2cd6f7013cc`, `manifest_sha256 = 802f01c0d13e9d1d2a559926e400a152f790b0913dbe75d16f3c6088416766ce` (`1` `bulk` reconnect, `0` `binance` disconnects).

## HIP-3 fee admission correction — November 18, 2026

The capability snapshot `2026-11-18.v1` removes generic audited fee rows for
Entropy and TradeXYZ. Public market discovery stays available. Native perps'
base `1.5/4.5 bps` is insufficient evidence for a HIP-3 market's all-in cost:
the [official fee formula](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees)
depends on deployer fee scale, growth mode, collateral and account terms.
The [deployer action specification](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/hip-3-deployer-actions)
also makes fee settings asset-specific. The
[perpetual metadata endpoint](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals)
exposes growth-mode metadata; a current response alone cannot establish its
historical effective interval.

Before restoring audited capability rows, bind native market identity,
effective-time rules, deployer scale/growth mode, collateral and evidenced
account tier/discounts. Unknown values remain unadmitted. Preserve W04/W05
frozen binaries and configurations; their configured fee scenarios are not
retroactively account-verified by this correction. Capture source snapshots
and hashes in the append-only campaign inventory outside Git.

## Tokyo W05 completed capture and frozen evaluation — November 18, 2026

Public capture `20261117T064447Z-w05` covers approximately November 17,
06:44:48–16:59:47 UTC. All three gzip streams were sealed, checked for clean
EOF, transferred to the existing Aleph E-drive Bronze vault, and backed up
as whole files to the existing Drive lake. Full Drive download/read-back
SHA-256 verification completed for every stream; receipts match the Bronze
manifests on both Aleph and Tokyo. No private account records were used.

| Stream | Compressed bytes | Capture SHA-256 |
| --- | ---: | --- |
| bulk | 4,052,590,355 | `b2178d8e117f472beeaa8168c26fdf4dedcfe41ebe509220da35a7a12fd4a359` |
| crypto | 4,989,091,368 | `09ee73ecaeb34c4bc5295b7ba466528dbf79df8f403c83c4920fcb37d822fdc0` |
| rwa | 6,534,729,227 | `39d2b536eb7d44b64c568c4f70de35c242a3136a695fa54ae154c77372d836d6` |

The total is 15,576,410,950 bytes. Exact domains, markets, source/receipt
clocks, availability rules and per-source gaps remain in the immutable
Bronze manifests, native archive manifests, Silver manifests and
`readiness_{bulk,crypto,rwa}_w05.json`. Normalization used precise times;
causal replay preparation retained source lifecycle and reconnect barriers.
All three captures have approximately 10.25h physical coverage but fail the
strict gap-free continuity flag because source disconnects occurred.

The existing pipeline completed Silver/DuckDB/Parquet, GPU diagnostics,
290 exploratory ledgers, the pre-registered v2 51-arm evaluation, the v1
26-arm control and five separately reset-window stress reports. Seven
frozen manifest/binary hashes stayed unchanged. Parameter/configuration,
fee-term and actual tape hashes were independently checked. Exploration is
separate from the frozen evaluation; these inspected data cannot be reused
as an untouched holdout for newly selected parameters.

For v2, 18 arms were cash-reconciled, 23 withheld results for gaps and 10
remained unsettled; v1 counts were 8, 13 and 5. Four pair-hedge arms in each
suite include the template's extra one-minute warmup in their entry
interval (10h01m), so they are not certified exact-10h decision windows.
Five reset windows are not a continuous 50h equity path. Retain unknown
incentive eligibility and asset-specific HIP-3 costs; all tracks remain HOLD.

Artifacts are under `gold/tokyo_w05_20261118/` and the existing per-track
Gold directories. The append-only campaign inventory includes ingest,
source provenance, complete pipeline output hashes, full-backup verification
and versioned independent reviews. Detailed local-only interpretation is
in `.agent-state/research/W05_TAKEOVER_RESULTS_2026_11_18.md`; the latest
independent review is
`codex_w05_independent_review_20261117T204529931365Z.json`.

### November 18 Taiwan replication, Tokyo transfers and Binance monthly archives

Aleph campaign `gold/strategy_iteration_20261118/` binds engine, policies,
tape and per-arm outputs to append-only campaign inventory events. Taiwan
Core BTC used the November 13–14 legacy continuous and November 14 Core/RH
Bronze captures. The archived 100 ms book compilation reproduced both BTC
maker-input SHA-256 values, all source trade acknowledgements, and both
published 5,000 USD quote-size net025 results. Preserve the separate 250 ms
book-sampling sensitivity. The 500 USD quote-size candidate traded less in
both original windows and had smaller modeled drawdown. Confirmed eligible
turnover remains unknown.

Tokyo W01–W05 are historical for the new hypotheses. The first matrix has
130 arms, with exact ten-hour decision intervals, independent capital,
pre-window warmup and terminal drain. Another 40 arms transfer the
Taiwan Core/RH crypto hedge to Tokyo. Individual trade-delivery failures,
funding slot counts, reconnects, equity session metadata gaps and zero-fill
units remain in the inventory. Historical comparison windows have no new
holdout designation.

The official Binance public monthly series cover January 2023 through
December 2025. BTCUSDT spot klines and USD-M BTCUSDT/ETHUSDT perpetual
klines, mark-price klines and funding settlements comprise 252 distinct
archives with official per-file SHA-256. Use the per-archive inventory for
URLs, digests and date coverage. Native spot timestamps after January 2025
use microseconds. Hourly executable-price proxies, delayed signals, mark-open
funding estimates, per-fill cost sensitivities and missing observations are
explicit in each protocol. These data support historical price/cost screens.
Depth, simultaneous fills, account fees and venue incentive eligibility
remain unverified. [Binance public data](https://github.com/binance/binance-public-data)

The frozen ten-unit W07–W16 protocol excludes already-started W06. Each
window must retain raw-source verification, its scheduled position and
actual decision start. Local research detail and source references are in
`.agent-state/research/STRATEGY_VALIDATION_2026_11_18.md`.

### November 18 scope of Core Standard turnover

The frozen BTC Mode B 500 USD quote-size strategy starts flat, trades only
Lighter Core, and assumes a Standard account. Its two Taiwan reference
windows generated 42,012.41 and 54,158.45 USD of simulated Core turnover.
Both have exactly zero direct Robinhood Chain program volume and no RH
position to boost. Standard accounts are outside the published Liquidity
Partner Program's account scope. That program measures eligible resting
orders at snapshots. Venue-awarded points remain unobserved in these
replays. Cost per million eligible volume is undefined at a zero
denominator. [RH points](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points.md) · [Liquidity Partner Program](https://docs.lighter.xyz/programs/liquidity-partner-program.md)

Dated official rules and their SHA-256 inventory are kept outside Git under
the forward campaign's `rules/`. The separate program attribution preserves
frozen cash ledgers and distinguishes an excluded activity, an upper bound
on eligible RH activity, and actual venue awards.

### Zero-reward market making and independent monthly receipts

The third research ledger values reward income at zero. Count simulated
passive and aggressive turnover separately. Divide terminal, fee-adjusted
cash cost by gross turnover and separately by passive turnover. A zero
denominator leaves the respective unit cost undefined. For Taiwan BTC and
ETH, both original 100 ms input tapes per asset reproduce the archived
SHA-256. The 180 historical variants, including incomplete runs, and their
cost sensitivities are recorded in the campaign inventory under
`gold/forward_ops_20261118/zero_reward_hist_v1/`.

Tardis provides native Lighter Core and Robinhood Chain messages and recorder
receipt timestamps. Empty lines mark recorded disconnects. The campaign
freezes two independent ten-hour units per UTC month-first date from May
through November 2026. Core is in scope on seven dates. Core/RH overlap on
the October and November dates. Dated slices and hashes live outside Git in
`public_tardis_native_20261118_v1/`. Report the actual venue, coverage,
clock checks and gaps per unit. Lighter's public daily trade files provide
exchange-level trade identifiers and exchange times for independent coverage
checks. A final ledger cannot supply an earlier message receipt time.
[Tardis native historical feeds](https://docs.tardis.dev/api/http-api-reference) ·
[Lighter historical data](https://docs.lighter.xyz/developers/integration-guides/historical-data.md)

The November 18 monthly extraction verified 1,251 ten-minute response slices
and nine official venue-date Parquet files. Core has 28 candidate market
units across seven dates. Eight units on four dates reconcile all public
regular trades by ID, economic fields, side and source time and pass native
book/receipt checks. Preserve the 3,920 officially recorded trades absent
from the other units and retain all affected replay outputs. The 196 fixed
single-venue runs and 32 paired runs have separate output receipts.
`gold/forward_ops_20261118/local_pressure_v1/` contains the 168 matched
historical comparisons of top-of-book imbalance and a two-clip inventory
limit. The 56 unchanged-policy control reports reproduce their original
outputs byte for byte. These same-date comparisons reuse the monthly sample.

For the matched latency study, retain receiver-side and exchange clocks
separately. Taiwan Core public trade age medians span 62–87 ms across four
market units and Tokyo spans 31–43 ms across ten. The Tokyo public
WebSocket control-frame RTT sample has 32 observations per domain and
does not provide private execution timing. The same native monthly stream
was also replayed with its actually observed between-grid book updates.
Removing those updates reproduces the original 100 ms inputs exactly.
The latency and cadence studies contain 592 and 128 runs. Their independent
cash results include unavailable windows, stressed outcomes and a
full-volume maker markout and IOC cost reconciliation under
`gold/forward_ops_20261118/latency_matched_v1/` and `native_cadence_v1/`.
[Lighter fees and venue delays](https://docs.lighter.xyz/trading/trading-fees.md)

The November 18 reference study added 556 archived ten-minute Hyperliquid
L2 samples for the four Core dates that passed official trade reconciliation.
The tapes carry the arrival time and venue timestamp separately. The records
under `gold/forward_ops_20261118/cross_reference_v1/` include source
hashes, disconnects, count of frames with a fresh external observation,
and all 80 fixed execution comparisons.
[Hyperliquid historical coverage and channel definitions](https://docs.tardis.dev/historical-data-details/hyperliquid)

The Core imbalance study under `fair_value_shadow_v1/` trained on complete
May and July units. September and November were reserved for the subsequent
time comparison of this estimator. All four dates had previously been used
for other research. `fair_value_execution_v1/` records the fitted table,
input and engine digests and 72 simulations including 16 unchanged control
outputs. `two_ioc_anchored_v1/` reconstructs historical Tokyo W01–W05
from verified raw inputs with Coinbase USD anchors and sufficient per-leg
paper collateral. Its 80 outcomes include every zero-entry run.
[Order book signals and adverse selection](https://ora.ox.ac.uk/objects/uuid%3A006addde-3a03-4d75-89c1-04b59026e1c0)

`carry_rate_screen_v2/` contains the two domains' public market metadata,
81 date-range responses for hourly rates and their separate source and
canonical hashes. Four markets overlapped from 2026-09-24 through 2026-11-17.
DOGE was absent in the RH metadata snapshot. Each ten-hour unit records the
settlement slots, a two- or four-hour publication lag assumption and the
cost sensitivities. Rate history supplies no execution-price witness.
`carry_rate_screen_v1/` retains the earlier incomplete retrieval.
[Official public funding endpoint](https://docs.lighter.xyz/api/funding/fundings.md)
[Hourly payment conventions](https://docs.lighter.xyz/trading/funding.md)

### Two-domain five-minute prices for funding basis and cross-sectional tests

The Core and RH public candles in `basis_hours_v2/source/` cover the
September–November 2026 BTC, ETH, SOL and XRP funding study and its price
warm-up. The inventory pins accepted daily responses from 456 requests and
the canonical hourly payment inputs. Empty or zero-volume bars and orders
below size or volume constraints remain visible in the per-arm output.
Prices use the open of the next five-minute bar after the decision, with a
separate adverse high/low sensitivity. The source supplies traded prices and
aggregate volume. Executable bid/ask depth and receipt times are unknown.
Version one preserves the incomplete two-day request schedule.
[Official candle endpoint](https://docs.lighter.xyz/api/candlesticks/candles.md)
[Public funding endpoint](https://docs.lighter.xyz/api/funding/fundings.md)

### Additional native market coverage and disjoint ten-hour windows

The November 18 campaign's `alt_asset_transfer_v1/` pins the Core public
market registry, seven first-of-month dates from May through November 2026,
and SOL, XRP and DOGE native books and trades in a separate Bronze namespace.
Each raw ten-minute slice has its own digest. Each market and period links
the reconstructed stream to the exchange's final regular-trade archive by
ID, quantity, price, aggressor and source time. The frozen temporal splits,
lot sizes, transformed inputs, errors and cash results are indexed per arm.
The metadata snapshot supplies instrument rules as of retrieval.

`quality_windows_v1/` records reasons for excluding each affected minute
and eligible five-minute-aligned starts. It reserves previously admissible
intervals, then selects disjoint ten-hour units by coverage alone, with a
one-minute warm-up and fifteen-second drain. BTC and ETH can share the
same wall-clock interval. `quality_windows_execution_v1/` links the
accepted BTC and ETH intervals to their unchanged strategy configuration
apart from the four absolute time bounds. Rebuilt overlapping reference
tapes are checked byte for byte against their prior native versions.

The same coverage scan adds one September SOL interval and one XRP interval.
`alt_additional_execution_v1/` records their native tapes, parent hashes
and all 14 outcomes. `alt_asset_lot_repair_v2/` keeps the 84 separately
retried XRP/DOGE quant configurations, with the base-unit fallback set to one
exchange lot and the original USD risk amounts. The original fixed windows
and their reasons for withholding cash evidence remain in the inventory.

### W06 Arcus public trade identity checks

For conflicting W06 public trade messages, the official market-qualified
single-trade endpoint provides an independent view of trade identity,
price, size, side, sequence and timestamp. Preserve the original Bronze
payload and the retrieval time and hash of the later response. The REST
lookup is retrospective evidence. The isolated W06 scope audit records
which fields agreed and which source timestamps differed, without using
later observations to change historical receipt times. Research with
withheld markets uses a separate partial Silver projection.
[Arcus single public trade](https://docs.arcus.xyz/api-reference/public/get-a-single-trade-by-id.md)

### Sprint 3 historical execution comparisons

`sprint3_historical_v1/` and `sprint3_months_v1/` under
`gold/forward_ops_20261118/` reuse the hash-verified Core native tapes and
the Tokyo Core/RH pair tapes above. Their protocols pin the strategy, market,
decision period, source hashes and configuration for each run. The two Core
panels contain six and eight market windows with disjoint decision times
for each instrument. Each comparison uses zero rewards, the existing venue
fees and an additional 0.25 bp on gross turnover, with a further 1 bp on
taker turnover for stress. The Tokyo W03 ETH pair lacks a funding-time
witness required for settlement. Historical price, queue and receipt times
retain their original definitions throughout. The append-only campaign
inventory links each protocol, score set and one-second fill attribution.

### Four-market Core native execution inputs

`sprint4_input_coverage_v2/` intersects the separately audited BTC, ETH,
SOL and XRP minute histories on seven archived dates. It uses source
continuity, observed trade subscriptions, official trade reconciliation and
book freshness across the warm-up, ten decision hours and drain. The
original funding and momentum screen stores its schedule in epoch seconds.
Version two converts those decisions to milliseconds before comparing
them to native recorder times. Version one remains in the inventory.

The earliest independent four-market interval runs from 07:15 to 17:15 UTC
on September 1, 2026. `sprint4_joint_native_tapes_v1/` pins all four tape
hashes. `sprint4_joint_features_v1/` pins eight public Core candle and
funding responses. The two completed five-minute bars used for each ranked
return are 24 hours apart. Historical funding observations enter cash
accounting at their hourly payment slots. A day-old native BBO anchor is
required to validate the Rust engine's midpoint ranking. The candle-ranked
execution diagnostic uses the earlier bar-based signal definition.
Its protected IOC planner rounds buy limits down and sell limits up to a
valid tick inside the slippage limit. The execution model checks the
visible depth at simulated arrival before crediting any volume.
[Core candle endpoint](https://docs.lighter.xyz/api/candlesticks/candles.md) ·
[Core funding endpoint](https://docs.lighter.xyz/api/funding/fundings.md)

### Sprint 5 historical cash, inventory crossings and fixed-clock rank transfer

The aleph campaign under `gold/forward_ops_20261118/` records the following
source relationships in its append-only inventory.

- `sprint5_cash_v1` reuses the 14 hashed, continuous Core market windows
  and venue-specific costs from `sprint3_historical_v1` and `sprint3_months_v1`.
  The protocol pins the Linux engine source, each profile and each tape.
  Each trial assumes zero rewards, charges 0.25 bp of all turnover and
  reports the effect of another 1 bp on taker turnover. The component
  receipt records one-second maker markout coverage and IOC prices against
  the submission midpoint per unit.
- `sprint5_inventory_audit_v1` adds two counters to an isolated copy of
  the paper simulator. A crossing requires a maker fill taking the signed
  venue ledger directly from long to short or short to long. The amount is
  only the value opened on the opposite side. Each run compares every
  earlier report field to its original value before publishing the counters.
- `sprint5_arb_bridge_v1` reuses the 30 Tokyo Core/RH W01–W05 entry-threshold
  reports, preserves funding exclusions and reconciles signed execution
  costs by phase, price drift, fees and overhead to each funded, flat ledger.
- `sprint5_rank_transfer_v1` covers 01:00 and 13:00 UTC on each of the seven
  archived first-of-month dates, May through November 2026. Native depth
  and trade continuity come from the two `quality_windows_v1` audits.
  Each candidate uses four public completed five-minute Core candle returns,
  if the date has enough native tapes. Both selected legs need confirmed
  books and all hourly funding slots. The two-leg executions reuse the
  Sprint 4 protected IOC planner and costs. The receipt retains unavailable
  windows, per-instrument sources, and SHA-256 references. The September 1
  morning unit overlaps the earlier 07:15 UTC sample by 3 h 45 min.

### Four-market price history and Sprint 6 execution studies

- `gold/forward_ops_20261118/early_rank_2026_v1` preserves 556 public
  Lighter Core candle-day and funding-week responses for BTC, ETH, SOL
  and XRP. The calls use `candles` and `fundings` on
  `mainnet.zklighter.elliot.ai`. The manifest includes the interval,
  resolution, market, retrieval time, response and artifact hashes,
  historical metadata assumptions and the original failed retrievals.
  Five-minute trade candles and hourly per-base funding cover the
  January–April 2026 experiment, including pre-window data. Exclude
  nonpositive-volume and invalid bars. Do not interpolate missing anchors,
  holding-period prices, return history or funding. The fixed-size
  supplement removes the volatility lookback it does not consume.
- `sprint6_subset_v1` tests the three-asset quorum on the same January–April
  grid and 14 May–November native tape clocks. Availability at entry governs
  ranking. Subsequent gaps can invalidate the chosen pair. The existing
  `rank_price_transfer_v1` receipts compare next-bar opens, reported
  quote/base volume ratios and adverse bar extremes with independently
  repriced directions at the two fully covered native clocks.
- `mm_depth_budget_v1` and `mm_depth_admissions_v1` use the 14 prior native
  continuous market units. The initial budget reads only pre-entry L1
  observations. The admission audit matches original reports before
  comparing simulated submitted notional with the nominal sizing input.
- `arb_entry_flow_transfer_v1` and `w06_flow_transfer_v1` keep the
  Core/RH pair tapes, verified funding slots, entry/drain clocks and
  fee assumptions. W06 records the original Crypto Bronze ancestry and
  the elapsed time without joint books or a connected trade feed.
  Cash from incomplete tape coverage is diagnostic.
- `sprint6_matrix_v1` and `sprint6_inventory_audit_v1` pin the released
  Linux source and binary hashes, tape lineage, configuration for every arm
  and report hashes. Forty-two instrumented reruns compare every existing
  cash and execution field against the original before counting inventory
  sign changes and the subset from reduce-only orders. The separate
  cross-sectional cache correction does not change these frozen artifacts.

### Sprint 7 native cross-sectional and zero-reward execution studies

- The goldmine Lighter dataset supplies daily BTC, ETH, SOL and XRP perpetual
  depth changes with separate exchange and recorder times, fixed-point quantities,
  embedded full snapshots, and per-market control files where present. The study
  pins the July 2026 dataset revision and records object hashes, missing objects,
  market, UTC date and capture range. A row's physical ordering and both timestamps
  matter when deciding what a trader could observe. [Publisher format and dataset](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026)
- CryptoHFTData provides hourly Lighter order-book archives. Check the actual
  file for exchange transaction time and linked nonces. Earlier files can lack
  both. An hour-opening checkpoint can initialize depth, but its local timestamp
  cannot establish venue freshness. A gap requires a later valid snapshot.
  Keep failed fetches and incompatible file schemas in the campaign inventory.
  [Lighter wire format](https://www.cryptohftdata.com/docs/wire-format-lighter)
  [Hourly archives](https://www.cryptohftdata.com/docs/rest-orderbook)
- `gold/forward_ops_20261118/multi_month_native_v1` fixes six UTC dates,
  four perpetual markets, a 24-hour native ranking lookback, ten-hour holding
  time, both ranking directions, visible-depth constraints and settlement costs.
  The separate `sprint7_monthly_ioc_v2` takes completed Lighter Core candles
  for the rank and Tardis native books for executable prices at fourteen
  May–November clocks. Its receipts distinguish full coverage, endpoint pricing,
  and unavailable inputs. Both studies verify hourly per-base funding.
- `sprint7_cash_v1` and `sprint7_depth_transfer_v1` cover the original
  native market units, separately recorded Taiwan/Tokyo sessions and Tokyo
  paired-market tapes. Their manifests pin the code, clocks, fee settings,
  visible-depth and queue parameters, reports, and outcomes where cash or
  funding could not be determined. Positive cash is scoped to the source,
  instrument and execution model in the receipt.
- `sprint7_native_month_grid_v1` reuses the seven month-first Tardis dates
  with four native bid/ask rank anchors, a 13:00 UTC decision and native
  execution through 23:00 UTC. The 12-hour and 6-hour lookbacks fit within
  the recorded day. `sprint7_native_offmonth_six_v1` transfers the same
  six-hour clock and both trading directions to the independently recorded
  dates in the six-date protocol. Source hashes and holding-period gaps are
  retained per market and price clock.
- `sprint7_passive_venue_v1` keeps both Core and Robinhood legs in their
  original order. Its tape audit counts actual public prints, offline
  milliseconds by venue and the elapsed time without joint fresh books,
  before comparing which venue supplies the passive entry. The receipts
  preserve flat, unsettled and no-trade outcomes.
- `sprint7_local_markout_v1` uses the same SHA-256-verified public
  research inputs as the earlier 1% displayed-depth study. Its 112 runs
  include 28 unchanged controls. Per-market receipts identify missing feeds,
  unpriced cash and late delivery. Three quote-reserve settings use only
  matured, same-venue post-fill observations. The frozen forward protocol
  and its executable have independent hashes.
- `sprint7_native_timing_risk_v3` records May–November monthly native
  receipt and venue times, including the 07:00 and 13:00 UTC decision
  cohorts. The 56 intended outcomes include six missing pre-decision risk
  histories. Its fourteen fixed-size 13:00 controls match the previous
  implementation's pair selection, cash, stressed cash and turnover.
- `sprint7_arb_stage_cash_v1` decomposes the 72 passive-venue histories by
  stage and reconciles every settled wallet. `sprint7_arb_clock_wait_v1`
  retains the historical symmetric venue terms and separately models the
  documented Standard-account RH 200/200/300 ms maker/cancel/taker delays.
  Market rules are a public snapshot and the experimental clocks also
  include the original modeled transport and response time.
  [Lighter's RH protocol overview](https://docs.lighter.xyz/developers/robinhood-chain/overview.md).
- `sprint7_goldmine_maker_v1` binds six BTC/ETH L2 and public print
  archives across July 19, 22 and 25 to the immutable publisher revision.
  Six print archives match the publisher LFS SHA-256. Final public Core
  executions are reconciled by market, trade ID, timestamp, price, quantity
  and aggressor, with receive time verified independently. Identical repeat
  messages are collapsed at first receipt. The receipts preserve missing
  or late trades, the warm-up and drain, and the all-interval book audit.
  Three of the six market windows qualify for a full ten-hour cash result.
  [Goldmine's frame and trade schema](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026).
- `sprint7_arb_clock_wait_v2` registers an eight-second unhedged offset
  wait that satisfies the existing two-cancel and fill-witness deadlines
  under both documented venue clocks. The earlier four-second configuration
  rejects are retained in v1 and excluded from cash comparisons.
- `sprint7_independent_wire_v1` reuses the three reconciled July maker
  windows and four Taiwan market windows. It tests 3, 10 and 100 ms outgoing
  action clocks against identical native input hashes, with fixed venue
  delays and response time. Seven 10 ms control outputs match byte for byte.
- `sprint7_july_calendar_v1` covers all 44 BTC, ETH, SOL and XRP daily
  L2 files in the pinned July 17–27 Goldmine snapshot. All SHA-256 values
  match the publisher LFS index. The original 12:10 UTC daily strategy has
  ten adjacent date pairs, eight priceable endpoints and three exact prior
  controls. The first and last date pairs lack four fresh rank anchors.
  Whole-request failures and the verified HTTP range recoveries are
  independently recorded in the campaign inventory.
- `sprint7_july_24h_sessions_v1` reuses the same 44 original books at
  01:00, 07:00 and 13:00 UTC with the original 24-hour native lookback.
  Five selected-pair holdings across four dates have continuous book
  coverage for the ten hours. Same-day sessions overlap and results
  remain separate by clock, coverage and direction.
- `sprint7_inventory_units_v1` binds all 21 eligible native market
  windows to the 84-run inventory comparison. Its two factors are raw
  asset units versus USD 500 units in the A-S reservation price and an
  explicit three-basis-point versus zero inventory skew. Each receipt
  retains base and stressed cash, peak and mean inventory at market value,
  pre-overhead model drawdown, and control digest.

- `sprint7_monthly_24h_join_v1` joins twenty-eight additional native
  prior-day hourly files to the seven established May–November month-first
  Tardis sessions at 13:00 UTC. Every earlier file was physically inspected,
  including the outer-Zstd Parquet files. The first six decision dates lack
  a venue last-update time on the preceding day's anchor. September 30 has
  previous nonces on its updates. October 31 has both fields on all four
  markets, which supports the November 1 daily rank. Receipt time, publication
  time and matching-engine book time are tracked separately.
  [Publisher clock and sequence definitions](https://www.cryptohftdata.com/docs/wire-format-lighter).

### Sprint 8 independent source and queue reconciliation

- Three new BTC maker sessions use the July 18, 21 and 24 Goldmine books
  and public trade IDs matched to Lighter Core's daily final settlement
  ledger. All 241,805 and 336,900 regular executions match on July 18 and
  21. The July 24 recorder omits 377 regular executions, includes 10 extra
  IDs and contains 265 late prints. Its cash is withheld. Both acquisition
  routes, publisher object hashes, physical reconstruction and ten-hour
  coverage are in `sprint8_capital_transfer_v2`. Entry size, markout cost
  and taker stress are recorded separately from the source comparison.
  [Publisher archives](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026)
  [Official daily history](https://docs.lighter.xyz/developers/integration-guides/historical-data.md)
- The paired synthetic market-by-price witness in
  `sprint8_queue_order_witness_v1` and `v2` uses the same economic event
  twice with different book and trade receipt order. Both replay engines
  now wait for the latest admissible trade delivery before attributing the
  unaccounted decline in depth. The historical comparisons register both
  queue sensitivities, the unmodified conservative control, the individual
  tape hashes, and all W01–W06 pairing outcomes separately. The queue
  share uses the normalized power function for front and back depth.
  [Reference queue implementation](https://github.com/nkaz001/hftbacktest/blob/master/hftbacktest/src/backtest/models/queue.rs)
- `sprint8_multiday_carry_v1` reuses the previously hashed Core/RH public
  five-minute trade bars and official hourly settlements across September
  24–November 17. The 10, 30 and 60 hour positions use the same pre-entry
  funding signal, keep inventory through intermediate ten-hour marks, and
  separate basis movement, funding, terminal price stress, missing bars and
  trading costs. All 524 ten-hour controls reconcile to the original public
  price screen. [Funding and transaction cost model](https://arxiv.org/abs/2212.06888)

### Four-market order books, execution and forward-window reconciliation

- `sprint8_native_twice_monthly_v1` registers the 8th and 12th of each
  month from January through November 2026 before ranking BTC, ETH, SOL
  and XRP. Of 88 prior-hour objects, 77 were retrieved. The availability
  audit counts actual venue timestamps and linked update IDs separately.
  Four October/November dates qualified for reconstruction, with all
  176 holding-hour objects available. Orders use 310 ms, observed
  depth, venue lot sizes, fees and official hourly funding. All four
  valued positions have intra-hold coverage gaps. The earlier failed
  retrievals and subsequent alternate-route recovery remain in the campaign.
  [Publisher wire format](https://www.cryptohftdata.com/docs/wire-format-lighter)
- `sprint8_print_rank_native_execution_v1` uses independent first-of-month
  May–November clocks. The previous day's final minute before 13:00 UTC
  uses CryptoHFT receipt and execution clocks, and normal trade IDs matched
  to official Core settlement records. Both aggressor sides are required.
  Rank extremes must survive the observed price ranges. Actual entry and
  exit use the established Tardis Lighter books, with identical protective
  IOC, market sizes, trading costs and ten-hour funding. The seven daily
  ledgers and 27 of 28 hourly sources were available. The June 30 XRP
  hour returned HTTP 404 from both acquisition routes. Only September
  and October produced priceable pairs. Source manifests retain the
  individual hashes, unavailable inputs and ambiguous ranks.
  [Official exchange files](https://docs.lighter.xyz/developers/integration-guides/historical-data.md) ·
  [Tardis coverage API](https://api.tardis.dev/v1/exchanges/lighter)
- `sprint8_preopen_market_selection_v1` uses 59 prior one-second
  samples in each of nine paired BTC/ETH market choices. The corresponding
  historical ten-hour cash ledgers keep both the chosen market and the
  alternative. `sprint8_arcus_public_flow_v2` covers 18 separate equity
  markets over four following windows. It checks each recorded public print
  against the next fresh mark and the available size for a same-size exit.
  Prints do not identify a simulated resting order's position in the queue.
  [SEC execution-quality methodology](https://www.sec.gov/files/rules/final/2024/34-99679.pdf)
- `sprint8_rwa_pipeline_audit_v1` matches the original Tokyo
  QQQ/SPY paired reference frames for W01–W05 and W07 to reconstructed
  books by both time and price/depth. Across 4,248,556 matched frames,
  4,247,145 contained a positive observed RH internal reference. All
  shared and Arcus references used last close and all lacked the equity
  session field. The legacy pair writer drops these fields. The audited
  preservation path reproduces upstream availability. Each original
  W07 eligibility decision remains in its frozen cash ledger.
- `sprint9_review` reconstructs the historical DSR and CSCV cohorts
  from their recorded per-window ledgers. A scheduled, cash-reconciled
  no-trade window contributes zero. Missing, unresolved or unpriced
  results remain missing. Each track uses its own complete common
  intersection and reports all scheduled and constant arms. The early
  rank group has 209 common windows of 285. The later funding and
  quant groups respectively have 50 and 78 common windows of 78.
  [Authors' CSCV methodology](https://scholarworks.wmich.edu/math_pubs/42/)
- `sprint9_passive_pairs_v1` records 96 W01–W06 Tokyo BTC/ETH
  Core/RH comparisons. The 24 baseline cash and fill controls reproduce
  on the frozen engine. `sprint9_forward_pairs_v2` retains its separate
  pre-W09 freeze, W06 regression and the same four strategy factors.
  `sprint10_mm_factors_v1` independently freezes eight Core maker
  configurations before W09. Twenty-eight historical Taiwan and July
  runs test the two factors on the original native tape hashes.
- `sprint10_mm_training_v2` and `sprint10_mm_lifetime_training_v2`
  retain both releases of the opt-in maker calibration diagnostics for
  the same 14 native market windows. BTC and ETH each have six distinct
  ten-hour windows, with two non-overlapping decisions on the May date.
  SOL and XRP each supply one descriptive control. The first execution
  incorrectly expected seven per BTC/ETH market; the actual date, tape
  and symbol inventory determines the grouping. Resting-order exposure,
  settlement and markout counts are kept separately. The diagnostic
  `sprint10_mm_exposure_witness` compares explicit activation and
  terminal times across consecutive book events.

- `sprint11_five_minute_print_rank_v1` extends the registered
  prior-day trade interval from one to five minutes on the same seven
  May–November dates. All normal prints used in the signal were checked
  against official Lighter trades by ID, side, price, size and exchange
  time. August gains both directions, but its extreme ranks overlap.
  September and October reproduce the earlier execution values. These
  seven dates overlap the one-minute study. The public CryptoHFT object,
  official file and Tardis execution tape digests are in the campaign.
- `sprint11_mm_causal_training_v1` retains the historical pre-execution
  price and true resting-order exposure for the same 14 training market
  windows. `sprint11_btc_fit_transfer_v1` records all 20 BTC transfer
  comparisons on four historical sessions and the separately labelled W07
  rehearsal. It uses the measured BTC fit from six training windows.
  ETH supplied 19 eligible fills against the specified minimum of 20.
  Distinct versions remain independently addressable in the campaign.
- `sprint11_interval_rank_v1` reuses the same seven five-minute
  reference dates and the original Tardis execution tapes. It divides each
  $50 side across all possible extreme markets determined at entry. August
  and November each have two possible counterparts. September and October
  reproduce both singleton controls. All four realized baskets have
  endpoint-only cash and share dates with the earlier evidence.

### November 19 cash and source-clock peer review

- `last_print_rank_20261119` retains all seven original May–November
  13:00 UTC dates. Match each observed ordinary trade to official Core
  exchange time, ID, side, amount and price. Each prior-day anchor uses
  the latest exchange-time buy and sell received before the cutoff
  within the preceding hour. Record missing official prints and whether
  both last-side IDs match the later archive. Six dates have native
  Tardis IOC entries and exits with ten hourly funding settlements.
  The missing June 30 XRP file withholds July. Use the same seven-date
  denominator as the one- and five-minute studies.
- `entry_markout_review_20261119` records both sets of 72 Tokyo
  BTC/ETH, Core/RH maker-role comparisons across W01–W06. Preserve
  the earlier attempt rejected by the incompatible calibration snapshot,
  the matched reference with Gemini's original pricing rule, and the
  revised hedge-source-clock version. Configurations, engine hashes,
  tape hashes, terminal positions and paired cash are retained separately.
- `btc_spread_factors_20261119` records eight variations across the
  five existing BTC transfer sessions. It varies only the fitted arrival
  decay, gamma and inventory budget. `btc_kappa_forward_20261119`
  freezes the decay-only pair for W09–W16, with W07 marked as a rehearsal.
  `standing_baseline_after_w16` registers routine future ten-hour
  replays separately from the original 14-arm W07–W16 campaign.
- `kappa_training_stability_20261119` reuses the twelve distinct BTC
  and ETH ten-hour reports in `sprint11_mm_schema_training_v1`.
  Their parent manifest identifies each venue, tape, config, clock and
  engine. The diagnostic verifies report hashes and disjoint trading
  intervals per market, then drops each window and each calendar date
  in turn. Each fit reapplies the original 60-second exposure and
  20-fill minimums. The separately labelled duplicate-input control
  measures the extractor's admission behavior and counts no new data.
- `four_coin_observed_risk_v2_20261119` uses the seven May–November
  dates and both allocations from `last_print_rank_20261119`. The cash
  reconciliation traces each funding input to its original receipt and
  payload hashes. The initial attempt looked only in the older cache
  and stopped before completing a date. All 13 date/market references
  match the supplemental monthly IOC cache. Risk marks use one-second
  pre-clock native midpoints for all held legs, excluding minutes with
  a book fault. The observed peak-to-trough loss is a lower bound under
  this valuation convention. Gaps and the unallocated July date remain
  in the coverage report.
- `four_coin_ioc_latency_20261119` replays the same entry and exit
  decisions with assumed 10, 150 and 310 ms order delays. Each delay
  has independent fill checks against depth received by its arrival.
  The reference position and ten funding settlements must reconcile
  before attributing a cash difference to the simulated execution.
  These historical delays do not measure either host's network path.
- `arb_preopen_venue_20261119` uses the paired BTC and ETH Core/RH
  tapes for Tokyo W01–W06 and the 24 independently replayed gate-off
  wallets in `entry_markout_review_20261119/reviewed`. Each of the
  59 pre-open observations uses quotes received strictly before that
  second. The role decision and its input hashes are stored before the
  cash lookup. Ten-hour outcomes retain each role's funding, fees,
  stress and terminal status. A separate hindsight comparison includes
  only windows where both roles settled. All dates are retrospective.
- `arb_directional_open_20261119` reuses the 12 Tokyo W01–W06
  BTC/ETH pair tapes and the 24 gate-off role wallets from the
  reviewed entry-markout campaign. With $100 quote notional and the
  original queue engine, the historical comparison varies two opening
  conditions independently. They are a 1.50 bp directional edge at the
  two venues' touches and a maker queue no larger than five quote
  notionals. The report retains all unfinished wallets. It groups
  completed cycles by whether an opening hedge filled and whether an
  IOC was needed for a forced exit. These records use simulated fills.
  The four active edge-filter reports also reproduce byte for byte with
  branch diagnostics in a separate build. Two distinct ETH cycles have
  a simultaneous empty frame across venues. The next usable paired
  quotes arrive 101 and 149 ms later on their respective replay tapes.
- `arb_refresh_source_quarantine_20261119` links six historical Tokyo ETH
  Core/RH ten-hour tapes (W01–W06) to the two public book connections' hourly
  source shards. It verifies shard hashes, archived manifests, compiled tape
  provenance and the original availability rule (ceil receipt-available wall
  nanoseconds to milliseconds). The source channel and public market stats
  establish market 0 as ETH in each deployment. W04 retains its quarantined
  diagnostic classification. W01 retains a mismatch between the available
  compile config and its recorded hash. The protocol records the initial
  sealed-path lookup failure and the independent six-window retry. The two
  previously scored exits use their existing fee and funding assumptions.
  This diagnostic counts missing books and recovery times without pricing
  new fills. Its 120-second shift changes only the historical RH scheduled
  outage intervals. The separate two-server loopback exercise uses synthetic
  books and independent connection epochs.
- Tokyo W08 crypto, RWA and BULK reached aleph on 2026-11-19 at 00:36 UTC.
  The three sealed gzip files total 16,294,410,023 bytes and 226,285,007
  records. Each spans about 36,900 seconds. The vault manifest, source
  and staged file digests, byte lengths, source availability bounds and
  per-connection disconnect counts are recorded in
  `gold/forward_ops_20261118/w08/bronze-audit-0850.json`. The publisher
  assigned `selection_dev`. The original 14-arm protocol predates the start
  of all three captures. Use its per-wallet funding, execution and terminal
  evidence when interpreting any subsequent scores.
- `mm_inventory_wait_20261119` reuses all 14 historical schema-training
  maker tapes and all five BTC transfer controls, including the W07 rehearsal.
  Its protocol binds every instrument, ten-hour interval, tape digest, engine,
  source config and original report. The four historical arms cross 50/100 s
  inventory expiry with 5/10 s onset of the age-based exit concession.
  The original venue and receipt clocks, funding guards, modeled fills and fees
  are preserved. Cost stress adds 0.25 bp of gross notional and 1 bp of taker
  notional. The source inventory records both the design and results.

### 2026-11-19 four-coin BTC exposure and gross-matched sizing

Historical selection research reuses the previously catalogued BTC, ETH, SOL and XRP Lighter Core native order books, exchange-matched prior-day public trades and original funding receipts on the fixed May–November 2026 first days. Five-minute synchronous returns used only observations already received at the measurement time, after applying the native age and gap rules. Two of seven dates qualified for the new native comparison. Separately, official Lighter January–April five-minute candles and hourly funding priced 242 common ten-hour windows. Subsequent BTC exposure could be estimated on 240. These are bar valuations with unknown historical receipt latency. The native and candle protocols retain parent/source digests, dated coverage, sealed allocations, fees and transformations under lakehouse Gold `forward_ops_20261118/four_coin_beta_size_20261119/`. Native cash review and holding-period exposure diagnostics have separate entries in the append-only campaign inventory. The common-factor rationale is adapted from [Avellaneda and Lee, Statistical Arbitrage in the U.S. Equities Market](https://math.nyu.edu/inmemoriam/avellaneda/AvellanedaLeeStatArb20090616.pdf).

### W08 Arcus ETH trade identity and per-stream availability

The 2026-11-18 13:29 UTC Tokyo crypto capture contained four Arcus ETH-USD identifiers with different microsecond timestamps across observations, at most 57 microseconds apart, and identical price, size and side. The complete crypto Arcus shard audit found no such differences for BTC-USD or SOL-USD. Full-stream strict Silver failed its original identity check. The scoped derivative excludes all Arcus ETH-USD public trades and records that omission separately from the canonical catalog. BTC Core and ETH Core book pairs were compiled with their original Lighter/Hyperliquid configs and engine. The W08 recovery protocol checks RWA market identity on its own sealed archive before admitting either frozen stock strategy. The original Bronze files, strict rejection, source receipts and frozen scoring protocol remain in the inventory. The per-stream manifests, scope and completion receipts are under lakehouse Gold `forward_ops_20261118/w08_source_recovery/`.

W08 RWA subsequently completed its original archive audit on all 11 Arcus trade shards, 18 markets and 116,067 observations. GOOGL-USD had one trade identifier with a 272-microsecond timestamp disagreement and equal price, size and side. SPY-USD (26,228 observations) and QQQ-USD (9,473) had zero conflicting identities. Their own sealed sources pass the pre-scoring identity gate for the two frozen stock wallets. The RWA projection excludes GOOGL-USD Arcus public trades. The full W08 crypto and RWA canonical catalogs and combined GPU result remain unavailable. The per-market counts and digests are recorded in `w08_source_recovery/rwa-audit.json`; W07 regression reproduces all 14 original configuration clocks and cash evaluations.

### 2026-11-19 W08 and independent Sprint 12 cash reviews

- W08 uses its pre-window 14-profile configuration. The scoped crypto and RWA source audits retain the original identity conflicts and the full-stream physical gaps. Twelve BTC engine reports were reproduced byte for byte with passive diagnostics of the last data pause, residual inventory presence, and the strategy return clock. Four of twelve stopped after one of ten funding boundaries. The local tape-drain clock is available separately. The WSL campaign inventory records the frozen input hashes, clocks, runner, both diagnostic builds and all dispositions under `gold/forward_ops_20261118/sprint12_peer_research_20261119`. The two-window cash-rollup v2 retains missing funding and inactive paired markets separately. Its reporting tolerance is two binary floating-point spacings at a scale of at least one dollar, and v1 remains reproducible.
- The Sprint 12 historical market-making study crosses three inventory horizons with cooldown-side cancellation isolation across 19 previously used ten-hour wallets. The arbitrage study compares strict, 200 ms and 500 ms balanced-hedge protection in 24 Tokyo BTC/ETH venue-role wallets. Each arm retains its own costs, modeled capital and labels. The 114 maker and 72 paired wallets include the unchanged controls. Separate derived studies bind the signed, turnover-weighted maker midpoint marks at one, five and thirty seconds, and both receipt and source ages at the ETH joint-book stop to the output hashes. [Execution-quality measurement reference](https://www.sec.gov/rules-regulations/staff-guidance/trading-markets-frequently-asked-questions/frequently-asked-questions-rule-605-regulation-nms)
- 0xArchive advertises Lighter Core price-level history from January 29, 2026. Anonymous archive requests for BTC, ETH, SOL and XRP at 12:00 UTC on February, March and April 15 returned HTTP 401 using the system client. The preceding Python transport failures are retained. Before using a future authorized response, verify both venue and receipt timestamps, distinct markets, coverage and funding. The anonymous probes contributed zero executable windows. [Provider history and example](https://0xarchive.io/resources/comparisons/lighter-native-api-vs-0xarchive) · [Provider access terms](https://0xarchive.io/resources/market-data-guides/lighter-api)

### 2026-11-19 Sprint 13 paired replay and native ranking transfers

- `s13_final_20261119` reuses all 19 prior BTC/ETH maker wallets and 24 paired venue-role wallets on the same ten-hour UTC intervals, cutoffs, replay tapes, modeled balances and fees. Both the intermediate and final engine snapshots, ordered source hashes, protocols, missing terminal/funding cases and 186 modeled wallet outcomes per revision remain in the campaign inventory. The six-case arbitrage phase audit prices changes in realized simulated exposure using both venue midpoints available at the fill receipt. Only aggregated exposure phases leave the research host. The latency sensitivity crosses configured transport and fill-notification delays separately and preserves venue speed bumps. The local maker-size sensitivity uses the native Core midpoint and completed minute ranges on the original 19 tapes. All are historical selections with 0.25 bp gross-notional cost and an additional 1 bp of taker notional.
- `s13_independent_20261119/{fourth_native,eighteenth_native}` covers each fourth and eighteenth calendar day from January through November 2026. Across 88 prior-day four-asset Lighter Core market-hour requests, 12 archives had exchange transaction time and linked update numbers, 67 had insufficient fields and nine returned HTTP 404. The three four-asset dates qualifying for reconstruction used another 132 holding-hour files. Both receipt and venue clocks precede each priced native decision. The 310 ms protected IOC uses 20% of eligible displayed depth and 3 bp price protection, rounded venue lots, ten official hourly funding payments and zero incentives. Record endpoint execution, incomplete fills and within-window gaps separately. [Native publisher schema](https://www.cryptohftdata.com/docs/wire-format-lighter)
- `s13_independent_20261119/residual_rank` reuses 285 preassigned January–April ten-hour units from the official Lighter candle/funding panels with 230 paired valuations. Its separate `native_factor` transfer covers all 22 calendar dates above, with an additional 132 prior native market-hour files for the three estimable dates. BTC slopes use synchronized five-minute returns 36 to 26 hours before the decision. The 24-hour residual rank retains equal per-leg dollar targets. File hashes, coverage, independent availability clocks, lot and fee assumptions, parameter freezes and every incomplete or failed outcome remain in Gold. [Factor-residual research](https://math.nyu.edu/~avellane/AvellanedaLeeStatArb20090616.pdf)
- W08 Drive readback is verified for all 16,294,410,023 bytes against each publisher checksum and both archive receipt locations. W08 crypto and RWA projected catalogs reconstruct from the independently verified Bronze gzip objects with the identified Arcus conflicting market excluded from its public trade stream. Their isolated catalogs have separate provenance, staging and per-table row-count checks. The full canonical crypto, RWA and combined GPU dispositions retain the original source conflicts.

### 2026-11-19 four-coin pair timing and execution cost attribution

The S14 historical timing comparison reuses the official Lighter Core January–April 2026 five-minute bars, settled hourly funding and the same 285 ten-hour decisions. Every possible ordered two-asset basket from BTC, ETH, SOL and XRP is priced at the prior $50-per-leg budget under the original rounding and cost assumptions. The 207 dates with prices for all twelve pairs and both ranking signals support a within-month, within-split rotation check, with timing gaps retained in the accompanying inventory. The corrected analysis uses the source Unix seconds; the first millisecond interpretation and its outputs are preserved separately. The companion wallet-level execution study rechecks the hashes of 57 native maker and 96 two-venue delay reports from S13. It measures the change in traded amount and net return per dollar independently for each comparable wallet, with separate counts for inactive and unpriced wallets. Both designs, dependencies, coverage, signed components and first-attempt dispositions are indexed under `gold/forward_ops_20261118/s14_independent_20261119`. [Trading-rule selection and data snooping](https://doi.org/10.1111/0022-1082.00163)

### 2026-11-19 native four-asset prior ranks, maker ladders and partner-hedge execution

The four-asset historical timing study compared decisions ten hours apart. The native replay retained all 22 preselected dates. For the three dates with prior executable endpoints, it checked 156 Lighter market-hour objects from CryptoHFTData and reconstructed four synchronized BTC/ETH/SOL/XRP midpoint series under the original sequence, venue-time and receipt-time gates. One incomplete transfer and one connection timeout preceded a verified retry of the 2026-11-16 15:00 UTC ETH hour. The final objects total 852,194,525 bytes. Ten official hourly funding slots and protected 310 ms IOC endpoints were required for each priced direction. Complete holding-period coverage and availability of the prior signal remain distinct. A separate public DeepTick catalog probe and six small Lighter L2 requests returned HTTP 503 from aleph. No DeepTick prices entered the results. [Provider coverage](https://deeptick.dev/datasets/lighter/) · [Sample scope](https://deeptick.dev/samples/)

The execution study records 24 Tokyo BTC/ETH role wallets on previously registered native tapes, two modeled latency settings and five offline partner IOC settings. Files include source and executable digests, all 240 input hashes, settlement states and the reconciled changes in entry, exit, price drift and fees. The native Core maker study replays 19 previously registered ten-hour market windows with the original two-level $500 top nominal and a common $500 per-side nominal split across one, two or three levels. Its corrected accounting launcher matches every original configuration digest after an import-name conflict. These premiums, placements and time assumptions are historical sensitivities. The original paper engines and untouched forward configurations retain their own registries. All inputs and dispositions are indexed under `gold/forward_ops_20261118/s14_independent_20261119`. [Order placement and execution costs](https://arxiv.org/abs/1210.1625)

### 2026-11-19 native four-coin calendar transfer, recorded maker risk and arbitrage admission

Matched official Lighter Core five-minute transaction-candle opens to native decision midpoints, 310 ms arrival midpoints and protected IOC executions on four perpetuals (BTC, ETH, SOL and XRP). The source inventory includes all 40 market-days in the two matched-price studies, their day boundaries, file hashes and prior versus new receipt status. Twelve market-days cover the three eligible dates in the historical January–November 4/18 calendar; 28 cover seven previously registered May–November month-first dates. Each priced two-leg wallet uses ten verified hourly funding payments per leg and the same filled lot quantities in both valuations. The August 1 13:00 UTC comparison lacks sufficient XRP candle volume at exit. Sample overlaps, incomplete fills, 19 blocked dates and intrawindow coverage limits are preserved. [Official Lighter candle interface](https://github.com/elliottech/lighter-python/blob/main/docs/CandlestickApi.md)

The fixed 13:00 UTC six-hour momentum/reversal pair was separately transferred to the 4th and 18th of September, October and November. CryptoHFTData's public Lighter-native source supplied 23 of 24 initial 06:00 UTC anchor hours. The September 18 SOL file returned HTTP 404. Four assets with actual exchange timestamps and nonce linkage qualified on three dates. All 144 holding market-hours from 12:00 through 23:59 UTC on those dates were hash-verified (857,153,763 bytes). Ten-hour cash and paired observed seconds are reported separately. These historical dates also appeared in the 24-hour ranking campaign. [Provider clock and sequence definitions](https://www.cryptohftdata.com/docs/wire-format-lighter)

The maker experiments use nineteen prior ten-hour Core wallets and source-hashed replay engines. They cross three OFI widening levels and three volatility coefficients; a separate study holds those at the original levels and crosses assumed cancel-risk horizons (310/550 ms) with three simulated one-way order transport profiles. All nine strength cells and all six timing cells retain their independent cash, turnover, markouts, flatness and missing states. The arbitrage admission experiment uses 24 Tokyo BTC/ETH venue-role wallets from W01–W06, with a 60-second warm-up, continuous ten-hour interval and 15-second drain. Both hedge execution policies were evaluated at three opening edge floors (1.5/2.5/10 bp). All studies use zero incentives and stressed taker turnover. Designs, file hashes, failures and wallet dispositions are indexed under `gold/forward_ops_20261118/s15_independent_20261119`. [Order flow imbalance and price impact](https://arxiv.org/abs/1011.6402) · [Venue terms](https://docs.lighter.xyz/trading/trading-fees)

### 2026-11-19 July native transfer, two complete-week registrations and W08 public exceptions

The six-hour cross-sectional study reused the 44 immutable public Goldmine Lighter BTC, ETH, SOL and XRP daily books for July 17–27 (2,863,187,402 bytes). The source manifest binds each native object key, date, size, SHA-256 and receipt. Eight of eleven dates priced both protected endpoints with ten hourly funding observations per leg. Only July 19 also passed two-leg holding-period continuity. The separate October 12–18 and November 9–15 CryptoHFTData study registered all fourteen dates before scoring. Nine initial transport failures on the 56 signal anchor market-hours were recovered against the original URLs. Positions and results for the two directions are isolated. Both inventories, original failures and covered holding market-hours are indexed under `s16_independent_20261119` in the append-only campaign inventory. [Goldmine Lighter dataset](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026) · [CryptoHFTData wire format](https://www.cryptohftdata.com/docs/wire-format-lighter)

The July BTC maker study froze its selection using May 1 and July 1 before transferring all nine settings. July 18, 19, 21 and 25 used native ten-hour tapes. July 24 retained its public-trade reconciliation failure. For the Tokyo W01–W06 exit study, 144 reports retain the original hashes and gates for both venues and both roles, including eighteen funding or settlement omissions. The W08 RWA source audit independently scanned 22 public Lighter trade shards, verified the Robinhood SNDK zero-quantity exception and registered a retained economic ledger fingerprint for its 11,219 updates. QQQ and SPY are the two frozen equity instruments. [Order placement under execution risk](https://arxiv.org/abs/1210.1625)

### 2026-11-19 Sprint 15 paired execution tests and phase-clock reproduction

The isolated Linux replay used 19 previous native Core maker wallets and 24 Tokyo BTC/ETH venue-role wallets from W01–W06. The 134 completed reports include 67 unchanged byte controls. For each measured wallet the registry retains the tape hash, previous and current cash, configuration digest, source revision and precise settlement omissions. A separate synthetic test reused the same market events and phase-transition timing on the two source revisions and measured the last unmatched position. The two historical full weeks retained 19 holding-hour transport failures on the first acquisition and all four repeated historical cash controls before rerunning the registered source recovery. [Tokyo and historical source registry](#2026-11-19-july-native-transfer-two-complete-week-registrations-and-w08-public-exceptions)

### 2026-11-19 phase-clock and exit-policy interaction on Tokyo W01–W06

The completed factorial reused the 24 prior ten-hour native BTC/ETH venue-role wallets, both admission floors and their original warm-up, drain, fees and funding gates. All 96 broad-IOC controls on the phase-clock engine match the original report bytes. The 48 entry-only, long-patience reports also exactly match their Sprint 15 versions. Each of the eight cells retains 21 cash-verifiable and three unpriced wallets. The pre-existing seven July native paths with partial coverage now carry per-leg physical gaps and joint one-second counts in addition to their protected execution endpoints. [Queue-aware execution cost model](https://arxiv.org/abs/1210.1625)

### 2026-11-19 full two-week native coverage and subsequent four-asset registration

For October 12–18 and November 9–15, all 56 six-hour ranking anchor objects and all 672 holding market-hour files covering four native Lighter markets were recovered and hashed. The latter total 3,836,746,392 bytes. Source-ledger revisions retain the initial 19 and 41 transport failures in separate acquisition batches, the ten remaining timeouts after retry, and the completed longer-timeout readback. All 28 direction/date settlements kept exactly the same cash after the last ten files arrived. All fourteen pairs have valid protected entry/exit observations and hourly funding. Native paired availability spans 35,642–35,972 of 36,000 one-second marks per date. Every holding path also had a physical gap or reset. Reversal stress was positive on 9/14 dates and its largest date exceeded the aggregate profit. A later registration uses the same four assets and rules on October 19–25 and November 2–8. Two dates in the later registration overlap previous six-hour controls and are explicitly identified. [CryptoHFTData wire schema](https://www.cryptohftdata.com/docs/wire-format-lighter)

### 2026-11-19 adjacent native signal anchor recovery and cross-platform replay control

The October 19–25 and November 2–8 six-hour signal cohort has all 56 BTC, ETH, SOL and XRP signal anchor files at the original public URLs. Three files that timed out from aleph were received on the existing Mac relay and verified by SHA-256 after transfer to aleph. Both transport attempts and the original manifest are retained. The two November 4–5 dates remain overlap controls. Each date requires separate entry, exit, funding and intraperiod quality checks. The finalizer checks 672 unique holding market-hour keys and both registered directions on all fourteen dates. It records paired coverage, missing inputs, previous control equality and the leave-one-date-out net range. Its date-level aggregation reproduced the earlier fourteen pairs and rejected incomplete and duplicate direction fixtures. [Native Lighter data specification](https://www.cryptohftdata.com/docs/wire-format-lighter)

An independent offline Linux replay of the synthetic forced-drain fixture reproduced the earlier Mac clock results on the two pinned source hashes. The slow entry retry previously delayed risk resolution to 2,610 ms. Both retry intervals resolved at 1,310 ms after the phase-clock correction. The accompanying same-leg reprice-spacing regression passed. These three synthetic checks have a separate inventory receipt from the 192 historical wallets.

### 2026-11-19 equal-budget four-asset native pair reference and price-cost attribution

October 12–18 and November 9–15 retain their registered 13:00/23:00 UTC decision times, protected 310 ms execution, lot/depth constraints and zero incentives. All 168 directed pair/date alternatives have verified entry, exit and ten hourly funding slots per traded leg. The 56 market-date inventory links original native sample hashes, exact public Lighter funding URLs, response hashes and acquisition receipts. The original 28 strategy/date controls reconcile in turnover and cash. Same-day uniform basket cash, 24 fixed asset-label permutations, decisions versus arrival midpoints and realized execution costs are retrospective references. The two adjacent calendar weeks were registered for this comparison before those cash results were produced, with their November 4–5 overlaps retained. The independent cash-ledger attribution reconciles 38 maker and 192 arbitrage reports without pricing unknown wallets. [Federal Reserve on implementation shortfall](https://www.bis.org/review/r181206g.pdf)

### 2026-11-19 native inventory-state midpoint attribution and IOC time buckets

Two isolated offline probes use the already-registered parent wallets and public inputs. All 96 Tokyo broad-limit two-leg reports and all 38 single-/two-level maker reports were reproduced byte-for-byte. The additional arbitration receipt decomposes each interval between simulated fills by the inventory actually held. A held leg requires fresh midpoints at both boundaries. Completed-cycle sums match original signed midpoint drift, with overlap across parameter cells and three unpriced wallets per cell retained. The maker receipt reconciles IOC notional, filled level count and shortfall against submitted midpoints into mutually exclusive time buckets using the frozen decision, funding-drain, guard, halt and end clocks. The regular trading bucket does not identify a submission reason. Public source remediation for the adjacent four-asset trial recovered five originally timed-out CryptoHFTData market hours over the established independent Mac route. Each compressed object, byte count, original URL and SHA-256 was verified at aleph before ingestion under its file lock; the original failed manifest remains immutable. [Native source specification](https://www.cryptohftdata.com/docs/wire-format-lighter)

### 2026-11-19 W08 scoped database completion and historical book availability

The W08 original bulk, crypto and RWA archives total 16,294,410,023 bytes. The Mac acceptance references each archive's publisher, aleph and Drive hashes, clean gzip readback and all fourteen frozen dispositions. Both scoped DuckDB databases match their manifests and staged tables, including 939,648 public trades in crypto and 1,244,656 in RWA. Conflicting Arcus ETH-USD and GOOGL-USD trade identities stay excluded in their respective projections. One positive-price zero-size Robinhood SNDK observation was isolated. All 11,219 retained SNDK economic records match the independent source fingerprint, with no nonpositive public trade quantities in the RWA database. The combined GPU input remains withheld. The Mac availability check also confirmed the unchanged Goldmine four-market July revision and Tardis's April 17 Lighter coverage start; two documented public DeepTick endpoints again returned HTTP 503. No additional native windows were credited to those availability checks. [Goldmine native dataset](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026) · [Tardis official catalog](https://api.tardis.dev/v1/exchanges)

### Inventory timeout, passive reductions and total trading cost on the fixed maker wallets

The same archived 38 public-book maker reports were byte-reproduced on an isolated Linux replay build carrying simulated IOC submission causes through async execution. Each wallet reconciles level-fill count, notional and reducing volume to its prior clock-only receipts. Across 14 month-first market windows, all $9,069.36 of regular-session two-level taker activity arose from inventory timeout. The paired intervention used the 19 original two-level wallets with the baseline, a reduce-only size cap and that cap plus the existing passive touch at 65% of the unchanged 50-second hard timeout. A separate Mac readback reconciled all 57 reports and 19 baseline digests, funding slots and explicit stress costs. Month-first stress cost per $1M of gross executed volume was $10.14, $111.26 and $134.34. These are retrospective zero-incentive tests. The source inventory, binary, configuration and tape hashes are in the origin and passive-exit protocols and wallet receipts. [Empirical limit-order execution study](https://arxiv.org/abs/1610.00261)

### Scoped ingestion retry and operational scope readback

The W08 signed projection keeps the Lighter positive-price zero-quantity count, both Arcus trade-identity exclusions and the incomplete combined GPU scope. The separate hourly scope view recognizes the W06 and W08 receipt schemas. Eight synthetic checks cover completion, unfinished scopes, unexplained exclusions, legacy files and idempotence. The normalizer's audited count-mismatch case now releases the newly created unpublished staging directory so a corrected retry can reuse the path. Mac and isolated Linux reproduced the old failure and passed the corrected nine-case ingestion suite, including existing published-directory protection. No new W08 economic records or strategy decisions arise from this change.

### Four-asset long and short selection contributions and adjacent source inventory

Every ordered native basket in the October 12–18 and November 9–15 cohort decomposes into a long-leg and short-leg selection advantage over that day's equal-weight four-asset mean on each side. Both common counterparties must give the same within-side difference and the reconstruction must match all twelve baskets. The observed reversal's $2.11037 relative edge consists of $2.67486 on the long selection and −$0.56449 on the short selection. These are relative cash differences on the same historical wallets. This diagnostic was also registered before the adjacent pair results. For October 19–25 and November 2–8 the source-ledger readback matched all 56 anchor and 672 holding market-hour objects across BTC, ETH, SOL and XRP, totaling 3,354,700,105 holding bytes. Eleven plus five original two-timeout objects were recovered using their same public URLs. Every final digest and receipt matched either its original or its independently verified recovery. [Historical book provider](https://www.cryptohftdata.com/docs/rest-orderbook)

### Adjacent 14-date native direction and all-pair readback

The October 19–25 and November 2–8 four-market Lighter cohort produced 28 settlements, all 168 executable same-budget basket alternatives and 24 shared-label permutation scores per strategy. Four prior direction/date controls matched. Reversal lost $2.61635 across its 14 separately initialized 10-hour wallets, momentum earned $1.72804 and reversal ranked 21st among 24 fixed asset relabelings. After excluding November 4–5 overlap, reversal lost $1.67990 over 12 dates. Signed position and price components keep zero incentives, native tick/lot rules, settled funding and explicit costs. None of the fourteen wallet pairs passed continuous intraday book coverage. Across both legs, the valid window grids contained 35,404–35,972 of 36,000 seconds, with a maximum consecutive 229-second gap. The four-market source and one-day-per-month follow-up protocol are listed in the campaign inventory.

### Tokyo W09 closure and distinct transfer and forward receipts

On 19 November the original hourly campaign observed the W09 bulk, crypto and RWA archives closed, totaling 11,354,324,470 bytes. All three W10 successors were growing in two separate observations. The bounded transfer coordinator started at 10:00:22 UTC and all three source archives were being staged; downstream verification, full-file Drive readback and the frozen forward result each require their own receipts. Tokyo's existing verified-backup cleanup timer remained active with a successful last run. The rollover and transfer snapshots do not imply complete downstream coverage.

### January–September fixed third-Monday four-market book screen

Nine third Mondays, BTC, ETH, SOL and XRP, 06:00 UTC, yielded 36 pre-registered CryptoHFTData native-market checks. Twenty-eight files were receipted on aleph, seven original URLs returned HTTP 404, and the July 20 ETH file timed out twice. A separate Mac read succeeded at the identical URL. Aleph verified the compressed file and its Parquet values. All 29 available files, 69,731,254 bytes, carried final update numbers but no non-null transaction time or prior update number on their update records. No new early date passed the four-asset native signal gate. The October 19 control priced both directions and reproduced previous cash. The separately inventoried Goldmine July 20 books from the same date priced the earlier reversal and momentum returns (−$0.10064 and +$0.03892), with 35,825 jointly priceable seconds of 36,000. That existing provider overlap is counted once.

### 2026-11-19 原生四币等总仓位、前瞻七日及做市套利选择上限

- 原生 Lighter BTC/ETH/SOL/XRP。CryptoHFTData 10/12–25、11/02–15 共 28 日，Goldmine 07/17–27 共 11 日，Tardis 05–11 月首共 7 日。13–23 UTC 连续回放，07 与 13 UTC 的行情构成排名输入，另记初始化和 310 毫秒终端撮合。1×50 或 2×25 美元每侧，盘口保护、逐档容量、最小数量与十个逐时官方资金费、1.25 bp 压力成本均显式。原来源清单、逐日原始衍生物 SHA、资金费出处和项目用量见本地归档 `.agent-state/forward_ops_20261118/s18_breadth_20261119/{protocol,summary,independent}.json`。46 日有 43 日全部可结算，七月三日排名缺口保留。完整持仓盘口覆盖两腿 2 日、四腿 0 日。
- 11/20–26 UTC 四币原生前瞻按日采 06 时与 12–23 时四币文件，并以官方逐时资金费结算。首读次日 01 UTC，次轮源缺口截至隔日 01 UTC 到期后复查。冻结于 11/19 10:25 UTC，演练与单实例任务证据见同目录下 `s18_breadth_forward_20261119/{protocol,freeze,rehearsal,watch-start}.json`。各次来源、失败、范围、哈希和账目另写 Gold 逐日证据与 campaign inventory。
- 历史做市与套利逐盘口、市场、角色比对沿用原有冻结清单和源 SHA；76 个做市方案窗口、192 个套利方案角色窗口。做市的目标额仅计挂单成交，激励为零。保留 19 个复用对照、套利 24 个现金未知、零往返窗口与资金费条数核验首版失败。另按五个月首日期顺序检查先前样本选型。输入哈希与分类计数见 `s18_menu_bound_v2_20261119/{protocol,bounds,chronology-protocol,chronology}.json`。[详细实测与研究出处](../.agent-state/research/NATIVE_FOUR_COIN_BREADTH_AND_FORWARD_2026_11_19.md)

### 2026-11-19 四币信号权重与同风险限额下的做市、套利容量

- 复用 46 个不同日期的 Lighter BTC/ETH/SOL/XRP 原生记录。CryptoHFTData 10/12–25、11/02–15，Goldmine 07/17–27，Tardis 05–11 月首。保持 13–23 UTC、事前六小时排名、310 ms 保护 IOC、20% 可见容量、逐腿十个官方资金费与全成交 1.25 bp 的压力费用。每侧 50 美元，比较等权、中心化排名和受限幅度分仓。276 个历史钱包中 258 个可在端点核算，86 个等权基准对平。七月三个日期留缺，四腿的连续持仓证据仍为零。来源、日期、哈希、资金费与初版登记读回失败分别保存在本地证据 `s19_native_weights{,_v2}_20261119` 及引用的前序目录。
- 独立做市 19 个市场窗按 250/500/750 美元基础报价，保持库存和资金限额；套利东京 BTC 与 ETH 的 W01–W06、两个方向，固定 2.5 bp 门槛与 150 美元风险上限，按 50/100/150 美元基础报价。每个配置保持原十小时窗、预热、排空、队列、费用、资金费完整性和零激励。报价金额、合约规则、盘口源摘要及回放版本在 `s19_quote_capacity_20261119/{mm,arb}/protocol.json`。57 份做市结果全可定价；套利 72 份中 63 份可计价，9 份缺失；两组共有 43 个原设置回读控制。保留零成交及价格路径变化。[数值、成本和外部方法](../.agent-state/research/NATIVE_SIGNAL_WEIGHTS_AND_QUOTE_CAPACITY_2026_11_19.md)

### 2026-11-19 Sprint 17 双端诊断复验、四币持仓与套利规模的时间归因

- 19 个已有 `t50_iso0` 历史双档做市钱包，使用相同原生盘口、资金费、零奖励、风险限额及十小时合约。Linux 源码 112 项、五个 Gemini 文件哈希固定。38 份报告开关对照核实原成交字段与现金，按月首、七月、台湾、W07 汇总撤单原因和撤单期间成交额。数据 hashes、冻结方法与时钟含义存于 `s20_gemini17_peer_20261119/{source,protocol,summary,independent}.json`。全部 147 笔待撤模拟成交的原因与可估值一秒统计覆盖全部金额。
- 四币复用 CryptoHFTData 10/12–25、11/02–15，Goldmine 07/17–27，Tardis 05–11 月首的原生 BTC/ETH/SOL/XRP 时段和原官方小时资金费。46 个日期、六种配置，按 18:00 UTC 可观测共同中价将独立 13–23 UTC 的十小时持仓分段。258 个钱包的两段与原账对平，18 个原缺口保留；中点估值没有撮合。源摘要和各腿资格见 `s20_holding_clock_20261119/{protocol,summary,first,adjacent,july,monthly}.json`。
- 东京 W01–W06 原 BTC/ETH、两场所角色，沿用三档固定报价、2.5 bp 入场、原队列、执行成本与零奖励。72 份报告和 24 个原对照复现，21/24 窗口角色每档可定价。持仓阶段估值只使用已成交库存和各腿可见中价，保留缺口和零往返。源清单、引擎哈希与独立验证见 `s20_arb_capacity_phase_20261119/{protocol,summary,independent}.json`。[归因报告](../.agent-state/research/GEMINI17_REPLAY_AND_HOLDING_CLOCKS_2026_11_19.md)

### 2026-11-19 Sprint 18 逐原因估值与四币择向、双腿基差时钟

- 19 个历史做市十小时原始钱包及 112 项隔离源哈希，复用月首、独立七月、台湾、东京 W07 已登记盘口和原费率。成对 38 次回放，对平 1/5/30 秒待撤成交估值、换手量、笔数与原现金。逐日期剔除复核保留同日重叠。哈希、方法、19 组凭据和所有原因在 `s21_gemini18_peer_20261119/{source,protocol,receipts,summary}.json`。
- 四币 BTC/ETH/SOL/XRP 复用已登记的 CryptoHFTData、Goldmine 与 Tardis 来源、原生到达档、资金费和 1.25 bp 压力成本。46 个已观察日期按同来源前一日已完成的两个独立账本择向，13–23 UTC 十小时，保留 12 个空仓和 1 个择向后不可计价。逐日父哈希、时序可用性与费用沿用原六臂账本。见 `s21_factor_direction_20261119/{protocol,freeze,summary}.json`。[研究动机](https://www.nber.org/papers/w25551)
- 东京 W01–W06 原生两场所 BTC/ETH 套利复用同一执行模型、资金费和三档固定规模。72 个原配置中 9 份有往返，对这 9 份以实际模拟成交时刻与两腿中价核对交叠或单档持仓期间的基差损益，54 个空仓和 9 个未知保留。方法、输入哈希和分组结果存于 `s21_arb_overlap_20261119/{protocol,freeze,summary}.json`。[独立研究报告](../.agent-state/research/SPRINT18_PEER_FACTOR_AND_BASIS_CLOCK_2026_11_19.md)

### 2026-11-19 东京 W09 原生冻结验证、历史四币报价区间与套利退出

- 东京 W09 原始 bulk、crypto、RWA 共 11,354,324,470 字节。源与 E 盘三份归档和清单全量核对，三份 Silver、GPU、准备清单与原冻结账本各有 SHA。原 14 项和独立冻结的 BTC/ETH 套利 16 项、做市 8 项及 BTC 到达率 2 项逐一对平时钟、配置与费用后模拟现金。严格盘口连续性、资金费与 RWA 交易时段证据独立保留。源域与可用性、方法、模板与聚合结果见 `s22_w09_frozen_20261119/{verified,economics}.json`。原件仍属 selection_dev。
- 四币使用既有 CryptoHFTData 10/12–25、11/02–15，Goldmine 7/17–27、Tardis 5–11 月首 BTC/ETH/SOL/XRP 双时点原生报价。六小时收益上下界来自两个端点的 bid/ask；定仓仍用原生执行、1.25 bp 压力与十个资金费。46 个日期全部保留，三天缺锚点，九天存在名次歧义。每腿源哈希与时间条件通过父账本关联。见 `s22_rank_uncertainty_20261119/{protocol,summary,verified}.json`。
- 东京 W01–W06 的 72 个 BTC/ETH 场所角色、三档金额历史配置各复核原结果，再以同一引擎进行一个五秒配对时限实验。完整保留 63 个可计价、9 个未知以及真实的强平和吃单影响。哈希、来源、时间划分、各项费用与全部测试见 `s22_arb_timecap_20261119/{protocol,summary,verified}.json`。[三条线报告](../.agent-state/research/W09_FROZEN_CASH_RANK_AND_EXIT_2026_11_19.md)

### 2026-11-19 原生四币多重检验、零激励成本和账户配额复核

- 来源为 aleph Gold 的东京 W01–W03 v2 消融矩阵、相应的 114 份配置与引擎报告，以及 14 个月首市场窗的 500 美元做市对照。每个钱包单独覆盖 10 小时决策区间，预热及排空另计。对 74 项有现金且有目标所成交的报告核对场内费用、融资、恢复量和目标所分母。38 项未完成、2 项零目标成交。固定费用和按成交比例费用分别计算，激励收入取零。[方法与逐项哈希](../.agent-state/research/V2_COSTS_TRIAL_INVENTORY_AND_QUOTA_2026_11_19.md)
- 四币历史来源为 S18 两侧排名、S19 权重、S21 前日方向和 S22 报价区间，保留原市场映射、时点、原始时窗、缺口和逐层来源哈希。46 个日期中 43 个有共同净现金，7 月 17、18、27 日缺锚点。11 个经济配置按相同压力收益比较，8 块 CSCV 另报最早和最晚 40 日。方法库存追加 185 个内容唯一的协议与一份逐例清单，四个已审计实验族共 316 个配置和行情组合。[输入与统计](../.agent-state/forward_ops_20261118/s23_peer_audit_20261119/dsr.json) · [协议与回放清单](../.agent-state/forward_ops_20261118/s23_peer_audit_20261119/catalogue.json)
- Lighter 官方 SDK 账户限制、账户等级及发送响应仅用于核查字段定义。东京和 aleph 在 15:34 UTC 的目录检查均无配额快照，真实等级和剩余额度未知。[账户限制](https://github.com/elliottech/lighter-python/blob/main/docs/AccountApi.md#account_limits) · [账户模型](https://github.com/elliottech/lighter-python/blob/main/docs/AccountLimits.md) · [响应模型](https://github.com/elliottech/lighter-python/blob/main/docs/RespSendTx.md)

### 2026-11-19 东京 W06 和 W09 双腿报价不可用时长归因

- 取 W06 历史回归与 W09 冻结前向中 Lighter Core 对 Lighter Robinhood 的 BTC、ETH 原生公共行情，逐文件核对原报告记录的 SHA。每个窗口为独立连续 10 小时决策，另计 60 秒预热和 15 秒排空。每市场八个配置共享同一输入。两种独立时钟积分均复现原联合缺口毫秒数，四份行情的缺口都对应至少一腿空簿不可用；在已有可用两腿时，年龄及时间差阈值没有产生额外缺口。此处只诊断输入，不改变费用、成交或资金费假设。逐资产可用性、冻结规则、汇总和原始转换追溯见[盘口缺口复核](../.agent-state/research/PAIR_QUOTE_GAPS_W06_W09_2026_11_19.md)。

### 2026-11-20 双报告核验的公开规则快照与报价约束诊断

- 读取 Gemini 分享正文与本地 Claude 23:00 报告，并以其后 23:52 成本/多重检验审计和当前源码为准。Gemini 未取得最新仓库，其工程现状陈述不能直接当代码缺陷；原分享链接仅留本地。
- 下表是未鉴权官方公开快照，下载时间为 2026-11-19 16:03:31 至 2026-11-19 16:06:01 UTC。仅用于当前规则核查，不倒填旧回测的知识时间；账户实际档位、特定请求豁免和历史生效时刻仍未知。按 ppm÷100 转换为 bp，未消费任何私有账户响应。完整原文、逐份可用时间与哈希清单保存在 Git 外的 `report_synthesis_20261120/sources/manifest.json`。

| 规则源 | SHA-256 | 用途 |
| --- | --- | --- |
| [Lighter volume quota](https://docs.lighter.xyz/developers/reference/volume-quota.md) | `4452ab665a57089a890729c3fae49c6241e6a3d4c9589dfb352bbaeb38ac5df8` | 账户档位、动作消耗、Core/RH 额度市场差异 |
| [Arcus fee model](https://docs.arcus.xyz/concepts/perpetuals/fees.md) | `30605baea69525270657de5cfe563fd051ba9f7fdf245f518fa8b511750f7eb3` | maker-share 分母、钱包作用域、资格生效延迟及返佣封顶 |
| [Arcus latency](https://docs.arcus.xyz/guides/latency.md) | `6f6ae8e11eca21bd354eb15fd1ace2716708cc3837a0f0333e6904ec33e3ac66` | ALO、撤单优先、动态减速带 |
| [Lighter rate limits](https://docs.lighter.xyz/developers/reference/rate-limits.md) | `305e3f2b12c269a435dcc7f9b8e355cd405d44e0ec2856866a244f46fe6138ef` | 请求桶与交易类型限频分别核查 |
| [Arcus public market fee schedules](https://api.arcus.xyz/v1/markets/feeSchedules) | `9c87ba33fef682dd8a0ae49ff13007468535f02f329696465d2edfec78c06338` | BTC 及 SPY/QQQ/GLD/NVDA 条件费表，非账户资格 |

- 同期离线报价诊断复用 Sprint 18 已冻结的 19 个公共原生历史市场窗，来源/市场/时间、资金费和队列沿用父协议。不开新策略参数，只增加诊断开关；每个决策区间连续十小时，预热和排空另列。原始输入、代码版本、对照字节和所有失败在 Git 外追加登记，不将 19 窗相加冒充连续 190 小时。新工具不改变原有盘口缺口或未知净现金。见[研究与工程边界](RESEARCH_SYNTHESIS_AND_QUOTE_CONSTRAINTS_2026_11_20.md)。

### 2026-11-20 东京盘口刷新到恢复的原始时钟、四币历史收益集中度及做市钱包匹配

- W06 回归和 W09 已读数的冻结前向中，核对 Lighter Core 和 Robinhood 的 46 个公开盘口源分片、归档清单与 BTC/ETH 四份回放哈希。原始可用纳秒时间向上取整到毫秒，按窗口及市场分别匹配刷新、断连、订阅快照与回放发布帧。两窗为各自的 10 小时决策期，预热排空另计。另在保留实测恢复耗时的条件下检验 Robinhood 计划刷新提前 120 秒的固定情景。原始墙钟、回放帧、两腿同时缺簿和至少一腿缺簿四种统计分别存档。[输入、归因与局限](../.agent-state/research/BOOK_REFRESH_AND_ZERO_REWARD_WALLETS_2026_11_20.md)
- 四币以已有 43 日共同可计价压力收益作逐日、逐月剔除，保留三个缺锚点日期及 11 个经济配置。19 份已完成报价约束对照按配置和原基准输出哈希与原做市钱包匹配，继承原场内交易费用、融资和压力附加成本，奖励收入为零。结果仅作历史归因，未生成新策略变体。[逐窗与逐月账本](../.agent-state/forward_ops_20261118/s24_window_sensitivity_20261120/summary.json)

### 2026-11-20 报价到成交关联与单一放置余量研究

- 继续使用 Sprint18 的19个已登记公共回放带、原配置/费用/资金费/队列模型，
  不引入私有成交。原决策标签随模拟派单、接单、部分/迟到成交保留；source与
  receipt时钟不互换，原1/5/30秒标签缺失规则不变。阶段1为只读历史诊断，
  38次回放对平原报告；汇总SHA-256
  `421c64f934833a9fde78643144f6bfc8a1f9d21bc58e9b8cac07be9c762a277b`。
- 唯一保护幅度由2026-05-01 BTC/ETH R0095公共本地簿推导：原到达时钟及
  market-data-delay，1秒网格ASOF可用中价，簿龄≤1000ms，缺失不跨段；各取绝对
  1秒收益率nearest-rank q90，取大后向上到0.1bp，结果0.7bp。
  BTC带hash `5381d6f17fa4494aaa0abddb42aa85853058fb8eb63e9cb2ca4877e19cc37bc8`，
  ETH带hash `0348cb49005d50ffb82609a46668dce90012c9b47dfc26058a4c792f78923cb3`。
  决策区间均为2026-05-01 01:35–11:35 UTC；BTC/ETH各35999/35977个相邻标签，
  后者缺11个采样点。它们只是校准标签，不能当独立交易样本。
- 同日期四窗仅作训练日回归，其余15窗作较晚历史选择评估，均非untouched。
  时间/输入/hash/参数/可用性/失败及转换规则追加保留在Git外
  `bound_guard_20261120/{variant_design.json,calibration.json,variant_parameters.freeze.json}`
  和campaign/methodology库存。未改已有前向配置。详见
  [关联与验收规则](BOUND_BREACH_LINKAGE_AND_GUARD_2026_11_20.md)。

### 2026-11-20 实际库存周期与结清现金归因

- 复用前项19个公共历史市场窗及既有control/已淘汰0.7bp两臂，原输入域、市场、
  tape hash、费用/资金费/队列与可用时钟沿父协议。各10小时决策区间另列60秒预热、
  15秒排空，4训练日回归/15较晚历史选择标签不变，零新经济变体。
- 转换：模拟venue真实e8净仓划分新开/增仓/部分平仓/全平/穿零；穿零按实际量
  拆开旧/新仓，FIFO费用按量分摊，按两臂共同空仓边界比较完整周期。接收已知
  中价建立成交价差/持仓漂移会计桥；原空簿与过期/缺失后验标签不补造。
  原始账户/订单标识不入输出，只保留模拟cohort及周期聚合。38次原账本逐字节
  不变；结果摘要SHA256 `b236f0ef1f997fc9b93ee4a19136f3fec935837049b37d405538b329ba62ac10`。
- 逐源时间、hash、转换和失败追加于Git外
  `inventory_cycles_20261120/source_inventory.ndjson`、方法及campaign库存；
  [分析与限制](INVENTORY_CYCLE_CASH_ATTRIBUTION_2026_11_20.md)。

### 2026-11-20 固定短期盘口预测验收

- 消费同一19个公共历史源窗口及S20原配置，逐带和配置hash核对。每窗仍有连续
  10小时决策源区间，预热60秒/排空15秒另列；5月BTC/ETH R0095各前6小时训练、
  后4小时预测验证，同日另两窗回归、其余15窗历史选择。短分区不作交易盈利
  验证，不改变原19个钱包，也不增加经济策略变体。
- 1秒接收时钟ASOF的L1对侧数量微价偏移/前1秒动量，标签取1秒到期后源时间
  合格的首个簿，最大年龄和额外延后均1000ms。空簿、子网格陈旧间隔、源时钟
  回退、跨分区及缺标签全部明确计数；保持market-data delay，不补造交易流
  连续性、执行深度、费用或资金费。训练集独立标准化，固定两特征ridge0.001。
- 仅生成模型及聚合评分，不输出原始成交。新增1个预测模型试验、0个经济变体，
  BTC验证门槛失败后停止。源/域/市场/时窗/哈希与变换清单位于Git外
  `short_forecast_20261120/source_inventory.ndjson`；结果汇总SHA256
  `2a647a5e86290e095438d2683d1e829223085e73506bc21a5a8dd96b1c10f5be`。
  [结果与局限](SHORT_HORIZON_FORECAST_AUDIT_2026_11_20.md)。

### 2026-11-20 东京恢复后价差、四币逐资产归因及做市保护成本复核

- W06/W09 共 300 次 Core 计划刷新，BTC/ETH 共用原始事件。两种观察时点共 1,200 条市场样本，冻结原生盘口、整份编译输入、历史费率及等值记账假设，按双方到达时间、数量和反向退出计算模拟价差及缺口。另以冻结回放中 43 个共同可计价日期的六个四币钱包复算逐腿现金，独立登记 11 月 20–26 日已冻结四个前向钱包的逐币报告。既有 19 对 0.7 bp 做市钱包按原分组复核费用敏感性。上述三个项目各自使用原资金、成本及缺口定义、零激励；每个参与单元的原始时钟、来源哈希和计量限制见[研究原件及回执](../.agent-state/research/REFRESH_SPREAD_ASSET_AND_GUARD_2026_11_20.md)。

### 2026-11-20 — Native month-end coverage, matched Core/RH depth and delay, and forecast review

- Six consecutive month ends (May–October 2026) used CryptoHFTData Lighter BTC, ETH, SOL, XRP at 06 UTC and 12–23 UTC on eligible dates. 21 of 24 anchor objects and all 48 October holding objects were independently hashed, 177,092,828 bytes in total. Three anchor objects returned 404. All earlier dates lacked a four-market exchange timestamp, with predecessor IDs also absent before September. October 31 priced four original 13–23 UTC wallets using 310 ms IOC, 20% depth, ten hourly funding slots per leg and 1.25 bp stress, with incomplete held paths. Registrations, daily source receipts, fields, funding and leg cash are indexed in Gold. [Publisher sample and schema](https://www.cryptohftdata.com/datasets/lighter-orderbook-data)
- W06/W09 recorded Core–RH BTC/ETH retain the source clock, 1,200 market/event units, actual native quote currencies under the frozen USD-parity convention, and original fee model. Reconstructed touch controls precede sweeps, capped depth, paired 0/10/310 ms arrival comparisons and matched $100/$500 studies. The public tape hashes, event digests, original gaps, execution assumptions and saturated/depth-limited counts are retained under `depth_sleeve_20261120`.
- Balanced pair cash across the original 46 dates and three weight schemes uses original native executions and funding, accruing separately to momentum and reversal wallets. Four equal-budget wallets on 20–26 November now have pre-warm-up paired attribution. The independent forecast audit reconciles 19 historical windows to the frozen model and cohorts; 15 later market windows share nine UTC start dates. The original BTC validation stop stands.

### 2026-11-20 — Native three-day transfer, Tokyo cross-check and cost of closing inventory

- Lighter BTC, ETH, SOL and XRP used 156 native CryptoHFTData objects dated November 16–18, 2026, totaling 1,012,209,994 bytes. Each date used 06 UTC rank anchors plus 12–23 UTC holdings. Receipt and object hashes were independently re-read. Ranking, 310 ms IOC, quantity, hourly funding, 1.25 bp turnover cost and original timestamp rules follow the November campaign. Four wallets per date settled. No wallet had a complete mark path. A separately hashed Tokyo Lighter Core BTC/ETH tape verified 226 additional four-asset seconds on November 17, with 185 remaining missing. Original wallet quality classes are preserved. [Publisher schema](https://www.cryptohftdata.com/datasets/lighter-orderbook-data)
- The original W06/W09 BTC/ETH Core–RH book and currency inputs were reused for one-second, sixty-second and causal first-break-even closes on every selected entry with a fixed 1.25 bp taker-turnover stress. Every original decision, unavailable opportunity, sweep, archive hash, time range and funding/end buffer is retained in the Gold event inventory. Across the two W09 BTC anchor clocks, none of 202 signals had a recorded stressed break-even exit within the allowed minute. Historical selection diagnostics.
- Nineteen ten-hour maker windows, two existing arms, 38 pairs of engine/config reports and 158 completed inventory episodes use the original input hashes, execution fees, liquidity-role volumes, 1/5/30-second markouts, 1 bp taker allowance and 0.25 bp all-volume allowance. The failed diagnostic-hash comparison and the successful byte-for-byte audit are both registered. Per-source receipts, transformations, gaps, results and fee conventions for all three studies are in [the campaign report](../.agent-state/research/CONVERGENCE_EXIT_NATIVE_PATHS_AND_MAKER_CYCLES_2026_11_20.md).

### 2026-11-20 — Four-market native calendar, asynchronous Core/RH quotes and maker inventory time

- Lighter BTC, ETH, SOL and XRP used independently initialized 13–23 UTC wallets with 07–13 UTC signals, original protected 310 ms IOC, 20% depth, funding and 1.25 bp all-volume cost. Eight September/October calendar dates added 128 native CryptoHFTData objects. The subsequent fixed 44-date calendar covers October 6 through November 18, using 35 prior date receipts and 468 objects (2,184,498,534 bytes) across the remaining nine dates. The initial calendar uses 43 CryptoHFTData dates and one Tardis date. A separate 52-object two-recorder cross-check repriced November 1, confirming the same signed targets and a change below USD 0.001 per wallet. Every new archive and receipt was hashed and existing per-date ledgers were checked. Same-row coverage of transaction time and both sequence identifiers was separately measured across 100 physical objects (346,819,064 bytes). All 24,267,173 updates before 16 UTC within the October 5 06–15 UTC sample lacked transaction time; all 21,114,612 subsequent updates in the sampled October 5 16 UTC to October 6 06 UTC hours had the three fields together. Historical publication times of the objects are unverified. [Publisher wire format](https://www.cryptohftdata.com/docs/wire-format-lighter)
- W06/W09 Core–RH BTC/ETH kept the original 1,200 market/time observations, touch-selection rule, $100 reference, quantity, model quote-currency parity and 1.25 bp taker turnover cost. Four per-venue 10/310 ms combinations were compared with the original simultaneous observation controls, using independent source clocks, age, depth, lot and funding/end buffers. Of 202 W09 BTC entries, 74 pairs share a refresh, yielding 128 unique refreshes. Book timing is available; venue execution confirmations were not recorded in these simulations.
- Nineteen ten-hour maker market-windows, two arms and 158 closed inventory episodes reuse the original source hashes and execution fees, with 1 bp taker and 0.25 bp all-volume allowances. Exposure time (contracts times milliseconds) reconciles independently against matched FIFO lots, and the long and short sides are scaled only within each market and window for attribution. Historical and forward stage observers, per-date or per-hour inputs, custody and funding receipts, failures, frozen script hashes, identical-date benchmarks and the complete week sensitivity are listed in [the campaign report](../.agent-state/research/NATIVE_44_DAYS_INDEPENDENT_LEG_CLOCKS_AND_INVENTORY_2026_11_20.md).

### 2026-11-20 — 决策价差动态定仓、受保护双 IOC 与做市开窗风险

- 四币 44 日同源 CryptoHFTData 公开盘口、十个逐时官方资金费和原 Rust 信号继续绑定到逐日哈希。13–23 UTC 各日期独立运行，13 时决策盘口与前七个日历日决定每腿 15.11–25 美元的实际目标。两个方向各有 36 个共同定价日期，保留预热与持仓缺口。固定、当日与滞后目标使用相同深度、手数、310ms 时钟及 1.25bp 费用。另以 7 月 17–27 日独立 Goldmine 全部 11 日原生盘口复核，7 月 26 日独有可用当日预算，7 天缺预热且 3 天缺原始排名锚点。登记、逐日来源与钱包收据、供应商日期、现金与手数分解见 Gold 的 liquidity_budget_20261120。 [原生记录器](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026)
- 做市使用 19 个已登账的原生 Lighter 市场时窗、原训练/选择角色、评分器与原费用，单独记录各磁带、配置和钱包哈希。开窗风险只用开仓前 31 个一秒盘口，训练数据为五月的 BTC、ETH 各两窗，SOL/XRP 没有同市场门槛。套利沿用 W06、W09 两场所 BTC/ETH 的四份原生公开带和原 1,200 个观察，203 个选中信号，模型时钟 10/10 和 310/310 ms，保护价按决策盘口，同时将深度不平衡与未知平仓分别留档。原费率、时序边界、模型假设、方法哈希和逐事件档案均通过各自冻结合同与来源清单关联。[协议与完整结果](../.agent-state/research/DECISION_SPREAD_SIZING_PROTECTED_ENTRIES_AND_MAKER_RISK_2026_11_20.md)

### 2026-11-20 — 两地反向盘口估价、固定时钟检验与共享做市运行费

- 东京 Core/RH 的 BTC、ETH 两地公开盘口覆盖 W01–W06、W09 的 14 份原生带。沿用既有冻结源、市场顺序和配置，各原带文件的 SHA、原可用时钟及范围登记在 Gold 的 execution_hurdle_20261120/clock-protocol.json。每市场 10 小时取 300 个固定时点，另按 W06、W09 原刷新时钟做 1,200 个观察。以事前 60 个一秒盘口的可见反向深度计算结清预测，源时间不晚于接收、接收不晚于估值，至少 48 点有效。固定 $100 单腿参考、两地排他抵押、USDC/USDG 平价及额外 1.25 bp 全成交额成本；保留未平仓和空仓。逐份市场账本、缺口、可计价状态及复核结果均绑定 Gold 源哈希。[方法和结果](../.agent-state/research/ROUND_TRIP_HURDLES_AND_SHARED_MAKER_COST_2026_11_20.md)
- Lighter 原生四币沿用 44 天 CryptoHFTData 和七月 11 天 Goldmine、原官方资金费与每腿 25 美元执行，七个日历日同记录器历史估计准入。所有缺口和未选择交易保留。做市 19 个原钱包中的后 15 窗按 BTC/ETH/SOL/XRP 时钟并集核算共享运行费用，比较原策略与已研究的开盘过滤，保留市场分组和每百万挂单成交额。W06–W10 的公开文件逐档以两代格式交叉签收，五窗三端完成整件回读。[公开来源与估计口径](../.agent-state/forward_ops_20261118/execution_hurdle_20261120/cost-protocol.json)

### W10 native-source recovery and zero-reward validation, 2026-11-20

- The Tokyo W10 crypto, RWA and bulk source identities, publication windows and availability clocks are recorded in the original Bronze manifests. The three compressed archives total 15,616,868,281 bytes with verified cold storage and Drive readback. The complete RWA normalization rejects nonpositive book sequences. Hash review of 36 native market-hour shards found 1,273 empty zero-sequence book snapshots in Lighter Core market 139. The separate partial RWA projection withholds the whole channel, including its other 61 snapshots. BTC, ETH and the NVDA, QQQ and SPY inputs for the original scoring use their respective unchanged native archives, time rules, costs and frozen templates. The complete RWA Silver and combined GPU remain unavailable. Sources and market IDs, hash-addressed shards and gaps are indexed under `w10_source_recovery`. [Per-source audit](../.agent-state/forward_ops_20261118/w10_source_recovery/source-audit.json) · [40 frozen configurations](../.agent-state/forward_ops_20261118/leg_clock_20261120/checked-w10.json)
- Three explicit BTC Core public trade gaps in the Tokyo decision clock totaled 3.209 seconds. Hashes, reconnect snapshots and delta counts were reviewed against the CryptoHFTData BTC public trades at 13 and 18 UTC on November 19. Lighter's official daily trade archive returned HTTP 404 at the first check. The independent recorder provides historical venue and receipt clocks. A later snapshot cannot establish receipt by a Tokyo strategy during an outage. A separate comparison uses the same Tokyo recorder's strict crypto Silver for BTC book and lifecycle transitions. Original wallet outputs, stale books, missing settlement boundaries and uncovered equity session metadata retain their frozen dispositions. [Public source availability and hashes](../.agent-state/forward_ops_20261118/w10_source_recovery/print-sources.json) · [Per-pause comparison](../.agent-state/forward_ops_20261118/w10_source_recovery/book-clock.json)
- The four-coin study reuses 44 CryptoHFTData dates, 11 July Goldmine dates and seven month-first Tardis dates. November 1 appears in two recorder cohorts. For 61 distinct dates, the four previous-day 14–23 UTC Core funding series supply 2,440 distinct asset-hours from 56 receipts. Cash uses original native ranks, 310 ms protected IOC endpoints and actual ten-hour funding. Historical delivery time at the decision is assumed. Maker and arb each select a size from actual same-tape wallet runs using pre-open quotes. All missing terminal amounts, spending assumptions, source versions and the initial decimal string comparison failure remain recorded. [Original vendor and funding bindings](../.agent-state/forward_ops_20261118/funding_clock_20261120/carry-inputs.json) · [Two ticket studies](../.agent-state/forward_ops_20261118/funding_clock_20261120/ticket-plan.json)

### Tokyo BTC clock recovery at market level, 2026-11-20

The W07–W10 frozen crypto decision intervals contain 598 scheduled Core clock resets. Every reset could be linked by increasing recorder sequence to a subsequent fresh BTC book. The W08 input is its separately verified crypto projection. The other three windows use the canonical crypto partitions. The source manifest, partition hashes, time bounds and threshold of 2,200 ms are recorded per window. Six resets took over 30 seconds, including five in W10. The Tokyo public book worker logged two read timeouts before the 62.137-second recovery and one before the 31.078-second recovery implicated in eleven W10 wallet stops. Only coarse error classes and relative offsets were retained from the service journal. The method for W11–W16 was registered while W11 was recording and reproduced all 148 W10 episodes. [Four-window source and method](../.agent-state/forward_ops_20261118/w10_source_recovery/refresh-windows-plan.json) · [Cross-window results](../.agent-state/forward_ops_20261118/w10_source_recovery/refresh-windows.json) · [Receipt-clock comparison](../.agent-state/forward_ops_20261118/w10_source_recovery/connection-review.json) · [Forward registration](../.agent-state/forward_ops_20261118/w10_source_recovery/refresh-forward-plan.json)

Eight distinct W10 nonterminal pause times share six BTC native updates that became available between the last emitted pair frame and the decision. The preceding frames held valid Hyperliquid books. The subsequent frames carried valid local books. Tokyo receipt and availability, venue publication, the per-market probe interval, and the 100 ms sampling configuration were checked separately. The first and next pair frames serve different temporal comparisons. The six native updates remain tied to their original availability and have no inferred execution outcomes. [Original shards and compiler](../.agent-state/forward_ops_20261118/w10_source_recovery/frame-witness-plan.json) · [Per-decision results](../.agent-state/forward_ops_20261118/w10_source_recovery/frame-witness.json)

Five hash-verified CryptoHFTData BTC order-book hours cover all six long recovery intervals. Across the two W10 recoveries associated with frozen wallet halts, the independent feed recorded 1,232 and 609 distinct venue update IDs, respectively. It received 1,229 and 606 of them before the corresponding Tokyo books recovered. These external timestamps provide an independent activity check, with each hour, receipt, adjacent five-second control and observation rule preserved. [External book sources and timing](../.agent-state/forward_ops_20261118/w10_source_recovery/independent-books.json)

### 2026-11-20 前一日资金费、同市场可用现金与独立盘口样本

- 同一 Lighter Core 四币组合以开盘前 14 小时以前的十次官方结算比较多空方向，沿用 CryptoHFTData 44 日（10/06–11/18）、Goldmine 11 日（7/17–27）及 Tardis 七个月首。逐日行绑定原生盘口、资金费、币种、来源、缺口和两种方向。56 份来源回执生成 2,440 个不同币种小时，11/01 的跨来源重复独立保留。全持有期有缺口的可结算日仅报告终点现金。做市与套利复用同市场成对的 15 及 24 个原账本，冻结后逐腿核对实际结束与排空时钟、原盘口与费用。来源、派生及失败已归档于 aleph Gold 的 funding_rank_20261120 和本地研究报告。[冻结盘口及资金费清单](../.agent-state/forward_ops_20261118/funding_rank_20261120/protocol.json) · [同市场回放清单](../.agent-state/forward_ops_20261118/funding_rank_20261120/prior-ticket-plan.json)
- 0xArchive 在 Hugging Face 发布的样本固定至版本 68ee22d74620384c64c00f01747c97ba2d32e076。Lighter Core 的 14 个 BTC/ETH 日档覆盖 11/09–15，共 15,798,800 字节、120,946 行，逐件匹配发布者 Git LFS SHA-256 与大小。前十档每十秒一次，仅含单一 timestamp。用于核对 13:00–23:00 UTC 中价变化；无法重建 310 毫秒保护 IOC 或独立接收时钟。该版本公开价位样本只含两个币和这七日，原生可执行四币月份没有因此增加。DeepTick 当次公开目录请求 HTTP 503，保留待复核状态。[发布者原版](https://huggingface.co/datasets/0xArchive/hyperliquid-lighter-market-data-sample/tree/68ee22d74620384c64c00f01747c97ba2d32e076) · [文件哈希与核对规则](../.agent-state/forward_ops_20261118/funding_rank_20261120/book-sample-plan.json) · [逐日价格核对](../.agent-state/forward_ops_20261118/funding_rank_20261120/book-sample-results.json)

### 2026-11-20 四币相对现货强弱、外部价格变动与订单规模

- Binance 官方 BTCUSDT、ETHUSDT、SOLUSDT、XRPUSDT 现货一秒 K 线按 61 个独立 UTC 日期取 244 个日档。五月至十一月的选定日期共 436,568,106 字节，ZIP 和逐档发布者 CHECKSUM 已在 aleph Gold 留存。取 07:00、13:00 UTC 截止前至少一秒、最多十一秒内最近一个有真实成交量和成交笔数的收盘价，与 Lighter 原生同币两时点中价计算六小时相对变动。原生盘口为 44 个 CryptoHFTData 日期、11 个 Goldmine 日期和 7 个 Tardis 月首，11/01 两家记录器分别保留。归档时间为微秒，缺行情的来源日、成交到达假设、源和宿盘口哈希、原费率和尾仓均单列。[Binance 数据和字段](https://github.com/binance/binance-public-data) · [逐日来源与可用性](../.agent-state/forward_ops_20261118/cross_spot_20261120/spot-sources.json) · [四币结果](../.agent-state/forward_ops_20261118/cross_spot_20261120/results.json)
- 做市 15 窗与套利 24 个角色窗复用同币种日档中下单决策前 61 和 1 秒时点以前已完成且含成交的秒线；历史到场采用一秒滞后假设。以 2.5 bp 的绝对变动判定使用原大档或半档，各自保留回补、费用和终值缺口。独立解析器核对 566 个时点，244 个归档全部复核 SHA-256，原版重放源与本次衍生同时登记于 aleph 的追加式 campaign inventory。[单量规则与实际逐窗选择](../.agent-state/forward_ops_20261118/cross_spot_20261120/ticket-results.json) · [逐档复核](../.agent-state/forward_ops_20261118/cross_spot_20261120/source-readback.json)


### 2026-11-20 — BTC 残差排序与固定路径成本承受能力

- 重用已签收的 Binance BTCUSDT/ETHUSDT/SOLUSDT/XRPUSDT 61 日、244 个官方一秒现货 ZIP，436,568,106 字节；原公开 URL、CHECKSUM 和归档 SHA 保持原 cross_spot_20261120 来源链。新增变换仅取 00:01–06:00 UTC 360 个一分钟 ASOF 锚点，最后实际成交至少滞后一秒、最多再老十秒；359 个相邻收益至少288对有效，无缺口跨接。第二 CSV 解析器核对87,341个有效价格，Decimal回归与原系数最大差异1.3923e-13。历史一秒发布时间为假设，未被解释为实际接收时钟。
- 原生Lighter 07–13 UTC残差排名、13–23 UTC独立钱包使用原44日CryptoHFTData、11日Goldmine、7个月首Tardis及原官方资金费/执行腿哈希。62来源日期代表61独立日期，11/01跨源重复保留；59可结算、3原生锚点未知，仅10/29四币持有路径覆盖完整。全组合回撤仍未知。11/08通过原S19→S18哈希链补回父级未携带的模拟腿账，其余61日期逐字不变；旧版本保留，非新经济变体。
- 做市15窗、套利24角色窗仅重核原大档账，maker0.25bp/taker1.25bp额外费用与固定运行费分列，套利3个未结终值不当成零。协议、修正、源哈希、两经济臂和所有未知已追加到本地methodology inventory与aleph campaign inventory。完整公开输入仍在aleph Gold；本地只取回不含日志、价格样本和逐腿账的已审查汇总。[方法、结果与来源](RESIDUAL_SIGNAL_AND_CASH_FEASIBILITY_2026_11_20.md) · [最小结果凭据](../.agent-state/research/residual_signal_20261120/v2/reviewed-export.json)

### 2026-11-20 双腿退出时钟、基础报价金额上限与残差方向的周度复核

- W01–W06 原生 Lighter Core、RH 的 BTC、ETH 十小时回放共 72 个独立角色与金额钱包。复用 S19 原配置、S20 原现金与 S21 来源绑定，核对三个活跃事件的三份原生盘口和九份模拟报告。逐快照时间以原回放接收时刻为界，取最后一个同毫秒更新，保留交易所时间、时效、配对时差和整量可成交深度。对冲形成到原首次减仓之间可定价 49,059 个钱包毫秒。原开仓手续费按账，所有成交另计 0.25 bp，吃单再加 1 bp，无额外激励。W09、W10 的四个原冻结基准角色各自绑定 40 项签收、市场盘口 SHA 与原账，七个已结周期含分笔或强制退出，保留报价缺失。持续观察 W11–W16 时按来源首次到达和本次登记的先后分别标注。[原始三档结果](../.agent-state/forward_ops_20261118/paired_exit_depth_20261120/summary.json) · [W09 分笔归因](../.agent-state/forward_ops_20261118/paired_exit_depth_20261120/partial-w09.json) · [W10 分笔归因](../.agent-state/forward_ops_20261118/paired_exit_depth_20261120/partial-w10.json)
- 四币周度复核使用同事残差 v2 的 62 个来源日、61 个独立日历日与原始 UTC 时钟及资金费。10 月 6 日至 11 月 18 日的 44 个完整结清日按七个 UTC 自然周逐块剔除，保留三次七月锚点缺失与持有路径缺口。做市复用 S19 对同一 BTC、ETH 盘口的 250、500、750 美元三个实际回放账本，按 15 窗的 11 个开窗时刻组合，重叠营运时段取并集，成本基准与被动成交额分列。原始来源和方法文件在计划内有 SHA-256，变体、失败尝试、模拟时钟、封窗状态及最终聚合在 aleph 追加式 campaign inventory 留存。[来源和分组规则](../.agent-state/forward_ops_20261118/paired_exit_depth_20261120/peer-plan.json) · [完整结果与限制](../.agent-state/research/PAIRED_EXIT_DEPTH_AND_SIZING_BOUND_2026_11_20.md)

### 2026-11-20 原生四币 11/19、早期订阅快照、双腿顺序退出与做市开窗

- CryptoHFTData 2026-11-19 的 Core BTC、ETH、SOL、XRP 原生 06 UTC 锚点四档和 12–23 UTC 持有区间 48 档全部签收，共 404,773,839 字节。两个原 URL 初次超时后重试成功，按原 07/13 UTC 六小时排名、310 ms、3 bp 保护、单档 20%、原市场手数和逐币十期官方资金费执行。四个十小时钱包均可核对端点现金，持仓路径均有缺口。W10 Tokyo crypto 只含 Core 市场 0、1、2，逐秒取价同时要求原交易所时钟、回执时钟、可见时间、2.2 秒时效和流重置规则。在四腿缺失的 152 秒中，122 秒能在独立记录器验证相应缺失腿的双边报价。已冻结面向 W11 签收的后续复核。[日期、方法及缺口](../.agent-state/research/NATIVE_NOV19_AND_SEQUENTIAL_EXIT_2026_11_20.md) · [归档与资金费来源](../.agent-state/forward_ops_20261118/receipt_clock_20261120/catalog.json)
- 逐件重读冻结清单中的 67 份一至四月 CryptoHFTData 小时原件，共 166,474,068 字节。按接收时刻及更新序号聚合订阅快照；259 个双边快照，0 个具备交易所交易时间，21 个不同日期/小时组合中没有一个四币快照同时满足 2.2 秒接收时效。来源登记区分已签收样本、缺内容和供应商访问失败，覆盖不外推至整月。[发行方格式与快照](https://www.cryptohftdata.com/datasets/lighter-orderbook-data) · [逐件冻结清单](../.agent-state/forward_ops_20261118/receipt_clock_20261120/early-manifest.json) · [SQL 复验](../.agent-state/forward_ops_20261118/receipt_clock_20261120/early-check.json)
- W06/W10 两个 BTC 历史触发复用已签收 Core/RH 原生盘口、原始被动开仓和资金费边界。以逐场所接收时钟模拟第一腿获得回执后才发第二腿，无额外限价和固定触发价限分别核算，单腿未结金额保留未知。第二腿零显示成交后的一次回补另有事后预登记。做市固定 500 美元原钱包只改变开窗启停，按原 15 个独立钱包及 11 个开窗时刻核对全时和按量计费。[套利时钟与成本](../.agent-state/forward_ops_20261118/receipt_clock_20261120/serial-plan.json) · [做市预登记](../.agent-state/forward_ops_20261118/receipt_clock_20261120/maker-plan.json)


### 2026-11-20 配对保本报价的完整撤单与逐腿退出验证

- 复用已签收Lighter Core/RH BTC、ETH的16份原生回放带，W01–W06及W09/W10全部32个100美元角色钱包；W06 BTC角色1的150美元已知触发只作额外诊断。每钱包60秒预热、连续10小时决策、15秒排空，原始配置、带、报告及W09/W10签收哈希写入新协议；没有新采集或重编译盘口。全部为历史复用selection_dev，不称untouched holdout。
- 转换只增加一个默认关闭的整周期保本估值触发：模拟费用后，全成交0.25bp及taker额外1bp；触发后原取消、迟到成交、通知及分腿保护IOC规则原样执行。33个原基准逐字节一致，31个未触发变体只多诊断字段。两次报价正值均在完整退出后为负；主样本仅W10改善0.000574785美元，仍亏；150美元诊断恶化0.014746288美元，不晋级。
- 16份带均有原双腿报价缺口，单份12.408–443.755秒；成交流停顿独立记录，3个原资金费/终态未知不补零。新覆盖重算与W09/W10原回执一致。两路径22个已结周期按阶段现金和共同平仓区间核对；币种等价、原费率/延迟假设和无对手方反应的限制保留。单一经济臂、所有来源/配置哈希、失败及结果已追加methodology与aleph campaign。只回读不含日志、价格样本或逐笔账的161,705字节汇总。[完整报告](PAIRED_CASH_EXIT_AUDIT_2026_11_20.md) · [协议及最小证据](../.agent-state/research/paired_cash_exit_20261120/reviewed-export.json)

### November 20 native four-asset controls, five-month clock availability and paired exit attribution

The November 20 exit attribution registers two prior simulated BTC Lighter Core/RH paired exits in W06 and W10. Both independent observer builds reproduced both complete original report hashes. The source binds the frozen variant, input tape, engine and per-window protocol. Receipt clocks, modeled arrivals, source book timestamps and ledger status are distinct. The original stress cost was 0.25 bp on all fills plus another 1 bp on taker fills. Both replays had a resting maker quote at the decision and waited 2,600 ms of grace after the modeled cancel before a later event released it. Conditional remaining-position depth marks were negative at first submission in both cases. The two documented cases are selected historical diagnostics.

Six fixed $25-per-leg long/short assignments used all 63 recorded historical source-date units for the native BTC, ETH, SOL and XRP six-hour rank through November 19. Sixty units had complete matched terminal cash for both signs; three Goldmine July anchors were unavailable. The 22+22 single-provider split had one fully continuous four-market ten-hour path. Original protected delay of 310 ms, 20% book participation, 3 bp collar, market terms, hourly funding, and 1.25 bp traded-notional charge are inherited. Fixed identity controls remove net directional terms in their equal average. Separately, a preregistered fourth-Thursday CryptoHFTData 06 UTC check from May through September 2026 received 17 of 20 native market-hours (33,324,762 bytes). Every received incremental-update archive lacked exchange transaction times and predecessor update numbers. May 28 BTC and September 24 ETH and XRP returned 404. No new ten-hour wallet could be priced. July 23 overlaps a previously signed Goldmine date. The source inventory retains every market, URL, hash, failure, publication rule and historical cohort. [Publisher's native order book archive](https://www.cryptohftdata.com/datasets/lighter-orderbook-data)

For independent zero-incentive maker wallets, all 4,194,304 assignments of off/$250/$500/$750 at the 11 common openings were compared with both pre-existing restricted menus, the same 15 baseline wallets, a $9,604.51 turnover floor and the fixed 96.25-hour reservation. The source binds each size-specific report to the signed input and market interval. No assignment cleared that volume floor with positive cash. The per-wallet tapes, known gaps, aggregate controls, dates, and SHA references are recorded under the local/aleph campaign directory `exit_attribution_20261120`.

### November 20 two-day native ranks, flat-checkpoint maker pauses and sequential exit clocks

The four-asset Lighter reversal used 63 recorder-date units covering 62 calendar dates, with native CryptoHFTData, Goldmine and Tardis histories identified by the upstream campaign manifests. BTC, ETH, SOL and XRP signs from 07 to 13 UTC were compared with the prior calendar day. The October 6–27 and October 28–November 18 historical cohorts had 21 and 22 usable paired decisions. The selected legs retain $25 per asset, 310 ms protected arrival, 20% displayed participation, 3 bp collars, original hourly funding and a 1.25 bp cost on all fills. The matched-size check compares against the mean of all balanced pairs. Missing source and ten-hour path coverage are saved separately from terminal cash. The six preceding month ends May through October and newly checked April 30 supply 25 of 28 original CryptoHFTData 06 UTC market-hour books. October 31 alone had all four qualifying native anchors. The seven May–November Tardis month starts are historical checkpoints.

An independent reference for the same seven month starts uses previous-day Lighter official normal trades in the last five minutes before each cutoff, alongside CryptoHFTData public trade receipts. Seven official daily archives and 45/56 recorder hours (6,823,091 compressed bytes) were verified. Only 37/56 official reference trades matched the receipt-clock requirement. No month had all eight eligible points. Six months had all event-only prices under an explicit prior-publication assumption. The month/day/market/UTC hour, archive URLs, file and receipt hashes, missing objects, identification failures and clock rule are registered in the WSL campaign signal_persistence_20261120. The native-prior-results, monthly-sources and monthly-results manifests retain both successful and incomplete source paths.

Nineteen independent zero-incentive maker controls were reproduced with original report hashes. A single prospective-in-replay change pauses the next funding interval on a loss observed at a verified flat checkpoint. All 19 modified reports were reproduced independently for FIFO attribution and 1/5/30-second markouts. The initial scorer failed on an unpublished status field. Its amended verification uses terminal state, certified checkpoint counts, tape coverage and cash from the published report and preserves the original experiment. Cost includes venue fees, 0.25 bp on all notional, another 1 bp on taker notional, and the original fixed operating reservation. The paired-exit diagnostic uses the original 16 historical inventory intervals, modeled order latency, full displayed depth, conditional confirmation and a terminal funding or reducing-fill cutoff. Two independent calculations compared 8,186 eligible clocks per timing assumption, including unknown legs. The campaign binds original tape hashes, protocols, costs, asset identity, per-arm outcome and independent review in maker-results, quality-results, arb-results, arb-check-results and review-results.


### November 20 sealed trade clock lineage and frozen replay counters

Sixteen already signed Lighter Core/RH BTC/ETH tapes cover W01–W06, W09 and W10,
with separate 60-second warm-up, ten-hour decision interval and 15-second drain.
The historical diagnostic matches 2,880,905 public projected trades to timing
counts in 32 original $100 simulated wallets and a separately identified W06 BTC
$150 diagnostic. No economic variants or independent holdout samples were added.
Original fees, funding, cash, source gaps and availability rules are inherited
without modification from the paired_cash_exit_20261120 parent protocol.

Before inspecting new timing decompositions, five market-window cases were fixed:
W03 ETH, W05 BTC/ETH, W06 BTC and W10 BTC. Both domains were linked to their four
sealed public archive manifests, all consumed trade-shard hashes, native market
identity and exact frozen tape projection, covering 1,065,117 prints. Recorded
execution microseconds supplement block milliseconds only for aggregate diagnosis;
receipt/availability nanoseconds are floored to microseconds for an exact bridge
to the original upward-rounded replay millisecond. Conflicts, missing fields,
prestart filtering, subscriptions and duplicates remain explicit. Execution clock
alignment, future delivery bounds and uninterrupted market coverage are unproven.

All input paths, UTC ranges, market/domain mapping, source/config/report hashes and
transformation hashes are in protocol `ce5d5443103c1fbf5d69d46e8658b9958e569556a51d9dfc97d15d913bd5ee91`,
frozen at 2026-11-20 06:23:00 UTC, under the local/aleph campaign directory
`.agent-state/research/trade_delivery_20261120`. The official timestamp page was
inaccessible in this round; semantic interpretation uses the November 14 archived
review, not a claimed new official-document retrieval.
[Results and limits](TRADE_DELIVERY_CLOCK_LINEAGE_2026_11_20.md).

### November 20 sealed W11 and public native recorder cross-check

The four-market Lighter Core historical census contains 63 recorder-date units and one calendar date recorded independently by Tardis and CryptoHFTData. On November 1 both source lineages independently reconstruct the 07 and 13 UTC native ranks and the 13–23 UTC ten-hour execution endpoints. The original $25 per asset, 310 ms arrival, 20% available depth, 3 bp price protection, ten funding settlements and 1.25 bp traded notional cost are retained for each wallet. Every jointly observed second was compared using absolute midpoint differences divided by the mean midpoint. The two sources supplied all four marks together at 35,705 of 36,000 seconds. Their four-leg terminal nets were −$0.354584 and −$0.355096. Neither recorder supplies a continuous four-leg path that day. The subsequent 3 bp diagnostic was registered after inspecting the largest recorded difference. The Goldmine public catalog was fetched again at revision 7251ac84b055dd155e6cf9bc5bf7404a5cf06c86. All four markets still had exactly eleven July 17–27 daily delta files. Source hashes, ranking clocks, per-market cash flows, handling of missing observations, catalog dates and protocols are in the campaign directory cohort_clock_review_20261120. [Goldmine native data catalog](https://huggingface.co/datasets/rogerdehe/mktdata-lighter-2026)

The official Lighter timestamp definitions were retrieved directly from the publisher on November 20. The archived 15,641-byte markdown has SHA-256 7953935bda23a938eebaf9e741ec47c490bb6cf5d04e74431195c36f40923a0c. The Trade object separates millisecond block-open time and microsecond transaction execution time. The WebSocket envelope carries a separate publication timestamp. Collector receipt and availability remain independent observations. The original five Core/RH market-windows across W03, W05, W06 and W10 supplied ten linked clock comparisons and 1,065,117 matched prints under the repaired offline identity checker. Clock counts, first violations, all component distributions and prior report hashes were compared. Version 2 reports the absence of venue shards, in-window market messages or projected prints separately. W11 source checks are registered after capture and before replay results, conditional on the original BTC and ETH pair receipts. No new economic configuration is associated with this clock audit. [Lighter timestamp reference](https://docs.lighter.xyz/developers/reference/timestamps.md)

A separate public input parity check read the original W09 and W10 BTC/ETH single-venue maker tapes and the Core leg in the signed two-venue arbitrage tapes. All four hashes per strategy family were bound to the frozen summaries. Their 660,203 public trades and 16 trade-stream state messages matched in order. Comparison used exact decimal price and size, side, venue source time and local availability. Warm-up, ten-hour decision interval and terminal drain contained 961, 658,928 and 314 trades. The market prints are shared evidence and each simulation has its own fill and cash ledger.

### November 20 fixed pairs inside the native four-asset reversal

The previously registered 63 recorder-date units supplied 60 complete signed four-leg endpoints across 62 dates. The six-hour native ranking is recomputed at 13 UTC and reconciled to both the original one-per-side and two-per-side signed target sets. The primary pairings are BTC/ETH and SOL/XRP. The other two partitions are included for sensitivity and all economic parameters, 310 ms arrival, 20% displayed participation, 3 bp collar, actual ten hourly funding cash flows and 1.25 bp stress cost are inherited. Each pair takes opposing $25 positions. None of the three partitions produced positive terminal net cash in both fixed 22-day halves. July 17, 18 and 27 have missing native rank evidence. November 1 appears in two source cohorts. The pre-result rule, source file hash, market clocks, per-date order and all tested partitions are in cohort_clock_review_20261120/pair-rank-plan.json and pair-rank-results.json. [Pairs trading research](https://www.nber.org/papers/w7032)

## ANT campaign public-rule snapshots — 2026-11-20

[ANT review and 48-hour plan](ANT_CAMPAIGN_REVIEW_AND_48H_PLAN_2026_11_20.md)
uses Arcus ANT announcement, fee/funding/RWA/DMS/authentication documentation,
Protocol Terms Annex C, Arcus `/v1/markets` and `/v1/markets/feeSchedules`,
and Hyperliquid `metaAndAssetCtxs` for dex `xyz` plus its official fee formula.
These are point-in-time public rule sources, not executable depth, private fees,
settled funding or a continuous ten-hour execution tape. Request and receipt
clocks are separate; initial receipts with a mislabeled start time are retained
and excluded from freshness admission. The new public-only capture/audit tool
is `scripts/audit_ant_campaign.py`; it records URLs, request selectors, hashes,
failures and coverage limits outside Git, never grants live readiness, and
values unconfirmed points at zero. The append-only campaign record and receipts
are under `.agent-state/research/ant_p0_20261120/`.

## 2026-11-20 东京 ANT 历史覆盖与 aleph 双场所原生观测

W01–W11 的 bulk、crypto、RWA 共 33 份 Bronze 市场清单与两处归档逐件相符，三个 W04 隔离原件保留原标识。原有两域均无 ANT，11 个 RWA 窗均找到两域的 NVDA 正对照。W12 当前三个父子进程的公开订阅也未包括 ANT。独立 WSL 采集使用 Arcus mainnet 的 ANT-USD、TradeXYZ HIP-3 的 xyz:ANT 原生盘口与公开交易，另留独立的 xyz REST L2 对照。90 秒连接样本、两分钟对照、两次连续十小时的协议与各自可用时钟、原件哈希、时间与行情缺口分别入账。费用采用 Arcus 的公开定制费率以及明确标记的 xyz Tier 0 情景，资金费和抵押品差额未进入短样本。后续签收沿用每进程的时钟和分段终止证据。[数据、计价与回执](../.agent-state/research/ANT_NATIVE_CAPTURE_AND_W11_CUSTODY_2026_11_20.md)

### November 20 ANT matched prints, source clocks and W11 selective recovery

The Tokyo W11 bulk, crypto and RWA whole archives total 15,204,671,348 bytes. All three whole-file Drive readbacks match Tokyo, Mac and aleph receipts. Independent inspection of all 76,233,720 RWA input records confined 2,104 nonpositive sequences to empty Lighter Core market 139 snapshots with zero updates. The frozen Robinhood NVDA, QQQ and SPY inputs remain available for their registered analyses. Full RWA Silver and the combined GPU product remain withheld. The W11 scope binds the raw file, source manifest, pre-window parameters, projected catalog and independent physical readback.

The Arcus ANT-USD and TradeXYZ xyz:ANT study uses two independent receipt-clock capture processes on aleph, native full-depth quotes, public matches, supplemental xyz snapshots, lot limits, explicit fees and zero incentives. Historical 08:00–08:20 UTC and prospectively registered 08:45–09:05 UTC are distinct diagnostics. Two positive conditional prospective pairings were rechecked at the previously displayed touch, one observation per aggressor. Both had negative cash at zero queue ahead, and neither met minimum size under the full visible queue scenario. The independent clock-grid model was registered before 09:15–09:35 UTC. Source prefix hashes, availability, venue-time and fee rules, assumptions about queue position and unpriced outcomes are retained under ant_passive_cash_20261120. [Arcus public trade fields](https://docs.arcus.xyz/api-reference/public/get-a-single-trade-by-id.md).

An audit of 35,000,818 updates in 67 signed January–April Lighter Core market-hour files found no positive matching-engine timestamp or explicit preceding sequence link. On the signed 2026-10-20 four-asset unit, two reissued hours across four symbols and the official daily trade Parquet were verified; the 22:05 UTC minute has 882 official executions and zero seconds with executable four-market marks. Both official block-open and transaction clocks were tested against native availability. Raw file, transformation, terms and gap hashes are retained with the separate source and historical wallet receipts. [Vendor corrections](https://www.cryptohftdata.com/docs/data-gaps-and-corrections) · [Lighter clocks](https://docs.lighter.xyz/developers/reference/timestamps.md).

## ANT native execution evidence contracts (2026-11-20)

Public API specifications only, not account reads or market observations.
Arcus [orders](https://docs.arcus.xyz/api-reference/public/get-order-history.md),
[fills](https://docs.arcus.xyz/api-reference/public/get-fills.md), and
[positions](https://docs.arcus.xyz/api-reference/public/get-positions.md);
Hyperliquid [info](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint)
and [perpetuals account state](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals).
Acquisition receipts, hashes and a failed guessed order-document URL are retained
in the local-only `ant_native_evidence_20261120/sources/manifest.json` campaign.
Applicable domains are Arcus ANT-USD and TradeXYZ xyz:ANT. No economic time range
was consumed; tests use synthetic accounts, clocks, orders and amounts. Arcus
fee-asset identity and REST position source time remain unknown. REST history
counts cannot establish a source fill watermark or complete retained history.
See [native evidence boundary](CROSS_VENUE.md#native-ant-rest-evidence-normalization-offline).

The same campaign also retains the [Arcus WebSocket sequence contract](https://docs.arcus.xyz/api-reference/websocket.md). Per-account sequence IDs are a separate evidence domain from venue event time; their future use requires a versioned recovery proof, not relabeling a sequence as microseconds.

### November 20 native Lighter ticker, ANT wallet risk and W11 frozen receipts

The separate Tardis native Lighter ticker BBO channel was checked for BTC, ETH, SOL and XRP on seven May–November first-of-month dates. The 28 files (863,109,406 bytes) contain 89,139,671 rows. The campaign records market IDs, source and receipt clocks, dates, file checksums, timestamp ambiguity, one-level participation, ten funding slots, partial exits and per-leg results. The ticker supplied 26 additional four-market valuation seconds; 446 seconds remain unavailable across the seven 10-hour dates. [Exchange channel catalog](https://api.tardis.dev/v1/exchanges/lighter) · [Coverage, new prices and original ledgers](../.agent-state/research/FORWARD_HANDOFF_2026_11_20_1818.md)

The frozen ANT 09:15–09:35 UTC baseline and separate continuous-risk wallets used public Arcus ANT-USD and xyz:ANT depth, print and receipt clocks, explicit venue fees and zero incentives. The wallet protocol binds individual venue pockets, a loss budget, a queue model, per-leg inventory uncertainty, and a pre-entry paired convergence estimate. The 10:10–10:30 UTC wallet sample was frozen before its start. The 08:00–18:00 UTC long unit includes earlier observed trading hours and requires two terminal source manifests. The W11 supplemental market projection read back 76,651,912 rows in 45 physical files. All 40 frozen configurations were reconciled. The two positive ETH variants had identical economic observations and completed two passive-only cycles each, without a paired hedge cycle. The original and supplemental source, market scope, active/empty/withheld results and cross-window attribution are preserved. [Wallet rule, split and source hashes](../.agent-state/forward_ops_20261118/ant_grid_forward_20261120/wallet-plan.json) · [W11 catalog readback](../.agent-state/forward_ops_20261118/w11_source_recovery/catalog-verification.json)

### ANT fixed-wallet forward receipt, observed flow and additional Lighter months, 2026-11-20

Three sealed ANT short windows use the public Arcus ANT-USD 20-level snapshot and trade streams, plus Hyperliquid xyz:ANT websocket and REST snapshots. Separate receipt, venue, per-market sequence, fee, depth, execution and lateness observations are registered in the campaign. The forward cash result uses the 18:10 CST wallet freeze. The 19:00 CST window was registered before its start for the observed-flow and decision-depth factors. Four-market Lighter historical anchor audit adds seven dates in April–October outside the prior 62-date ranking census. 27 of 28 public hourly files were verified after separate transport recovery. Original first attempts and missing timestamps remain recorded. [Published Lighter format](https://www.cryptohftdata.com/datasets/lighter-orderbook-data) · [Order-flow study](https://arxiv.org/abs/1011.6402) · [Research receipts](../.agent-state/research/ANT_DEPTH_SIZING_AND_NATIVE_MONTHS_2026_11_20.md)

### 2026-11-20 — ANT cost, two-direction basis, netting and funding frontier

The same three already-scored public ANT windows (08:00–08:20, 09:15–09:35,
10:10–10:30 UTC) supply 84 existing paper wallets; 11 are settled, 22 idle and
51 unresolved. Immutable parent hashes and six native plus six snapshot sealed
segments are registered outside Git. New diagnostics preserve original decisions
for fee ablation, inspect both basis directions at fixed $25/$100/$500 sizes,
and measure hypothetical minimum hedge quantity for fixed 1%/10%/100% public-flow
participation with $100/$500 initial-mid inventory bands and terminal flatten.
Same-source-time batch netting is optimistic; public prints are not own fills.
The 09:15 public-trade subscription has an interruption. Pair freshness, skew,
depth gaps, failed or missing exits and every scenario remain in the inventory.

A separately registered follow-up uses 310 ms entry delay and actual visible
four-leg taker exits every 15 seconds for a fixed 300-second diagnostic horizon.
Overlapping quote scenarios are not wallets or independent profit observations.
No new economic validation units, no untouched holdout and no new 10-hour result.
Public fee/market captures at 11:18 UTC use Arcus Tier 0 0/1.12 bp and conditional
xyz taker 0.9 bp; account eligibility remains unknown. Official hourly funding
history spans (2026-11-19 11:00, 2026-11-20 11:00] UTC: 20/24 matched hours,
four missing Arcus points, with original HL subsecond publication offsets retained.
Equal-notional rate differences are not actual base-position funding cash.
Initial CA and exact-hour-boundary attempts remain documented. Source hashes,
request receipts, transformations and complete diagnostic outputs are in
`.agent-state/research/ant_opportunity_frontier_20261120/source_inventory.ndjson`.
[Full cost and capacity study](ANT_PROFIT_AND_TURNOVER_FRONTIER_2026_11_20.md) ·
[Arcus funding history contract](https://docs.arcus.xyz/api-reference/public/get-market-funding-rates.md) ·
[HL funding contract](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/funding)

### 2026-11-20 — ANT continuous maker inventory and displayed-queue coverage

One fixed continuous-inventory research policy uses the same sealed public
08:00–08:20, 09:15–09:35 and 10:10–10:30 UTC ANT windows, original reader hashes
and six native plus six snapshot segments. The initial model's two queue stops
were attributed without changing its ledgers: both absent quote prices were
inside the complete 20-level visible prefix. A separately frozen second model
assigns zero displayed queue to such gaps while retaining unknown depth outside
the prefix and all late/ambiguous-print barriers. Both model versions and all six
wallets remain registered; no new untouched validation or ten-hour unit.

The final model has one +$0.0492154312 flat paper cycle and two unresolved wallets;
none has an external net hedge fill. Partial maker quantities below the order lot
are preserved. Funding is unavailable rather than assumed zero when inventory
crosses an hourly settlement boundary. Source and receipt clocks, partial fills,
unknown risk intervals, early stops, full terminal drain, fee assumptions and
the synthetic two-venue accounting regression are in the append-only campaign at
`.agent-state/research/ant_continuous_maker_20261120/`. Public modeled fills are
not account evidence. [Research report and reproduction](ANT_CONTINUOUS_INVENTORY_AND_QUEUE_COVERAGE_2026_11_20.md)

### ANT first forward depth test and protected paired wallet, 2026-11-20

The Arcus ANT public native book and trade feed and xyz ANT public snapshots on aleph supplied the November 20 11:00–11:20 UTC depth/flow forward test. Seven sealed parts per feed contributed 13,901,554 and 3,388,090 bytes respectively. `ant_ofi_depth_20261120/forward-1900-aggregate.json` on Mac and the three `*future.json` receipts on aleph distinguish depth, queue and missing-sequence evidence. The separate `ant_paired_wallet_20261120/plan.json` froze protected entry and exit limits at 11:47:59 UTC. Its four earlier 20-minute periods are historical. The additional forward short begins November 20 12:00 UTC and the untouched ten-hour period runs November 20 20:00 to November 21 06:00 UTC. Per-file hashes, window and drain boundaries, quote availability and segment provenance are indexed by `signed-summary.json` and the append-only campaign inventory. The pair model uses separate venue capital, a 310 ms arrival delay, 1.12 and 0.90 bp per-leg fees and a USD parity assumption. Entry excludes funding boundaries, and residual funding or inadequate executable liquidation depth remains unknown. Cash is modeled from public books. The separate one-second drawdown receipt preserves all missing observations.

### ANT verified 20-level queue transfer, residual exits and latency, 2026-11-20

Arcus ANT-USD native `l2Orderbook` and trade messages and xyz:ANT WebSocket and REST books on aleph supply five separate 20-minute historical diagnostics beginning at 08:00, 09:15, 10:10, 11:00 and 12:00 UTC. The final period is the original protected basis wallet's first pre-registered forward short. It is historical for the new queue, residual and dual-clock methods. Only full-precision, validated Arcus snapshots with exactly 20 levels on both sides support the new zero-displayed-queue hypothesis inside the quoted span. Unknown public allocations, beyond-depth quotes, bounded residuals and missing marks remain explicit. Source availability is the local receipt clock. No private execution or incentive data enter. Each wallet keeps the prior separate venue capital and exact per-leg fee assumptions. The one-shot residual order retains a new decision-time IOC limit and expires before a funding boundary. Each complete 10/310 ms clock is rerun from entry with per-second executable liquidation marks, including prior stops. The fresh 13:00–13:20 UTC short and November 20 20:00 to November 21 06:00 UTC continuous sample were registered on aleph before either began. The `ant_queue_transfer_20261120` campaign receipts include the source segment hashes, roles, decision tail, warm-up, quote and mark gaps and every alternative. Arcus documents full versus delta frames and optional price bucketing in its [WebSocket contract](https://docs.arcus.xyz/api-reference/websocket.md).

### ANT source-clock brackets, matched public routes and early four-asset months, 2026-11-20

The pre-registered 13:00–13:20 UTC ANT-USD / xyz:ANT short uses eleven sealed parts from each public feed (24,248,717 native bytes and 5,548,595 supplemental bytes). Both signed latency wallets are reproduced. Independent post-decision checks compare full, limited IOC quantities against successive exchange-time snapshots from a single connection, retaining invalid or conflicting frames, incomplete depth and unpriced cycles. The next continuous ten hours on November 20 20:00 through November 21 06:00 UTC were frozen prospectively for this check and one additional maximum entry-age filter of 310 ms. Existing fees, funding boundaries, source time, receipt time, inventory stops and missing observations are recorded per wallet. Source-clock, availability and fee receipts in `ant_source_clock_20261120` reference the complete upstream source manifests. For the paired 14:02 UTC anonymous HTTPS measurement, Arcus ANT-USD and Hyperliquid xyz:ANT on Tokyo and aleph each have one cold warm-up and eight time-matched sampling slots with public timestamp, HTTP integrity and connection reuse. Venue age and monotonic HTTP round-trip are separate fields. Public methods are documented by [Arcus](https://docs.arcus.xyz/api-reference/public/get-l2-orderbook-snapshot) and [Hyperliquid](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint).

The separate four-asset campaign attempted January 12, February 12, March 12 and April 12, 2026, 06 UTC Lighter Core native BTC, ETH, SOL and XRP hour objects. Thirteen of sixteen were recovered (24,363,111 bytes). Update rows lack both transaction time and predecessor linkage. Three March original URLs returned 404, and a February BTC timeout was recovered. Each source hash and outcome is cataloged at `native-2026-*-12.json`, `early-network-check.json` and the original per-object receipt. The four frozen directions/width configurations remain unpriced on each date. The separate three-venue maker review consumes the pinned public November 20 13:03–13:14 UTC research prefix, records its hash and reader versions, one malformed record, missing terminal and conditional fill/MTM assumptions, and reproduces all 12 peer configurations with diagnostics isolated from the economic outputs.

### Lighter pre-IPO paired cash, vendor tape and source custody, 2026-11-20

The research prefix covers 13:41–13:53 UTC for Lighter Core ANTHROPIC/ANT (193/10902) and Lighter RH ANTHROPIC/ANT (38/4108). All four are perpetuals. The original book and market-stat prefix contains 45,645 decodable records followed by one malformed line. Archive the public projection checksum, ordinal connection epochs, receipt and server clocks, contiguous book nonces and the missing collector terminal. Each of the four contract metadata requests, the published reference multiplier, the final SEC filing and the independent MNX settlement calculation have source receipts. The venue quotes 17.615235684 as its reference and describes the final prospectus condition. MNX's 17.644765268 is a separately labelled sensitivity. The registered diagnostics retain every size, trigger, clock, active position, dynamic quantity, displayed depth, direction, fee assumption and missing funding input. [Lighter settlement rules](https://docs.lighter.xyz/trading/real-world-assets-rwas/pre-ipo-markets) · [SEC filing](https://www.sec.gov/Archives/edgar/data/2130357/000162828026074944/0001628280-26-074944-index.htm) · [MNX settlement calculation](https://docs.mnx.fi/contracts/valuation-futures/anthropic-settlement)

CryptoHFTData's ordinary public historical download delivered eight native Core objects for 13 UTC on November 19 and 20, with both order books and trades for ANT and ANTHROPIC. Individual byte ranges in time, original SHA-256 hashes, publication paths and recovery attempts are in the campaign receipt. The independent vendor receipt clock identifies 539 ANT and 98 ANTHROPIC distinct prints inside the November 20 overlap. Four independent one-sided passive-wallet tests retain the visible queue, actual trade direction, original lot, unknown fills and displayed-depth liquidation constraints. The public REST recent-100 check on each of four markets found zero prints in that time range. Venue-tier fees, rewards, the user's order queue and actual executions remain unobserved.

A new source acquisition attempt for a continuous ten-hour window obtained zero native subscriptions on WSL. The original 23-byte partial and both failure receipts are retained. A separate Core-only vendor cohort was registered on aleph before 15:10 UTC on November 20 and spans through 01:10 UTC on November 21. Its full input is 44 dated instruments, channels and natural hours. Vendor-receipt causality, book sequence and expiry, warm-up, terminal exposure and public funding evidence are graded separately. The durable runbook entry point is the registered vendor reader in the campaign. No RH backtest can be inferred from these Core objects.

### Lighter pre-IPO archive qualification and matched-minute queue replication, 2026-11-20

Eight already archived CryptoHFTData Core objects for ANT and ANTHROPIC at 13 UTC on November 19 and 20 were rechecked, including the previously recovered November 19 ANTHROPIC book. The intake verifier requires non-null nanosecond receipt time within the requested half-open UTC hour, exact symbol and channel receipt identity, forward venue sequence steps, causal exchange clocks and immutable content fingerprints. Its frozen economic replay continues to use the 23:10–09:10 CST Core-only contract. Missing quote coverage, source clock failures, spliced partitions, null trade evidence and unavailable paired quotes retain explicit grades. A signed result references its signed intake and rechecks all accepted objects. Earlier code, rejections and original sleeves remain in the campaign.

For the two historical UTC 13:41:37.500–13:53:15.000 slices, 885 distinct publisher-received public trades support four isolated historical maker wallets per date. The replication tracks visible queue, decision and arrival times, side, actual simulated opening quantity and liquidation prices. Each cycle attributes entry spread, inventory movement and crossing spread at exit. The campaign receipts record per-market clocks, hashes, source gaps and the lack of actual account fees or funding. The two short windows are retained as historical diagnostics.

### W12 boundary, initial Core pre-IPO hour and ANT holdout acceptance, 2026-11-21

The Core ANTHROPIC/ANT independent archive received the two order books and two public print objects for the natural 15 UTC hour of November 20. The full cohort needs 44 objects and its decision windows remain pre-registered. Both WSL print requests timed out. The same publisher's authenticated-free public HTTPS route from the Mac returned the two objects, which were hashed at receipt and again on aleph. Original failure and relay receipts record the exact UTC partition, row counts, clocks, sequence, byte counts and stable fingerprints.

The Arcus/xyz 04:00–14:00 CST frozen holdout adds an independent evidence check before economic interpretation. It binds the sealed segment list to the signed launcher and original wallet, verifies the freeze, audits actual decision and arrival book availability, exposes the longest gap, confirms whether the Arcus trade subscription covered each candidate maker interval, counts distinct causal prints and records terminal depth and the 8.310-second specified drain. A complete envelope with no admissible book remains visible as an incomplete source example. Funding and account fees require separate evidence.

The two already recovered CryptoHFTData Core book and print hours each contribute the matched 11.625-minute historical slice. The added maker variant keeps the same FIFO admission, initial quantities and order arrival. A fully matched simulated order can exit using a later vendor receipt plus the fixed 360 ms model delay while partial allocation retains the original timed exit. Availability, venue-millisecond and exact lot constraints remain explicit. This retrospective variant provides two independent short dates and no new ten-hour observation.

### W12 custody, ANT precise admission and Core queue stress, 2026-11-21

The Tokyo W12 collector has three sealed raw files, and the original locked coordinator is still reading them. Campaign evidence keeps collector seal, Bronze manifest, transferred bytes and Drive readback as distinct fields. Tokyo's installed public flywheel reads unmanifested archives before it reaches low-water eviction. The offline synthetic check reproduces this order using a backup-verified old file and a newly closed file. Observed read counters and the W11 crypto Drive eligibility remain attached to the custody record.

The Core vendor's second legal UTC partition supplied both ANT and ANTHROPIC book and print objects. Eight of the 44 scheduled inputs passed the existing acceptance gates. The frozen Arcus/xyz holdout separately checks exact nanosecond-form launch intent and first-frame order, rejects an unexpected partial segment on recheck, and reports acknowledged public prints by receipt latency instead of using that count for allocation. Original holdout strategy source, funding assumptions and execution path remain the same.

The existing two-date 11.625-minute Core data supports a historical mechanism check with zero rewards. Preserve the eight original controls and 16 independent price-time queue stress results. The added 10% and 25% ahead reservations are rounded up to exchange lot precision against the larger of posted lot and public displayed-ahead quantity. This also evaluates candidate quotes with no public displayed queue. Trade directions, source clocks, end of order lifetime, protective exit rules, public depth and funding gaps follow the original inputs. None of these short historical checks is counted as a ten-hour result.

### W12 complete transfer, Arcus/xyz holdout launch and Core exit-delay stress, 2026-11-21

The original locked W12 transfer admitted the sealed Tokyo bulk, crypto and RWA raw files after checking 17,839,841,203 bytes, matching SHA-256 and clean gzip end of stream. The preferred custody chain keeps Tokyo Bronze, canonical E-drive archive, full Drive readbacks and frozen economic receipts distinct. W12 analysis and whole-file backup progressed through the pre-existing coordinator. An isolated Backpack research service was absent at the read-only follow-up. Tokyo's primary recording and proposed research location share the root disk.

The previous Arcus and xyz public captures each carry 21 sealed segments and a terminal marker. Their earlier wallet is a continuous ten-hour mixed-history-and-forward cohort. The new 04:00–14:00 CST holdout used the planned WSL recorder once in the permitted startup window. Preserve the precise launch-intent and command digest, the initial open-segment evidence and terminal eligibility. Observed Arcus and xyz public books and prints, REST snapshots and funding responses advance independently. Open warm-up packets are not promoted to a sealed economic sample.

Core independent archive custody passed all four objects for the third legal UTC hour and 12 of 44 cumulative parts. The two already-used 11.625-minute Core historical windows, frozen public FIFO, full-fill confirmation and terminal deadline support three additional exit latency arms. Advance only from data available to the vendor at the modeled exit. Keep original quantities, entry lag, partial fill handling, account fee and funding gaps, venue and vendor clocks and missing depth. The independent delay wallets are historical diagnostics.

### W12 causal source prefix, Tokyo custody recovery and ANT entry persistence, 2026-11-21

W11 crypto and W12 bulk were reclaimed by the existing Tokyo automation after their seals matched whole-file Drive readbacks. Archive byte counts, calendar times and matching tombstone evidence are stored with the operational receipts. The installed flywheel is identified separately from the local worktree. A narrow synthetic concurrency reproduction checks the handling of an archived capture that another shipper already reclaimed.

The independently signed W12 RWA prefix contains the first 8,192 archive records and two nonpositive nonce values for empty Lighter Core market 139 snapshots. Record exact availability time, archive and manifest SHA-256, source, market, record number, and prefix length. The original strict replay outcome and whole-file findings each keep their own evidence. The prior W11 scanner is rebound to the sealed W12 Bronze and frozen w07–w16 contract for a full-window audit after the original evidence records the actual error category. No partial catalog was admitted with the prefix receipt.

Forty-two already sealed Arcus and xyz (including REST) objects support the mixed 08:00–18:00 UTC ANT retrospective. The two tested entry variants re-use original fee models, order sizes, budgets, the 310 ms protected latency and original exit rules. Each demands the same executable direction in two one-second-apart decisions, and the stricter variant also demands a newer venue source timestamp on both legs. The archived test receipts preserve quote holes, exact original ledger reproduction, known cash, residual position and missing fee, funding and terminal valuation evidence. This retrospective adds no forward ten-hour result.

### W12 full-window source isolation and ANT bounded exit, 2026-11-21

Oasis Tokyo W12 bulk, crypto and RWA whole-file Drive readbacks match their Tokyo receipts and the aleph Bronze copy. The W12 RWA full audit covers 2026-11-20T06:29:49.204+00:00 through 2026-11-20T16:44:48.590+00:00 in collector availability time. It binds 92,098,538 records, gzip EOF, the transferred manifest and verified file identity. All 1,748 nonpositive sequences were empty Lighter Core market 139 subscription snapshots. The full scan found zero conflicting Arcus trade IDs and verifies native Lighter Robinhood books and Arcus trades for NVDA, SPY and QQQ. Full RWA Silver and combined GPU remain withheld. The independent scope gate binds the original 14 profiles, W07 compiler, unchanged public native tapes, and 16+8+2 pre-frozen crypto supplements. The replay and follow-up receipt review were started and retain their own results or failures.

The ANT bounded exit uses the same 42 sealed Arcus and TradeXYZ native and REST objects and the original 08:00–18:00 UTC mixed ten-hour retrospective, with fees, zero incentives, fixed clocks and no additional calibration artifacts. Two historical variants preserve separate ledgers. Both require a protection price on each leg at the common decision time. One adds the cross-venue source skew threshold. Each leg has its own simulated arrival and IOC fill and the resulting receipts independently record residual quantity, execution uncertainty, missing cash and absence of funding evidence. The separate 04:00–14:00 live public collection is still open and has no frozen execution-admission or economics receipt. [Scope evidence and experiment](../.agent-state/research/W12_FULL_RWA_CUSTODY_AND_ANT_BOUNDED_EXIT_2026_11_21.md)

### W12 native frozen receipt, Arcus acknowledgement and ANT fixed-limit size stress, 2026-11-21

Fourteen original profiles and 26 pre-frozen supplemental profiles have independent W12 native replay receipts. Retain the strict catalog failure, the failed legacy S9 v1 dispatch and the corrected frozen v2 path. Preserve the separate crypto and RWA ten-hour clocks and all missing cash, terminal inventory, fee, funding and trade coverage evidence. Tokyo automatic retention reclaimed the W12 RWA copy only after its full-object Drive readback and Bronze evidence matched. The frozen replay receipt does not certify the complete RWA Silver or combined GPU. The isolated storage projection has its own plan and verification.

The full W12 Arcus WebSocket and REST source-clock audit verified the sealed RWA gzip length, source digest and all 92,098,538 records. Among 2,577,181 Arcus book-channel payloads, the only missing timestamp was an empty subscribed acknowledgement for SNDK-USD at original record 640. The separate projection retry recognizes the exact raw row as a lifecycle acknowledgement. It preserves collector availability, original sequence and source identity without inventing a venue timestamp or a BBO. The readiness diagnostic keeps subscription confirmation separate from a timestamped book recovery.

The ANT sizing diagnostics retain the signed 42 public source objects, original mixed continuous ten-hour range, 310 ms arrival and fixed per-leg protection. Each of four clipped sizes runs independently across five known planning events. 100 and 250 ms predecision source reuse, missing predecision book history, known failed legs, and unmarked ending inventory are tracked separately. No new ANT holdout or actual execution evidence was generated. [Source and strategy receipts](../.agent-state/research/W12_FROZEN_40_ARCUS_ACK_AND_ANT_CAPACITY_2026_11_21.md)

### W12 isolated physical acceptance, W13 open prefix and ANT fixed tick, 2026-11-21

The W12 scoped v2 RWA projection read back 92,657,008 derived rows over 25 Parquet files from the original sealed RWA object. Its signed plan binds the one SNDK empty acknowledgement and exact 1,748 invalid Core 139 snapshots. A separate physical clock audit matches every one of the 2,577,180 Arcus quote rows by source and market to the earlier full Bronze audit and verifies exact receipt and availability conversion, unique sequences and the single event at sequence 639. Full canonical RWA and combined GPU remain withheld. The original 40-profile native replay scope is preserved with an independent physical projection pointer.

The 8,192-row Tokyo W13 open RWA prefix is hashed in receipt order. It spans 2.655 seconds and has 252 positive-clock Arcus book frames. The prefix contains no Lighter Core 139 book message even though the running recorder includes that public market in its configured scope. The full sealed archive and market coverage are yet to be verified. The seven-hour public vendor intake has 28 of 44 planned checked objects and keeps its own WSL qualification.

ANT one-tick diagnostics use the original 42 sealed public files and five unique protected timeout dispatches from the earlier 08:00–18:00 UTC mixed historical sample. Preserve original receipt-time policy, 310 ms arrival, fees and funding cap with separate matched statements. The whole-history repeat memory failure and bounded byte-identical repeat are separate receipts. W12 signed BTC/ETH exit opportunity diagnostics are sourced independently from their original frozen books and reconcile before hypothetical fees, arrival and fill changes. [Full evidence summary](../.agent-state/research/W12_VERIFIED_PROJECTION_W13_PREFIX_AND_ANT_TICK_2026_11_21.md)

### Tokyo sequence readiness, four-coin pre-cutoff custody and W12 ETH cash attribution, 2026-11-21

The Core and Robinhood reconnect diagnostic separates subscription confirmation from the first positive-integer book nonce. The Arcus readiness criterion uses the previously established native venue timestamp rule. Synthetic receipts keep the original empty or malformed frames for verification. This change is an offline diagnostic and preserves the byte-identical W12 40-profile receipts and separate RWA projection.

The first prospectively registered BTC/ETH/SOL/XRP source intake uses the original four-coin protocol, registered engine and decoder. At the readiness check none of the 52 exact first-day source receipts existed. Earliest eligible intake is 2026-11-21 01:00 UTC under the old source plan. Individual-asset attribution reuses the archived final parent and priced-file digest and has a separate receipt from paired attribution. The Core pre-IPO qualification has 32 of 44 source objects with actual receipt-clock range and symbol corroborated. The last publisher-hour gate is 2026-11-21 02:05 UTC. The two independent forward samples retain their own source, time bounds, costs and gaps.

ETH cost attribution uses the four signed W11/W12 original S9 v2 Core-maker/RH-hedge reports, their existing risk settings, reuse classifications, exact displayed price, original turnover, explicit model fees and stress budgets of 0.25 bp gross plus 1 bp taker. It accounts for 12 reconciled wallet cycles, the identical W11 economic path, W12 changing inventory drift and all known source gaps. No account fee or funding assumption is added. [Detailed receipts](../.agent-state/research/W13_BOOK_READINESS_FOUR_COIN_AND_ETH_COST_2026_11_21.md)

### First four-coin intake custody, Tokyo W13 prefix readback and Core BTC guard, 2026-11-21

The frozen BTC/ETH/SOL/XRP forward cohort first admitted its original public 06 UTC anchor objects after the registered 01:00 UTC intake gate. All four anchor objects and receipts were verified by raw SHA-256, byte count, exact market and hour, publisher identity and required populated source-time and sequence fields. The four anchor receipts span 2026-11-21T01:03:42.388783+00:00 through 2026-11-21T01:04:05.460168+00:00. At the last read-only holding snapshot 20 of 48 holding-hour receipts were present. Wallet, funding evidence, pair attribution and asset attribution remain tied to the registered final and priced receipts. No economic row was inferred from the anchors.

The observed Tokyo W13 open prefix has its original SHA-256 and 8,192 records bound to a read-only post-seal comparison. This checks the three-stream transfer receipt and the exact W13 RWA Bronze descriptor before comparing the prefix. It preserves any mismatch independent of the original canonical analysis. The separate Core pre-IPO source intake has 40 of 44 qualified objects. The ninth hour's first ANT trades HTTPS timeout and successful same-object retry are retained as separate receipts. The following hour's 404 and timeout evidence remain on file. The frozen 04:00–14:00 Arcus/xyz live public sample is in progress.

The retrospective W11/W12 Core BTC guard audit checks two original signed BTC tapes, 3,654,846 events and 1,463,891,387 bytes, and all twelve maker/kappa wallets. The independent clock join preserves source, trade acknowledgement and going-stale ages. Funding-eligibility guards at the inferred first failures have two seconds without a qualifying book despite explicit trade online state. Subsequent healthy slots, flat terminal positions, original trade continuity blockers and unknown economic cash remain separately substantiated. [Original evidence and controls](../.agent-state/research/FOUR_COIN_FIRST_INTAKE_W13_AND_BTC_GUARD_2026_11_21.md)

### First frozen four-coin terminal and ANT collateral falsification, 2026-11-21

The first CryptoHFTData Lighter Core forward day consumes four UTC 06:00 anchors and 48 BTC, ETH, SOL and XRP order-book hours. Original 13:00–23:00 UTC decisions for November 20 use a 310 ms native arrival deadline, $50 per leg for one asset per side and $25 per leg for two per side. Keep original receive and exchange clock, linked update sequence, ten model funding slots and 1.25 bp turnover stress, with zero rewards. The signed first attempt binds the expected publisher objects, sizes, hashes, method and separate intake times. The priced result, final acceptance, asset attribution and balance-preserving pair allocations are independently bound. The two wallet widths have 277 and 553 missing signed joint-fresh seconds. All four endpoint cash receipts are retained with zero complete paths. Pair allocations use only the original wallet and do not create new decisions or independent samples.

The ANT collateral check reads four prior signed public-model wallet summaries for separate Arcus ANT-USD and TradeXYZ xyz:ANT roles. Three are 20-minute diagnostics, one is a mixed continuous 10-hour simulation, and 28 wallet observations are checked without claiming separate market histories. Aggregate buys and model fees produce a lower bound only on recorded cash. The two-venue order-arrival counterexample uses synthetic books and existing wallet methods, so it consumes no extra captures and has no account-cost or private-fill evidence. The real prospective 04:00–14:00 CST capture is recorded as an in-progress prefix only, with a separate verified process, frozen engine, 14 sealed segments and no economic observation.

The independent Core ANTHROPIC and ANT CryptoHFTData cohort covers 2026-11-20 15:10 UTC through 2026-11-21 01:10 UTC. The first ten UTC partitions across each book and trade channel retain their signed replay input. The four last-hour sources preserve initial HTTP 404 and later WSL timeout receipts. The Mac retry used the identical HTTPS publisher objects and the aleph relay checked UTC partition, byte hash and channel-specific chronology or sequence before original publication. The replay and readback preserve original 3-second age, source and arrival clocks, book gaps, partial coverage, trade visibility and unmeasured account fees, funding and maker cash. This publisher cohort is Core only. The failed native WSL capture and RH absence remain separate.

### Core exit-source timing and ANT print-conflict chronology, 2026-11-21

The signed independent Lighter Core CryptoHFTData set reuses 44 ANT and ANTHROPIC public order-book and trade objects and the original 2026-11-20 15:10 UTC to 2026-11-21 01:10 UTC result. The original decoder regenerated both fixed-clock panels in scratch with identical sample counts, unavailable durations, resets and clock-reversal counts. The joint decision lattice contains 2,055 book gaps out of 144,000 points. The exit-clock check consumes the 16 UTC order-book and public trade partitions only for the two already recorded 16:36:50.360 and 16:45:50.360 UTC exits. It distinguishes receipt gaps from market-source ages and counts same-market prints through each cutoff. The next receipt is an after-the-fact witness. It does not change price availability, the frozen decisions or fee and funding exclusions.

The Arcus ANT print chronologies use 21 previously signed native mixed-window segments, 55,957,467 bytes and 325,337 decoded events. The original mixed 2026-11-20 08:00–18:00 UTC economic unit remains separate from the ongoing 2026-11-20 20:00 UTC to 2026-11-21 06:00 UTC holdout. Match identifiers are scoped by symbol and availability; 39,098 unique original match IDs were checked through the mixed-window cutoff, including the already admitted prefix, with no duplicates or conflicting variants. A synthetic test preserves the first-visible decision and distinguishes later relevant corrections from unrelated later conflicts. Original fee models, fills, ownership assumptions and absent account evidence are unchanged.

Tokyo W13 produced its three sealed bulk, crypto and RWA archives near 2026-11-21 02:59:49 UTC, with a combined physical size of 18,155,300,875 bytes. The existing coordinator recorded all three sealed and started attempt one at 03:00:31 UTC. Preserve the file roll observation, each original manifest and hash, the verified aleph transfer, and the whole-file Drive readback as distinct evidence. The W14 observations record capture growth only. The separate ANT collateral prototype uses synthetic prices, fills and fees exclusively, retaining a signed quantity for residual exposure and distinct availability for spendable, reserved and uncredited cash.

### Prospective Arcus ANT dual-site receipt clocks and Tokyo W13 custody, 2026-11-21

The matched print protocol registers Arcus ANT-USD, market 88, on two already running public recorders. Tokyo uses the separately authorized research capture started at 03:40:42 UTC. Aleph uses the existing 20:00–06:00 UTC prospective ten-hour ANT capture. Independent first-frame witnesses bind each run identity, engine digest and subsecond clock. Analysis requires sealed custody on both sides. The new diagnostic window is the half-open exchange timestamp range 04:15–05:30 UTC, with 10 seconds of receipt drain. The accepted aleph source also requires the existing full-window admission and its 8.310-second drain. Preserve absolute clock uncertainty, site-only identifiers, duplicate and conflict exclusions, event identity, receipt and availability. Infer no realized fees, funding, execution latencies or economics. No future-period records have been consumed at registration.

Tokyo W13 contains 18,155,300,875 sealed public bytes. At 04:04:30 UTC the original locked Mac transfer had staged 9,097,445,376 bytes and no complete stream receipts. Keep staging, aleph custody, whole-file Drive readback, physical eviction and frozen research acceptance as separate states. The original flywheel is still scanning W13 bulk while low-water eviction is pending. The installed method and observed byte offsets are preserved. W14 observations and the separate Tokyo ANT live prefix express acquisition health only.

### Tokyo W13 first transfer and low-water ordering regression, 2026-11-21

W13 bulk on Tokyo and aleph is the same public archive, 4,859,013,395 bytes. Active Mac transfer receipts record the publisher hash, matching staged hash, complete gzip verification and the aleph cold Bronze manifest readback. Crypto and RWA retain separate pending states. The whole-window Drive readback, replay admission and Tokyo eviction remain independent states.

The Tokyo current low-water diagnosis reads production service state, uptime, occupied space and compressed file offsets. The already backed W12 crypto archive has 6,200,697,098 bytes, existing matching seal and backup metadata, and previously verified three-site custody. Any early operational eviction needs its own authorization and archive revalidation. The code change exercises only synthetic public gzip captures and backup receipts. It keeps local SHA-256, byte count, receipt matching and immutable tombstones in the original eviction function. Two targeted ordering regressions fail before the change and pass afterward.

### W13 complete custody, RWA peer review and ANT closing evidence, 2026-11-21

W13 Tokyo bulk, crypto and RWA transferred 18,155,300,875 sealed bytes to aleph. Exact sizes, SHA-256 and gzip EOF are in the original three stream receipts. The first 8,192 original RWA rows were independently matched to the open-file prefix by their exact bytes. Transfer, the Drive whole-object readback, archived data retention and frozen economic evaluation retain separate receipts. The original W12 crypto tombstone confirms automatic reclamation of 6,200,697,098 already backed bytes.

The peer W13 RWA review consumes only the existing public-model result for six markets and nine variants and its matching Tokyo RWA transfer receipt. W13 remains selection_dev. Some reviewed variant settings were chosen after observing an earlier part, and the same 10.25-hour archive was used throughout. Keep Arcus simulated maker and RH hedge turnover as distinct denominators, with their original fee assumptions. Midpoint paper equity and the net-only RH drain estimate do not certify bilateral closure, exact account fees, funding, or realized cash. The two-site Arcus ANT diagnostic separately records the two original Tokyo segment hashes. Those witnesses prepare the previously registered 04:15–05:30 UTC comparison and contain no quote or trade extraction.

The original prospective Arcus ANT-USD and TradeXYZ xyz:ANT receive-clock holdout is 2026-11-20 20:00 UTC through 2026-11-21 06:00 UTC. It has 23 sealed public source segments, 49,629,910 compressed bytes and 304,408 capture records. The terminal tail of 599.738127916 seconds meets the 8.310-second execution evidence requirement. The signed admission freezes the actual decision-plus-arrival coverage of 1,359/2,400 and the one-second joint coverage of 21,877/36,000. Retain trade confirmations through the current subscription epoch, 18 late receipts, and original 310 ms execution rules. The frozen separate wallets and four 2x2 variants retain precise modeled fees, funding eligibility, closed cash and unknown terminal cash. The actual account fees and funding remain unobserved. The same aleph signed 23 source segments bind the pending Tokyo/aleph diagnostic without adding another sample.

### W13 bulk whole-object custody and ANT decision-clock attribution, 2026-11-21

W13 Tokyo bulk uses the existing 4,859,013,395-byte transfer, matching Bronze manifest and three byte-identical whole-file Drive readback receipts. The original history records default and HTTP/1.1 partial unexpected EOF attempts before the successful default retrieval. The Tokyo tombstone follows the verified receipt and matches byte size and archive SHA-256. Crypto and RWA remain separate pending Drive objects. Tokyo W14 and the ongoing W13 normalization observations record acquisition and processing health separately.

The already admitted ANT holdout reuses its immutable Arcus ANT-USD and TradeXYZ xyz:ANT receipts from 2026-11-20 20:00 UTC through 2026-11-21 06:00 UTC. All 23 source hashes, the original simulation and independent admission are bound. Retrospective attribution separates source-age validity, cross-venue source-clock divergence at decision, and the one- or two-venue shortage at arrival. Receipt availability precedes each coded choice. It observes the selected TradeXYZ REST or WebSocket book rather than either alone. The original 310 ms and separately registered 10 ms arrival clocks compare availability on the same decision grid. Quote coverage metrics imply no actual account fees, fills, funding or strategy returns.

### W13 complete Silver and exact latency recovery, 2026-11-21

The original W13 ingest binds all three previously sealed Tokyo archives, the three cold Bronze readbacks, the original Silver manifests and the original readiness receipts. The combined physical archive size is 18,155,300,875 bytes. Bulk, crypto and RWA cover about 10.25 hours each and retain their respective continuity flags and gaps. Five original combined GPU attempts failed in the DuckDB latency quantile query after complete Silver materialization. The linked W13 recovery retains the same pair compiler, source, frozen ledger parameters and window boundaries.

The one-thread, 6 GB exact diagnostic recomputes each original filtered quantile separately for each complete campaign, venue and domain. Its 16 groups retain all 266,577,958 BBO rows, with independently signed Silver counts matched for all three campaigns. The combined query, group-only failure, execution cap, equivalent synthetic fixture, complete low-memory result and canonical-lock recovery provenance are preserved in the original data-lake inventory. The GPU result and frozen finance receipts have distinct pending states. The operating evidence is in `.agent-state/forward_ops_20261118/w13_gpu_memory_20261121/REPORT.md`.

### W13 signed strategy receipts and separate Core trade-feed gaps, 2026-11-21

The completed W13 replay retains the original full three-stream Silver and frozen 14 plus 26 profiles. The signed 40-entry receipt binds the original market tape, parameters and ledgers. The new retrospective attribution consumes the matching Core BTC and ETH tapes and W13 Lighter public lifecycle Parquet. The clock bounds are the original continuous 10-hour decision interval, with 60 seconds of warm-up and 15 seconds of terminal drain. Each of the three native trade disconnect markers matches both tape clocks at millisecond resolution. Original book frames continue throughout each approximately one-second trade gap. Keep public market visibility, signature completion, continuity and verified cash distinct.

The same-window paired exit and ETH comparison receipts reuse their original frozen wallet and fee assumptions. Mark-to-book estimates use observed depth and declared 10 ms and 310 ms arrival clocks. No missing print, fee, liquidation or private order execution is imputed. The separate Arcus–RH lead/lag peer audit reuses the W13 selection diagnostic and clips no outcomes. Its fixture demonstrates that a stale received starting price can create a positive mixed-clock label even with zero forward exchange-clock move. The signed scope, code and report are under `.agent-state/forward_ops_20261118/w13_settlement_review_20261121/`.

### W09–W13 frozen paired exit arrival comparison, 2026-11-21

The retrospective arrival comparison uses ten signed single-fill and partial-fill public-book exit receipts, the five corresponding 40-profile authenticity receipts, and both exit method freezes. W09–W10 retain historical reuse, W11 retains enrollment after collection began, and W12–W13 retain enrollment before collection. Each cycle retains its original market, maker role, first modeled nonnegative trigger, next observed public quote at 10 ms and 310 ms, and coverage flag. Missing quotes remain unknown and the two methods remain separate because they can reuse the same original cycle. Artifacts and the first schema-probe version are in `.agent-state/forward_ops_20261118/w09_w13_exit_timing_20261121/`.

### W13 crypto Drive readback and Tokyo eviction, 2026-11-21

The existing gdrive workflow completed the full 6,096,334,732-byte W13 crypto readback on attempt nine at 10:28:47 UTC. Seven unexpected EOF attempts and one separately classified CLI error remain in the journal. The original source seal, the Mac, aleph and Tokyo receipt hashes, and the independently reread Cold Bronze object agree. The original Tokyo flywheel recorded eviction at 10:32:48 UTC with the same size and hash. RWA remains separately governed by complete readback. See the redacted custody evidence in `.agent-state/forward_ops_20261118/w13_crypto_custody_s19_r1_20261121/`.

At 19:15 UTC the original Drive CLI returned all 7,199,952,748 bytes of W13 RWA with the original SHA-256 on its thirteenth RWA readback. The previous twelve unexpected EOF results remain recorded. The window's three original archives total 18,155,300,875 bytes. A separate check reconciled all three Mac, aleph and Tokyo receipts with their whole-file readbacks and original transfer identities. Tokyo still held the RWA archive at the first observation after receipt publication. See `.agent-state/forward_ops_20261118/w13/verified-backup-summary.json`.

At 19:21:47 UTC the original Tokyo flywheel evicted W13 RWA. The independently observed tombstone matched the whole-file backup's 7,199,952,748 bytes and SHA-256. The archive was absent after the timer run, and free disk increased from 9.68 GiB to 16.37 GiB across the observations. The read-only before/after witness is in `.agent-state/forward_ops_20261118/w13/rwa-auto-eviction-witness.json`.

### Arcus historical market-screen exit-cost ceiling, 2026-11-21

The signed Arcus W02–W05 historical study reuses eighteen stock perpetuals per ten-hour window and keeps the original prior-window screen, public print and first-touch size gates, and one- and five-second receive-clock horizons. The new audit reads all 72 market-window outcomes at both horizons from eleven verified receipts. It recovers exit-to-entry notional from the original 1 bp exit haircut and applies an explicit zero-fee, zero-overhead counterfactual to the same executable-touch observations. All 144 original fee-model results remain negative. Keep hypothetical exit valuation, third-party prints and own cash execution distinct. The paired Mac and aleph outputs are under `.agent-state/forward_ops_20261118/arcus_screen_cost_ceiling_20261121/`.

A separate peer check binds the W14 composite gate specification and scorer hashes recorded during collection, before seal. It executes only the rank-correlation routine against tied synthetic scores and uses no W14 observations. The original scoring rule and candidate remain separate from the historical cost-ceiling audit.

### Frozen Lighter four-asset forward day 2, 2026-11-21

The existing seven-date protocol signed November 21 on attempt 1. Independent readback checked the original engine and decoder hashes, freeze before warm-up, the 4 anchor and 48 holding archives and each object's size and SHA-256. The first original-object receipt was 01:03:31 UTC on November 22, after the earliest 01:00 UTC retrieval for this date. All 52 receipts postdate their original recorded hour. Final and priced fingerprints, plus the separate balanced-pair and per-asset reports, are bound together in `.agent-state/forward_ops_20261118/s18_breadth_forward_20261119/verified-2026-11-21.json`. This completes attribution for two of seven preassigned dates. It preserves four independent 10-hour wallets and the original completeness dispositions.

### Tokyo W14 sealed source and original rollover, 2026-11-21

The original Tokyo collector sealed the W14 bulk, crypto and RWA public-book archives between 13:14:48 and 13:14:56 UTC. Their sizes are 4,520,775,195, 6,011,342,008 and 6,304,086,228 bytes, respectively. The original Mac coordinator began transfer to aleph at 13:15:25 UTC and the receiver lock was observed. The source remains in `selection_dev`. Coverage and gaps, full gzip verification, archive hashes, cold and Drive custody, and the frozen economic results each require their corresponding receipts. All three W15 streams followed in the original Tokyo schedule. W13 RWA Drive readback remains a separate custody operation. Observations are under `.agent-state/forward_ops_20261118/w14_preseal_r3_followup_20261121/`.

The subsequent transport audit compares one-minute Google Compute Engine CPU, disk-read and total network-egress measurements for the same Tokyo instance during the W13 and W14 transfers. These measurements include other instance traffic and do not contain market data or order latency. They accompany the public archive staging counters, three W15 growth observations and the original cleanup timer status under `.agent-state/forward_ops_20261118/w14_financial_gate_review_20261121/`. W14 remained in transfer at the observation time.

The transport diagnosis subsequently matched the three W14 readers to one SSH transport and captured repeated receive-window limiting while the archive readers waited on their output pipe. Fresh connections carried bounded synthetic payloads, and the original transfer process resumed from all three on-disk offsets after the slow connections closed. Later network interruptions left the offsets intact. The referenced evidence is operational and supplies no order or exchange latency measurements.

At 15:00 UTC all three W14 publisher manifests existed and matched the original archive sizes. The `selection_dev` capture hashes were `167f0b73c22750dd23dee0c50624f10ebd361bd5f2a1a1c6fcb087054315b767` for bulk, `d72f35978d186523a3d8cb0509951974e5d5caeb310afb6d336ca9b5d7633bfc` for crypto and `cac17bb32e1f01f7dbbcaa645661aa920c8dd71a45016b87b54f4d99808e8f7f` for RWA. The Tokyo RWA hash independently matches the source declared in the registered W14 composite gate report. At this observation the Mac transfer remained active and aleph had no final cold-storage receipt.

At 18:23 UTC the W14 transfer completed with 16,836,203,431 bytes. The independent receipt matches all three publisher hashes, exact per-stream sizes, complete gzip evidence, the Mac aggregate receipt and the aleph cold-store `selection_dev` classifications. The original W14 backup and frozen replay started from that transfer receipt. The data validation and financial results have their own completion markers. The independently reconciled receipt is in `.agent-state/forward_ops_20261118/w14/verified-transfer-summary-20261122.json`.

By 20:02 UTC the W14 bulk and crypto objects had completed the original whole-file Drive readback. At 20:10:04 UTC the original Tokyo timer evicted W14 bulk. Its 4,520,775,195 bytes and SHA-256 match the Drive and transfer receipts. Tokyo had 20.05 GiB free at the subsequent observation. The third whole-file readback and the full frozen strategy receipts still awaited completion. See `.agent-state/forward_ops_20261118/w14/bulk-auto-eviction-witness.json`.

At 21:06 UTC W14 completed the RWA whole-file Drive verification. The three original readbacks and Mac, aleph and Tokyo receipts agree on 16,836,203,431 bytes and the per-stream hashes. The complete three-stream Silver ingest was already signed. Five downstream DuckDB allocation failures came from one latency quantile query. The exact recovered query returned 16 groups and 247,023,986 BBO rows with the original filters and quantiles. Their per-stream counts match the signed Silver manifests. The original GPU checkpoint was regenerated and the same coordinator resumed the frozen replay. The backup and recovery receipts are in `.agent-state/forward_ops_20261118/w14/` and `.agent-state/forward_ops_20261118/w14_gpu_memory_20261122/`.

At 21:27 UTC W14 completed the original 14-arm replay. The independent zero-reward cash receipt on aleph has SHA-256 `83a15efae159aee79089a71852ab09cbb56efd1b6fea057b55e99a750e1a0ad4`. The supplemental S9 pair replay had stopped after five missing-engine errors. The original source and toolchain reproduced the registered executable `199ce175063ab2470a9bfcca1ff2c1ec4673c4d0ae503eb257c5b6b3ea6fc6fd`. The exact executable resumed the original supplemental coordinator, with its old failure receipts preserved. See `.agent-state/forward_ops_20261118/frozen_engine_recovery_20261122/S9-REPORT.md`.

At 22:17 UTC the original W14 reconciler signed all 40 frozen profile receipts for the continuous ten-hour decision unit, including the 16 recovered S9 pair arms, eight maker factor arms and two kappa arms. The marker SHA-256 is `78530211c3db9254564d788c8eb2a20a673207d975b6a53b9ffa4d3a96fd7f4c`. The existing audit retains the original per-arm evidence classes and unresolved outcomes. See `.agent-state/forward_ops_20261118/w14/verified-frozen-40.json`.

### Tokyo W15 sealed source and W16 rollover, 2026-11-21

The three Tokyo W15 source archives were sealed by 23:30:21 UTC, totaling 9,256,136,056 bytes. The Mac original transfer coordinator began at 23:30:35 UTC. By 23:32:17 UTC the original coordinator held its lock and was staging each stream. Each W16 output grew across two observations. The active Tokyo cleanup timer last reported success at the later check, with 18.14 GiB free. Cold and Drive receipts were pending at this observation. See `.agent-state/forward_ops_20261118/w15/seal-start-witness.json` for the three sizes, stage and publication evidence.

At 00:08 UTC on November 22 all three W15 Tokyo publisher manifests were available. Their sealed sizes still totaled 9,256,136,056 bytes, and the three publisher-declared archive SHA-256 values are recorded in `.agent-state/forward_ops_20261118/w15/publisher-witness-0808.json`. The original Mac transfer remained active, with 4,353,687,552 staged bytes. The independent cold and Drive receipts were still pending. All three W16 files continued to grow.

At 01:22 UTC on November 22, the W15 bulk archive was independently read back from aleph cold storage. Its 2,365,902,269 bytes and SHA-256 match the Tokyo publisher and the Mac staging receipt. The latter verifies a complete gzip EOF. The cold manifest classifies this source as selection_dev. The independently signed record is `.agent-state/forward_ops_20261118/w15/verified-bulk-cold.json`. This is the first of three W15 cold receipts. Drive and frozen replay remain queued behind the complete transfer.

On November 22 at 08:18 UTC, W15 crypto's 3,121,436,468-byte whole public object was independently rehashed in aleph cold storage. Tokyo's publisher, the Mac staging EOF proof and the aleph selection manifest agree. This is the second verified archive in the same original W15 campaign. RWA remained in transfer and the complete Drive and frozen replay receipts were pending. See `.agent-state/forward_ops_20261118/w15/verified-crypto-cold.json` and `.agent-state/forward_ops_20261118/w15/observed-stage-1617.json`.

### OKX native four-asset order books and realized funding, 2026-11-21

The official OKX 400-level USDT perpetual archive lists BTC, ETH, SOL and XRP on January 20, April 21, July 21, October 20 and November 17, 2026. The dates are the third Tuesday of the selected months. All 20 candidate objects returned a 128 KiB prefix with a native snapshot and updates. Record hashes, object names, sizes, publication headers and the publisher response hashes are under `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/`. The dates form an independent historical venue sample with a separate partition from the frozen Tokyo forward windows. [Official archive contract](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-historical-market-data).

The April 21 XRP archive is the first complete ingestion. Its 58,051,672-byte gzip archive is preserved in `bronze_vault/public_okx_swap_l2_20261121/2026-04-21/`, with SHA-256 `5f4299b1fbebd7fd0f2baac34e7dc4066d40cf87e747745dd163e7bcd3de1102`. Its independent 13:00–23:00 UTC unit contains 1,519,002 two-sided book observations. The maximum exchange-clock gap is 880 ms. Recording receive timestamps and sequence IDs are unavailable in the complete first object. The original continuous ten-hour unit is kept separate from the 07:00 signal anchor and terminal quote.

April 21 SOL is the second complete ingestion in the same cold directory. The 128,372,274-byte gzip archive has SHA-256 `8fe5ec07eee1ec40bd1b8e249d444cfbf1c75c87f8532dba76c4aaeb63f067e1`. The same 13:00–23:00 UTC interval contains 2,244,445 usable two-sided book observations, with a maximum observed exchange-clock gap of 610 ms. The full SOL object has the same five-field record structure and the same receive-time and sequence-evidence limitations. The archive was obtained from the official CDN after the interrupted download resumed.

April 21 ETH is the third complete ingestion. The 436,563,256-byte archive has SHA-256 `350194129d8997e3d5aaf934ae8e986f74080168c2eb5556ca6ced5580af8e57` and supplies 2,774,050 valid two-sided book observations in the same ten-hour interval. The maximum observed exchange-clock gap is 561 ms. An independent source-clock quote valuation was frozen before the BTC archive completed and before price results were read. It preserves the 07:00 and 13:00 UTC signals, the 13:00–23:00 decision window, the two existing equal-notional arms, and the momentum and reversal directions. It records settlement estimates using the book midpoint and treats contract execution feasibility and receive-time latency as unobserved. One retry of the previously observed April funding catalog returned HTTP 403, with the original funding timestamp and archive hashes retained. The method is under `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/`.

April 21 BTC completes the four archives at 1,132,534,871 total bytes. The BTC archive contributes 509,547,669 bytes with SHA-256 `3d85e3a4e4cb8cd1fb28d4918dad1400e2bddb4c21d5702f64bcdd68521bd972`, with 2,940,637 usable two-sided book observations and a 569 ms maximum observed exchange-clock gap in the decision interval. All four reach the same 07:00 anchor and 13:00–23:00 UTC interval. A preregistered projection restricted to the pre-funding trading cash retains the signed original method and provenance. It leaves funding and net cash unknown while the official catalog retry remains blocked.

The resulting source-clock valuation receipt binds all four whole captures and the frozen two long-short arms. It stores quote protection failures, pre-funding trading values, and turnover under a 1.25 bp cost scenario. The one-long/one-short and two-long/two-short accounts remain distinct. The next intake uses July 21 from the pre-existing date list because it has the smallest total archive size among the remaining dates. Each capture requires its own CDN prefix check, full gzip validation, and lakehouse hash readback. The observed price result and July source-selection receipt are under `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/`.

The July 21 four-asset intake is complete at 916,737,815 bytes across four separately hashed captures. The common ten-hour interval contains 8,404,644 usable two-sided observations. The maximum observed exchange-clock gap ranges from 520 ms for ETH to 1,310 ms for XRP. The July price-only replay uses the same two arms, cutoffs, latency assumption and quote protection. Its input and output receipts retain funding and net cash as unknown. Both April and July funding-catalog retries produced HTTP 403. The next intake follows October 20 from the same preregistered list because it has the smallest remaining archive size. The July proof is in `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/july-valuation/`.

The October 20 four-asset intake is complete at 1,333,952,077 bytes with four separately hashed archives. The continuous 13:00–23:00 UTC interval contains 10,082,223 usable two-sided observations. The maximum adjacent exchange-clock gaps for BTC, ETH, SOL and XRP are 463, 488, 765 and 970 ms. The same six source-clock quote samples per asset have a maximum observation age of 174 ms. Receive timestamps and continuity sequence IDs are unavailable. The historical quote valuation retains the registered 07:00 anchor, two equal-notional portfolios, 310 ms modeled placement delay, 3 bp protection, and 1.25 bp fee scenario. The official public funding history returned one actual settled rate per instrument inside the October decision interval at 16:00 UTC. It was observed after the window and is used only for settlement attribution. Raw public response hashes and book and valuation receipts are in `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/oct-valuation/`. The November 17 date is next by minimum remaining published archive size, 1,275.75 MB versus 1,453.89 MB for January 20. [Official funding history](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-funding-rate-history).

The November 17 four-asset intake is complete at 1,337,721,857 bytes and 10,421,486 two-sided book observations for the common 13:00–23:00 UTC unit. The per-instrument maximum adjacent exchange-clock gaps are 428, 403, 728 and 680 ms for BTC, ETH, SOL and XRP. The book archive lacks venue sequence IDs and collector receive times. Four independently stored official realized-rate responses supplied the 16:00 UTC settlement for attribution. All four registered directions and portfolios withheld total cash when at least one leg breached the original 3 bp quote protection. November archives, source qualifications and valuation receipts are indexed under `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/nov-valuation/`. January 20 is the remaining registered historical date.

The November diagnostic varied only the modeled source-clock delay in the existing order-book cutoffs. The 310 ms scenario reproduced the previously withheld legs individually, and all breaches occurred at the exit. At 0 ms and 10 ms all four portfolios passed the same quote protection. The variant is a post-outcome attribution on the same historical date. The original parameters, result and scope remain signed. The diagnostic and its freeze are in the November valuation directory.

The January 20 four-asset intake completed the registered five-date set. The four January books total 1,524,516,308 bytes and 10,850,537 usable two-sided observations from 13:00 to 23:00 UTC. Maximum adjacent exchange-clock gaps for BTC, ETH, SOL and XRP are 820, 806, 773 and 645 ms. Exchange sequence and collector receive times are unavailable. The January catalog reread produced no candidates. Earlier funding ZIP digests and their observed 16:00 UTC slot remain recorded, with the January funding cash unknown. The common 310 ms and 3 bp protection case supports 14 of the 20 independent date, portfolio and direction combinations before funding. All twenty archived books total 6,245,462,928 bytes. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/jan-valuation/`.

Four archives per month in January, April, July and October provide official settled funding file hashes. The November monthly index did not list the required object at observation time, and the public funding history independently supplied realized-rate fields for all four instruments. Each of the 20 instrument-days has a settlement at 16:00 UTC. Effective contract sizes, historical trading fees and attribution to held positions remain to be reconciled before economic replay. [Official historical funding endpoint](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-funding-rate-history).

The OKX open-instruments API returned the four linear USDT swap specifications at 22:14 UTC on November 21. The response hashes, HTTP dates, contract multipliers, lot sizes, minimum sizes and ticks are in `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/instrument_terms_20261122/`. All four observations postdate the five existing replay windows. Use the snapshot prospectively and bind each historical window to its own effective contract and fee evidence before converting quoted contracts into executable notional. Historical exchange announcements establish that contract face value and lot constraints can change independently. [OKX open-instruments API](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-instruments) [OKX minimum quantity adjustment](https://www.okx.com/en-gb/help/okx-to-adjust-the-minimum-order-quantities-of-spots-and-futures).

The five-date source-clock replay reused the original cutpoints, 3 bp price protection and four holdings decisions per date. It reproduced every original 310 ms leg decision. At 0, 10 and 310 ms, respectively, 18, 18 and 14 of 20 combinations passed full-leg protection. Four additional misses at 310 ms occurred exclusively at the November exit. The April slice has two quote-protection failures even at 0 ms. No quantity execution, account fee, or received-time simulation was inferred. The original 20 files and method revisions are bound by the protocol and independently signed completion at `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/five_date_quote_clock_20261122/`.

The OKX historical candlestick endpoint returned 17 confirmed 1-hour observations on each of the 20 linear USDT swap instrument-days. The 340 volume pairs provide exact volCcy / vol measurements in base units per contract. Cross multiplication, the candle price envelope, and the later open-instrument terms support 0.01 BTC, 0.1 ETH, 1 SOL and 100 XRP per contract across the five dates. These public observations qualify the book-quantity conversion only. The candle extraction followed the trading windows. Lot increments, minimum size, account fees and native receive-clock coverage retain their separate gates. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/historical_notional_20261122/`. [Official historical candlesticks](https://www.okx.com/docs-v5/en/#order-book-trading-market-data-get-candlesticks-history).

The displayed-depth follow-up replays each of the 20 exact native order-book archives. It converts contracts using the contemporaneous historical volume ratios and caps each entry and exit at 20% of the level quantities inside the original 3 bp guard. The two original leg sizes are $50 and $25. In the 0, 10 and 310 ms source-clock cases, the displayed-depth check retains all 18, 18 and 14 quote-qualified combinations. The smallest permitted displayed capacity exceeds their target by a factor of 66.02 at 310 ms. These are historical book-side capacity comparisons. Quantity increments, fees and receipt time still lack their own execution evidence. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/historical_depth_20261122/`.

A post-hoc linear capacity calculation lifts each original leg to 500 USDT at the same price, source-clock delay and 20% participation. All 18, 18 and 14 quote-qualified cases remain above the displayed capacity bound at 0, 10 and 310 ms. Their lowest capacity-to-target ratios are 20.598, 20.598 and 3.301. The projection does not model how the larger order changes displayed depth or market impact. Cash, order size restrictions and a replayed fill remain separate. The method and receipt are size_500.py and sizing-500-bound.json in the displayed-depth evidence directory.

The 16 previously inventoried monthly OKX funding archives for January, April, July and October were reacquired and matched the original ZIP SHA-256 and byte sizes. Every archive contains one 16:00 UTC settlement inside the matching historical decision window. All four October funding_rate values equal the separately preserved realizedRate API fields. The four November API witnesses complete coverage for the 20 instrument-days. Earlier failed requests and price-only outputs remain in the campaign. Settlement evidence postdates the decisions and is available for retrospective attribution. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/settled_funding_witness_20261122/`. [Official funding history](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-funding-rate-history).

The five-date funded quote valuation kept the original depth-group sizes, source-clock delays and 3 bp price protection. It replayed the same 20 native order-book archives and used the 20 verified 16:00 UTC realized rates. Simulated funding used the contemporaneous book midpoint for the held quantity. At 0, 10 and 310 ms, respectively, 18, 18 and 14 combinations were priced, of which 9, 9 and 7 were positive at 1.25 bp on entry and exit turnover. A zero-fee comparator produced the same positivity counts. The four additional November cases at 10 ms comprise two positive estimates and two other estimates. The 14 common cases preserve their signs across 10 and 310 ms. These sources qualify historical hypothetical cash at the model's quote and fee assumptions. Venue mark, actual fees and fills remain separate. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/funding_cash_sensitivity_20261122/`.

The matched OKX public mark-price API returned 40 confirmed one-minute candles across the same 20 instrument-days, covering [15:59, 16:01) UTC around the realized 16:00 funding settlement. Each public response was hashed and independently parsed. The post-window study retains the original quote eligibility, quantity, and 1.25 bp fee scenario with zero rewards. It replaces only the funding book midpoint with the 16:00 mark open and a sign-aware range using both bars. At 0, 10 and 310 ms, 18, 18 and 14 cases were priced and all 50 retained their signs throughout the range. Positive counts stayed at 9, 9 and 7. The largest modeled adjustment was less than 0.000882 bp per unit of turnover. The ten original exclusions remain withheld. Exact settlement-time marks, receive timestamps, executions, and account fees require independent evidence. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/funding_mark_witness_20261122/` and `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/settlement_mark_sensitivity_20261122/`. [OKX historical mark-price candles](https://www.okx.com/docs-v5/en/#public-data-rest-api-get-mark-price-candlesticks-history)

The five-date cash study adds a zero-reward fee sensitivity using all 50 original priced cases, preserving ten exclusions, 3 bp quote protection and the per-case funding mark bounds. OKX's published Lv.1 taker example of 5 bp on entry and exit turnover leaves 9/18, 9/18 and 7/14 positive estimates at 0, 10 and 310 ms. A fee-plus-additional-cost assumption of 8 or 10 bp reduces those counts to 8, 8 and 6. At 310 ms, reversal contributes 3 of the 7 positive 5 bp estimates. The account fee remains unknown. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/fee_headroom_20261122/`. [Official OKX fee formula](https://www.okx.com/help/how-to-calculate-the-contract-transaction-fee)

The Tardis `okex-swap/book_ticker` series supplies eight whole public native `bbo-tbt` files for BTC, ETH, SOL and XRP USDT perpetuals on June 1 and September 1, 2026. Registered source selection preceded requests. The eight objects in `bronze_vault/public_okx_tardis_native_bbo_20261122/` total 198,348,639 bytes. Full SHA-256, publisher MD5, verified gzip EOF, and all coverage exceptions are recorded. The two 13:00–23:00 UTC units have 7,581,734 valid two-sided updates in total and retain the vendor receipt timestamp and venue timestamp separately. The 07:00 rank anchor and terminal cut are separate. A paired 3 bp quote diagnostic retains the same 6/8 cases at each original delay on both clocks. Fifteen of 72 labeled source-clock samples take quotes before the vendor observed them. The receive-clock arm uses zero such samples. This source adds historical causal quote coverage. Funding, account fees, venue arrival and cash remain separate. See `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/vendor_bbo_clock_20261122/`. [Tardis OKX market-data contract](https://docs.tardis.dev/historical-data-details/okex-swap) [Tardis dataset API](https://docs.tardis.dev/downloadable-csv-files/api)

The June/September vendor-clock cash extension uses eight official OKX monthly funding ZIPs. Each BTC/ETH/SOL/XRP linear perpetual has one 16:00 UTC settlement on its retained 13:00–23:00 UTC date. Four September ZIP values exactly match the independent public `realizedRate` response. June has archived rates and no matched API rows. Sixteen confirmed one-minute mark candles provide an adjacent public mark range for each settlement. Historical marks and funding are applied to cash after the decision. This retains the original quote selections, equal notional, fees 0/1.25/5/6/8/10 bp on total entry and exit turnover, and zero rewards. At all three delays there are six priced cases, three positive momentum cases, three negative reversal cases, and two quote protection exclusions. Price and currency conversion remain hypothetical. Raw public ZIP and candle response hashes, the failed initial transport, the retry, and independent counts are recorded in `.agent-state/forward_ops_20261118/okx_native_coverage_20261121/vendor_bbo_funded_cash_20261122/`. [Official historical funding archives](https://www.okx.com/historical-data) [OKX funding calculation](https://www.okx.com/help/perps-funding-fee-mechanism)

A settlement-boundary sensitivity exercised both corrected ANT research runners and a separate candidate using synthetic six-event cases. The candidate settles before mutating event state and pairs the Arcus received book with its freshness clock. Replaying the already-used W01 and W02 public research archives, bound by 22 compressed-object SHA-256 digests, did not change realized modeled quote-cash to four decimal places, fills, or fees for 48 A2 and 24 CRASH-A2B alternatives in either historical selection unit or the additional stitched diagnostic. Funding witness counts stayed 10, 11 and 21. Model results and their chronology remain separate from regular-hours holdout coverage and actual settlement. See `.agent-state/forward_ops_20261118/ant_basis_settlement_boundary_20261122/`.

### W13/W14 Arcus–RH maker gate simulation review, 2026-11-22

The independent cost review consumes two signed `selection_dev` output files from the existing six-market original simulator. The two publication hashes bind the original Arcus and RH simulated notional, terminal marked equity, terminal positions, hedge retries, and sampled exposure. The review recomputes costs per million in Arcus and combined volume under all four previously recorded RH fee scenarios and solves the modeled fee crossover from the exact rounded input fields. A separate calculation retains the one-minute risk marks and measures consecutive sampled observations over the hedge trigger. No additional fills or continuity are imputed. Provenance and cold readback are under `.agent-state/forward_ops_20261118/w13_w14_maker_peer_20261121/`.

### W15/W16 Lighter Core RWA funding and native maker replay, 2026-11-23

The Tokyo RWA Bronze archives for W15 and W16 contain 40,830,164 and 42,947,778 public records. Their publisher SHA-256 digests are `d6d46fd9a7eefcb56b121ba870aaa068126a6b2b057b0b654249b383d0615118` and `befa8e0cf47baebf4c491d38a8bda1fe789e7ff08e186b19c035df1daeb076ca`. The retrospective replay uses each original ten-hour decision interval independently. W15 begins at 2026-11-21 13:20:58.307 UTC and W16 at 23:36:00.976 UTC on the same date. Both fall outside US equity trading hours.

Six unauthenticated Lighter Core `/api/v1/fundings` responses give hourly settlements for AAPL, AMZN, NVDA, QQQ, SPY and TSLA. Each market has ten settlements per window. The archive retains response bytes, request times, hashes, the source timestamps in seconds and their millisecond conversion. The settlement values and payer direction serve retrospective accounting only. For all 120 market-hours, the signed rate agrees with the first post-boundary public market statistic and the per-base amount agrees with the archived index times the published rate to within 0.000000005 USD. The last pre-boundary index and first post-boundary index agree in every case. Eighty-four funding timestamps in the first post-boundary statistics show one millisecond after the REST hour. Preserve model-position observations at either side of those boundaries. The source has no information about individual account fills, fees or cash transfers. [Lighter funding payment](https://docs.lighter.xyz/trading/funding#funding-payment) · [Official API reference](https://github.com/elliottech/lighter-python/blob/main/docs/CandlestickApi.md) · [Lighter account fees and delays](https://docs.lighter.xyz/trading/trading-fees)

### September–October VVV funding differential, 2026-11-23

Lighter Core VVV (market 69) and Hyperliquid VVV supply 1,464 matched hourly public settlement rates for September and October 2026. The retrospective screen selects the pair from the November venue audit. It fixes long Hyperliquid and short Lighter before reviewing historical rates. Lighter returns an unsigned hourly percentage and payer direction. Hyperliquid returns a signed fraction and millisecond event time. The source bundle preserves 19 public responses, request times, byte hashes, the initially clipped boundary hours, and the verified overlap used to restore them. The measure describes equal-notional funding differentials. Execution, basis, residual exposure and account fees remain unobserved. [Lighter historical funding](https://github.com/elliottech/lighter-python/blob/main/docs/CandlestickApi.md) · [Hyperliquid historical funding](https://hyperliquid.gitbook.io/hyperliquid-docs/for-developers/api/info-endpoint/perpetuals#retrieve-historical-funding-rates)

The multi-day carry diagnostic reuses those 19 archived responses without new requests. It independently partitions the 1,464-hour span from 2026-09-01 00:00 UTC into complete 10-, 24-, 48-, and 72-hour blocks, counting settlements after each block start through its end. It retains each grid's leftover hours and applies an illustrative 9 bp round trip once per complete block. All original 10-hour results reproduce exactly, and 494 overlapping source observations agree. The 24-, 48-, and 72-hour results are historical funding diagnostics with overlapping calendar periods across grids. The input hash, aggregation plan, and all complete and residual results are archived together. [Carry horizon report](../.agent-state/forward_ops_20261118/vvv_multiday_carry_20261123/REPORT.md)

### W09–W16 frozen BTC cost sensitivity, 2026-11-23

The review reuses the original cash, scope, and configuration receipts for the BTC quant control and local reversal arms. It retains each original ten-hour decision range, 60-second warm-up, 15-second drain, and exact source hashes. It compares the original closing cash with a fixed 0.25 bp charge on all turnover, 1 bp on taker turnover, and both charges together. The three windows with known cash reconcile with the original stressed cash to USD 0.00000001. The five other windows retain trade-feed, funding, and risk evidence with cash unknown. Independent BTC Kappa maker receipts are used solely to compare data coverage. [Source inventory, scope, and costs](../.agent-state/forward_ops_20261118/btc_kappa_maker_cost_review_20261123/REPORT.md)

### W15/W16 Core RWA market concentration, 2026-11-23

The calculation reuses the two original ten-hour W15/W16 simulations with the independently observed 15-second drain, six-market terminal receipts, and public settlement evidence. It excludes each of the six markets from each window and arm once and retains all 24 comparisons. Market-level inventory, capital, and fills come from the original independent model ledgers. It preserves the existing 0.25 bp charge on all executions and extra 1 bp on taker executions, then adds illustrative all-execution charges of 0.01, 0.025 and 0.05 bp. The inputs and six source receipts are verified by SHA-256. Each comparison is historical sensitivity of an existing model path. [Method and full results](../.agent-state/forward_ops_20261118/rwa_concentration_20261123/REPORT.md)

### Core RWA forward cash reconciliation and historical controls, 2026-11-23

The forward cash runner inherits the W18/W19 original eight-arm registration and its two independent ten-hour decision ranges. W15/W16 historical controls reuse the Tokyo Bronze archives, archived official hourly Lighter responses, and original replay summaries. The runner records simulated positions immediately before, at, and one millisecond after each settlement. It records backdated inventory changes and leaves cash unknown when they affect a settlement amount. Core terminal pricing requires a separate receive-ordered reconstruction from a snapshot and contiguous venue sequence on the same connection. The historical controls matched 120 public hourly settlements and 96 Core exits. Arcus and Robinhood hedge legs with modeled executions retain unknown settled economics pending independent external funding evidence. [Historical controls and source hashes](../.agent-state/forward_ops_20261118/rwa_w18_w19_cash_20261123/REPORT.md) · [Lighter funding payments](https://docs.lighter.xyz/trading/funding#funding-payment)

### RWA external funding coverage and BTC trade continuity, 2026-11-23

The W15/W16 external funding review reuses four frozen-model observation and settlement receipts. At each Core settlement grid it measures modeled Arcus and RH inventory separately, retaining all three timing observations. It audits the existing public archive registry and prior endpoint samples for the actual external settlement clock and market identity. Those sources do not cover all required external settlements. Full external-arm cash stays unknown, and the implied missing cash hurdles use the original static exits, known Core funding, and unchanged pressure costs. [External funding evidence and gaps](../.agent-state/forward_ops_20261118/rwa_external_funding_hurdle_20261123/REPORT.md)

The W13–W16 BTC continuity review reuses each signed continuous ten-hour Core BTC decision interval and its original warm-up and drain. The signed maker tape and BTC cash receipt identify when the trade feed became unavailable. The SHA-bound native Silver lifecycle independently corroborates all five offline starts in W13 and W16. Counts of compiled book frames and trade events describe each observed replay gap. Exchange trades missing from the tape and cash for the two affected windows remain unknown. [Scope, durations, and original receipts](../.agent-state/forward_ops_20261118/quant_btc_trade_gap_20261123/REPORT.md)

The repeated-observation review reuses BTC and ETH Core–Hyperliquid compiled BBOs for W13–W16, with eight separate market-window records. It verifies the compressed and decoded captures, the linked Bronze archive manifest, and the original window receipt. At fixed age thresholds of 100, 310, 500 and 1,000 ms it counts successive eligible frames with identical per-leg book timestamps and touches as the sampling clock advances. It retains same-timestamp price changes and per-leg regressions separately. The study does not compute order outcomes or cash from those frames. [Compiled-book provenance and counts](../.agent-state/forward_ops_20261118/kalman_historical_book_duplicates_20261123/REPORT.md)

Six newly archived public Arcus funding histories supply 120 in-scope rates for W15/W16. Each response retains its original bytes, market identity, epoch-microsecond hour, signed decimal rate, and request availability. The API contains rates without per-base paid amounts or the settlement oracle. A separate sensitivity combines the three equal modeled inventory observations with two scenarios based on the original modeled terminal price. Settled cash remains unknown. [Arcus rate provenance and estimates](../.agent-state/forward_ops_20261118/arcus_w15_w16_public_rates_20261123/REPORT.md) · [Arcus public rate endpoint](https://api.arcus.xyz/v1/fundingRates?market=SPY-USD&limit=96)

Six public Robinhood Chain funding responses add 120 in-scope W15/W16 RWA settlements and retain twelve preceding observations. Three native RH context partitions independently verify each payer, displayed percentage, and per-base quote amount from the RH settlement index. The RH source clock identifies 36 W15 and 12 W16 settlements one millisecond after their nominal hour. The original modeled inventory matches at all three observed time offsets. Conditional cash uses the original modeled orders and costs and values one RH quote unit at one model dollar. Actual account cash remains unknown. [RH source and conditional model](../.agent-state/forward_ops_20261118/rh_w15_w16_public_funding_20261123/REPORT.md) · [Lighter SDK funding response](https://github.com/elliottech/lighter-python/blob/main/docs/Funding.md)

An optional W18/W19 review uses the RH funding method after the original model and Core cash receipts are available. It binds the original inventory and funding review to the same sealed wave's RH Silver and separately witnessed RH settlement clock. The two historical controls reproduce all 120 W15/W16 points and four conditional baskets. Without independent RH terminal depth, RH-only inventory revision, or quote-to-USD conversion evidence, the review reports only the original terminal model and a parity sensitivity. W18 and W19 currently await their own inputs. [Method and validation](../.agent-state/forward_ops_20261118/rh_cash_overlay_20261123/REPORT.md)

### Historical latency interaction and frozen BTC clip comparisons, 2026-11-23

The latency comparison reuses the previously catalogued 592-run matrix, verifies 62 original baseline-equivalence marks and 248 resolved arm configurations, and compares 250/10 ms modeled outbound time by 100/10 ms modeled execution notice. Each four-cell comparison uses the original tape, continuous ten-hour window, modeled exchange delay, original fee and funding treatment, and 0.25 bp gross plus 1 bp additional taker stress. Tokyo, Taiwan and Tardis remain distinct capture cohorts. Incomplete cash and strict-book blockers remain in the ledger. An independent W15/W16 Core RWA comparison reuses the native depth and funding receipts to decompose 310-to-10 ms modeled cancellation under the original 10 ms maker-entry setting. Public source-to-arrival and control-frame RTT measurements provide no private order, cancel, or fill-notice clock. [Input proof and per-cohort results](../.agent-state/forward_ops_20261118/latency_factorial_cash_20261123/REPORT.md)

The BTC clip comparison uses W09–W16 frozen Lighter Core `turnB` selection outputs, with 24 same-window pairs across 10%, 25%, and 50% displayed-depth participation. The eight original scoring and input receipts, 48 resolved configurations, and six frozen templates are linked by SHA-256. Each pair changes only the base order notional from $500 to $5,000 and retains simulated realized fills, original risk stops, funding coverage, zero rewards, and the same turnover cost stress. The study retains nominal clip versus modeled maker turnover, the exact unit and denominator, PnL withheld for stopped runs, and the original source coverage at each window. [Roster, checks, and all outcomes](../.agent-state/forward_ops_20261118/turnover_clip_risk_20261123/REPORT.md)

### W17 Core maintenance and ANT single-leg repair, 2026-11-23

The W17 availability diagnostic binds the original public Tokyo Crypto Bronze archive, cold manifest, transfer and readiness receipts to the three original Core Silver public tables. It compares BTC and ETH tradable BBO and nonzero trade receipt gaps in the retrospective 2026-11-22 12:55–13:30 UTC clip, preserving publication clocks separately. The original systemd summaries independently agree with the native lifecycle disconnect counts and retain HTTP 503 and 502 handshake categories. The official 13:00 UTC upgrade is calendar evidence. The audit preserves source gaps, per-attempt versus continuous recovery, and readiness blockers. W18 journal coverage is bounded to the first 98 minutes and the original sealed W18 data remain pending. [Native evidence and method](../.agent-state/forward_ops_20261118/w17_core_maintenance_20261123/REPORT.md) · [Lighter network upgrade](https://t.me/lighter_announcements/490)

The ANT repair attribution reuses three original signed aggregate receipts from the formerly reserved ten-hour Arcus/TradeXYZ window of November 20 20:00 UTC through November 21 06:00 UTC. It matches the original 310 ms failed pair and the independently registered first allowed repair 15 seconds later, with the same exposure and arrival latency. The exact simulated entry and exit cash, modeled fees, and 0.25 and 1.25 bp notional pressures are retained. The 10 ms and 310 ms original wallets remain independent. Their conditional unwind-price equations preserve unresolved funding and original unknown terminal cash. [Input ledger, method and checks](../.agent-state/forward_ops_20261118/ant_repair_fee_attribution_20261123/REPORT.md)

### W17 frozen verification and Carry execution diagnostics, 2026-11-23

The original W17 standing regression completed 14 frozen profiles. Its four separately preregistered ETH arms used the common W17 crypto tape with an independent Binance anchor admission, 60 seconds of warm-up and 15 seconds of terminal drain. The review pins all three Bronze lanes, original custody, prepared and score receipts and native Silver lifecycle. It maps two BTC and ETH replay trade-coverage stops to the exact available timestamps of the native Lighter Core stream. Unknown economic cash, equity-session metadata blockers, terminal exposure and funding coverage are retained per arm. [Signed results and causal checks](../.agent-state/forward_ops_20261118/w17_frozen_signed_20261123/REPORT.md)

The BTC inventory-skew comparison reuses eight original W09–W16 ten-hour wallets for each frozen combination of 6 bp normalized reservation-price skew and markout quantiles. It pins the profiles, model reports, source tapes and clock receipts, preserving missing feed intervals and funding slots. Exposure, peak inventory, maker and taker capacity, fees and stressed cash use the original signatures and remain independent of the turnover clip study. [2×2 results](../.agent-state/forward_ops_20261118/btc_inventory_skew_factor_20261123/REPORT.md)

The Carry availability and direction audits recheck Gemini's 74-pair, 960-hour archive of 222 unauthenticated Arcus and Hyperliquid/TradeXYZ rates and the separately collected 151 public liquidity, market and context responses. The dynamic entry audit uses an isolated synthetic current-hour absence and checks 360 original portfolio calls with byte-exact outputs. The static direction audit chooses direction in the earlier 468 hours, preserves the 24-hour embargo and original later 468-hour scope, and charges whole episodes at the published model friction. The original funding archive and replay digest were verified in aleph. Point-in-time liquidity is a historical fee sensitivity. Realized cash, cross-venue basis and private execution remain unverified. [Funding-clock review](../.agent-state/forward_ops_20261118/carry_current_hour_gate_audit_20261123/REPORT.md) · [Static direction review](../.agent-state/forward_ops_20261118/carry_snapshot_direction_audit_20261123/REPORT.md)

The META, AMD and NVDA quote diagnostics use original W15–W17 RWA ten-hour decision intervals and native Arcus/TradeXYZ Silver snapshots. Available and source clocks are checked separately at each fixed minute or real TradeXYZ quote trigger, and 310 ms modeled arrival uses only previous observed publications. The $100 common-base size must fit both top sides. The same-book full taker crossing sensitivity includes the original 6.3 bp fee assumption. Each interval retains clock failures, quoted basis geometry and complete source fingerprints. Historical depth for the other requested pairs remains unverified in these three archives. [META](../.agent-state/forward_ops_20261118/carry_meta_liquidity_20261123/REPORT.md) · [AMD and NVDA](../.agent-state/forward_ops_20261118/carry_amd_nvda_liquidity_20261123/REPORT.md)

The derived Carry liquidity v2 re-evaluation (`carry_liquidity_spread_audit_v2`) consumes the identical SHA-256-verified 222 funding-rate receipts (`multiday_carry_universe_20261123`) and 151 public orderbook/context receipts (`carry_liquidity_snapshot_20261123`), replacing Holdout static sign selection with Train-frozen static direction (468h Train; `None` if Train is absent) and deducting point-in-time crossing costs across every modeled entry/reopen/exit episode (`11/35` crypto, `16/39` equity RWA; `11/35` and `12/39` static-only; `SNDK` static full-taker sensitivity `+6.0712 bp` over `2` episodes). Historical 40d realized cash (`null`) and outside-RTH equity depth caveats remain unchanged. [v2 report](../.agent-state/research/carry_liquidity_snapshot_20261123_v2_train_fixed/REPORT.md) · [v2 checksums](../.agent-state/research/carry_liquidity_snapshot_20261123_v2_train_fixed/SHA256SUMS)

The Carry clock review matches 148 of the 151 original public responses to 74 paired orderbooks and verifies each hash. It compares venue source timestamps and original HTTP receipt times separately. All 74 original pairs have source-time separation above one second. The saved snapshots support individual depth observations at their respective times. Synchronized basis, joint capacity and executable carry remain unverified. [Clock review and synthetic checks](../.agent-state/forward_ops_20261118/carry_liquidity_snapshot_clock_peer_20261123/REPORT.md)

The historical protected-open/close study reuses the three independent W15–W17 ten-hour RWA scopes, their original drain, and the signed native Arcus and TradeXYZ BBO partitions for AMD, META and NVDA. It records both decision schedules, source and availability clocks, each execution endpoint, and source-age rejection. The primary replay uses a 310 ms decision-age cap and simulated arrival delay, a one-second arrival-age cap, a $100 common-base target and a 3 bp price collar. A second historical sensitivity changes only the decision-age cap to one second. Both use original rates from the archived public series, an independently held same-market midpoint within 30 seconds of each funding hour, the original modeled fee with added cost stress, and USD/USDC parity for the estimate. Actual funding settlement prices, fills and economic cash remain unknown. [Plans, receipts and both result sets](../.agent-state/forward_ops_20261118/rwa_protected_open_close_stress_20261123/REPORT.md)

The W17 active ETH crash-pause review reuses the signed four-arm replay, its original ten-hour interval, warm-up and drain, and the immutable implementation blob. It separates rejected quote targets, actual simulated target-size reductions and original model risk metrics. The `CommonShock` aggregate includes different trigger and recovery branches, so its count leaves external confirmation and order outcomes unknown. Original partial funding and terminal exposure keep full cash unknown. [Source-bound attribution](../.agent-state/forward_ops_20261118/w17_active_crash_pause_target_attribution_20261123/REPORT.md)

The delayed-leg RWA review reuses the protected-open/close study's fixed W15–W17 endpoints for AMD, META and NVDA. Each protected opening or closing retains the original decision, same-base quantity, 3 bp collar, arrival-age cap and cutoff. One leg keeps the 310 ms modeled arrival and the other uses 0, 10, 50, 100, 200 or 310 ms of extra delay, using only BBOs received by each leg's own arrival. The review retains all failed endpoints, reuses the original fee and conditional funding only when settlement-hour coverage stays the same, and preserves unknown actual cash and unmatched position outcomes. Each original BBO partition and derived input is verified by SHA-256. [Delayed-leg protocol and verification](../.agent-state/forward_ops_20261118/rwa_fixed_endpoint_delayed_leg_20261123/REPORT.md)

The maker markout review reuses 16 original Sprint10 BTC and ETH control wallets from eight W09–W16 ten-hour decision windows, with independent inventory and cash per wallet. It checks the original simulated maker fill-notice clock, 1/5/30-second reference-price observations and label coverage against reported fill counts and notionals. Original funding, terminal evidence and known cash gates are preserved. The stress amount adds 0.25 bp on total simulated trading and 1 bp on taker trading with zero incentives. Ten wallets retain unknown cash due to incomplete original public trades. [Original source pins and complete results](../.agent-state/forward_ops_20261118/maker_markout_horizon_cash_20261123/REPORT.md)

The independent Carry v3 review reproduces the original release and checks the public peer book's echoed market identity. All 74 archived peer books contain the expected symbol. A nonempty Hyperliquid or TradeXYZ book with a different or absent `coin` now rejects before pricing. The original v3 archive, report and checksum manifest reproduce byte for byte with this check. The review also records how source age at each receipt can differ from source age when both books first become visible. [Independent vectors and identity patch](../.agent-state/forward_ops_20261118/carry_liquidity_v3_peer_20261123/REPORT.md)

The Carry clock-gated downgrade (`carry_liquidity_spread_audit_v3`) re-evaluates the exact same 222 funding receipts (`multiday_carry_universe_20261123`) and 151 public orderbook/context receipts (`carry_liquidity_snapshot_20261123`) while enforcing strict integer source stamps (`type is int and > 0`), timezone-aware HTTP receipt times (`received_at_utc`), dual-leg venue availability (`Arcus ONLINE` and HL/XYZ `!isDelisted`), and three-clock joint bounds (`max_source_age_ms=1000`, `max_pair_skew_ms=1000`). Joint simultaneous depth, basis, friction, and hard-gate pass counts are formally downgraded to `0/35` Crypto and `0/39` Equity RWA (`74/74` pairs exceed `1000 ms` source/receipt separation from unaligned v1 fetching), with simultaneous joint metrics withheld (`null`) while single-leg depth counts (`20/35` & `31/39` Arcus L1 `>=$100`; `35/35` & `39/39` HL/XYZ L2 `$100`) and historical 40d cash (`null`) remain preserved. Synchronized per-pair live fetching is staged for US RTH open. [v3 downgrade report](../.agent-state/research/carry_liquidity_snapshot_20261123_v3_clock_gated/REPORT.md) · [v3 checksums](../.agent-state/research/carry_liquidity_snapshot_20261123_v3_clock_gated/SHA256SUMS)
