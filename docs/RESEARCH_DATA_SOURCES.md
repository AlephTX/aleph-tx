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
