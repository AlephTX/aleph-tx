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
