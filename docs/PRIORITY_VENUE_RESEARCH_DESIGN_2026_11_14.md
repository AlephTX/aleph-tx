# 主平台研究设计：Binance 辅助决策与三类独立目标

日期：2026-11-14。代码基线：`06995343f8f41c6702a75c12c1d1914244c1bdf1`。

本文记录新的研究设计和当前官方规则核查，不是新的回测结果，也不是已冻结的新实验配置。当前资料在 2026-11-14 查询；Arcus、Entropy、TradeXYZ 公共元数据读取时间为 06:35–06:36 UTC。当前规则没有明确历史生效时间时，不回填到旧窗口。

## 1. 固定研究范围和目标

主平台是 **Arcus、Entropy、TradeXYZ、BULK、Lighter Robinhood**。激励候选优先核查 Arcus、Entropy、TradeXYZ、Lighter RH；BULK 的现行比赛、Maker 返费和尚未启动的计划分开。Lighter Core 与原生 Hyperliquid 可以作对冲或对照，Extended 保留作历史基线。平台排序由目标、经济机会和可确认奖励决定，数据缺口进入待办，不用已有回放适配器替代平台选择。

| 场景 | 独立策略族 | 主要评价量 | 不足以证明成功的结果 |
| --- | --- | --- | --- |
| 量化盈利 | 条件化做市、短期趋势/反转、同标的跨市场领先信号 | 全部退出后的净现金收益、回撤、资本占用和容量 | 未知空投估值、少量成交的正 markout、单窗口最佳参数 |
| 对冲套利 | 可执行价差、基差收敛、资金费差；RWA 单独处理 | 双腿完整现金流、未对冲暴露、资本时间、强制退出成本 | 中间价偏离、单腿收益、当前资金费的全年外推 |
| 激励产出 | 符合现行规则的真实交易、报价服务、持仓/抵押品；可用跨平台对冲控制风险 | 相同损失预算下确认产出、相同确认产出下现金成本、产出/资本时间 | 总成交量代替合格量、积分代替现金、把未知资格记为 100% |

**激励研究允许明确且可控的交易损耗。** 是否亏损与是否符合奖励资格是两个问题。已实现奖励价值按实际到账记录；未知积分兑换价值不加入现金收益，但不因此把允许的交易损失预算设为零。

三类程序保持独立配置、状态、资本、损失预算、账本和报告。共享行情不允许在结果中跨程序补贴。中性、先多、先空仅是入场姿态，不能代替平台策略族。

## 2. 平台 × 标的 × 场景 × 奖励规则

下表的“优先”是研究排序，不是已验证的交易建议；实际账户权限、等级、规则版本和资格均须单独取证。

| 平台 | 第一批标的或配对 | 量化/套利研究 | 激励状态与产出证据 |
| --- | --- | --- | --- |
| Arcus | BTC/ETH 作 Binance 信号研究；SNDK、NVDA 等 RWA 单独建组 | BTC/ETH 条件化做市；RWA 真实报价服务、库存与同股跨平台风险控制 | S1 Points 活跃；官方 FAQ 当前明确没有 token。积分考虑交易质量、持续报价、留存抵押品等，权重不公开；主要为积分而作的对冲/循环活动可能被取消资格。不能把循环量预认证为奖励量。[A1–A3] |
| Entropy (`io`) | `io:SNDK`、`io:NVDA`，AAPL 作候选对照 | 与同股 XYZ 的基差、时段和资金费差；条件化报价 | 已确认 Early bird 自返费计划；返费比例作用于 Entropy 的费用份额。需建号、等级和领取证据。自身 token/points 计划本轮未证实，不记作确定无币或确定有积分。[E1–E3] |
| TradeXYZ (`xyz`) | `xyz:SNDK`、`xyz:NVDA` 等同股；商品另列 | 主平台量化和相对价值研究；可同时承担其他平台的对冲腿 | 当前查到费用、合约与时段规则；自身 token/points 奖励未核实。保留在激励核查队列，现阶段不为未知奖励增加交易量。[X1–X4] |
| BULK | BTC/ETH；有完整同标的规则后再扩展 TradFi | 条件化做市、跨市场价格信号、完整资金费差 | 当前 Challenger Series 有 Trading AURA 和 USDC 奖池，但排除做市商。已生效 Maker 返费按实际资格/入账计算；Alpha Program 尚未启动。token 发行状态未获充分官方证据。[U1–U3] |
| Lighter RH | BTC/ETH；RWA 独立配对与单位核查 | 本地报价配 Binance 辅助过滤；RH/Core 相对价值 | 活跃 Live/Weekly Points；LIT 已存在，不能归类为等待 RH 单独发币。Standard-to-Standard 不计分；API 量不能假定 Wallet 2x。按实际积分变化、周度结算、可撤销状态分层记录，不能认定每笔都合格。[L1, L2] |

### 2.1 会改变成本模型的当前规则

- **Arcus 按市场收费。** 公共 `/v1/markets` 当前 BTC/ETH 基础 Maker/Taker 为 150/450 ppm，即 1.5/4.5 bps；NVDA/SNDK/AAPL/SPY 为 0/225 ppm，即 0/2.25 bps。按实际市场 schedule 和账户 tier 读取，不能全平台套零 Maker 费。[A4]
- **Entropy 自返费。** Trader/OG/Preferred/Partner 表列 60%/120%/160%/200%，分母是 Entropy 的 HIP-3 费用份额，不是全额手续费。高级别不可默认拥有；返费与 referred-user benefit 不叠加。最低领取 $5；原文 `bi-weekly` 的确切日历仍待确认。分开记录已付费、估计返费、批准返费和实际领取。[E1]
- **HIP-3 Growth Mode 按市场判断。** Entropy 当前非 delisted 元数据均有 growth 标记；XYZ 并非所有市场都启用。增长模式同时影响费用、返费和 Hyperliquid 统一 14 日费率等级的交易量权重；不能只下调收费却维持该等级的完整贡献。Entropy 自返费计划另设的 30 日门槛是否同样加权，本轮尚未确认。元数据仅证明当前配置，不证明实际可下单或历史配置。[E2, X1]
- **BULK 的三类激励不能混加。** 当前 Growth Mode Maker 费为零；Maker 返费另按市场份额及可用返费池结算；比赛资格排除做市商；尚未启动的 Alpha Program 计分不进入当前收益模型。[U1, U2]
- **RH Hedge Boost 有方向和期间。** 官方 W13/W14（11 月 6–19 日）表列 Core short / RH long，满额 2.5x，适用 weekly points，部分对冲按比例。它支持研究持仓时间与对冲成本，不支持将所有循环或中性/先多/先空一律乘 2.5。[L1]

Arcus Annex C 明确包括主要为积分而进行的 delta-neutral、hedged 等活动；RH 条款也排除人为量、同控互成交和以操纵积分为目的的参与。正常风险管理是否获奖仍取决于平台规则和审查。研究不设计自成交或规避资格识别；未知资格保留未知。[A3, L1]

## 3. Binance 的角色：辅助参考输入

**优先检验 Binance 是否改善主平台的实际决策。** 不默认将它设为唯一公允价、下单场所或对冲腿。Core-only 在旧窗口胜出，不能排除 Binance 的用途；旧回放主要比较本地与 Hyperliquid 参考。

Arrakis 的 16 日样本截至 2026-02-26，报告 Binance 在 29 个币中领先 Lighter 23 个。其 100 ms 搜索步长、成交价格统计和所研究平台，不证明 Oasis 收到信号后仍可盈利，也未直接研究全部五个主平台。[B1]

### 3.1 四组消融实验

在每个具有合格同标的参考的“平台 × 标的 × 场景”内比较，保持资本、规模、库存限制、手续费、执行路径、退出和窗口相同。

| 组别 | 定价基础 | Binance 的作用 | 主要问题 |
| --- | --- | --- | --- |
| A：本地基线 | 本地 | 仅记录 | 本地机制的净结果是多少？ |
| B：风险过滤 | 与 A 相同 | 仅对增加风险的方向过滤、减量或撤单 | 是否减少不利成交，付出了多少成交机会成本？ |
| C：受限调价 | 本地加有上限的修正 | 仅使用基差校正后的参考偏差 | 小幅参考调价是否改善完整交易现金流？ |
| D：两者结合 | 与 C 相同 | B 的过滤叠加 C 的调价 | 两种机制互补，还是重复减少成交？ |

**首先比较 A/B。** C/D 是独立候选，不预设更复杂就更好。过滤不得阻止必要的降风险和退出订单。参考断流、过期、乱序和基差尚未预热时采用何种行为，必须在验证前写入配置，不能事后挑最有利 fallback。

对三类场景分别使用：量化辅助报价/方向/短期信号；套利辅助区分全市场共同变化与平台特有偏离；激励减少逆向选择，同时量化其对合格产出速度的损失。

### 3.2 最少特征与基差

第一阶段只用 Binance 同标的永续 BBO、本地 BBO、两边的实际可用时间、短窗口 Binance 价格变化、慢基差和库存状态。深度、microprice、成交方向压力逐项做增量实验，避免同时改变多种机制。

先统一合约单位和计价资产，再用此前已可用数据估计：

\[
b_v(t^-)=\operatorname{EWMA}\left[\log\left(m_v/m_B^{\mathrm{同计价单位}}\right)\right],\qquad
\widehat m_v^B(t)=m_B^{\mathrm{同计价单位}}(t)e^{b_v(t^-)}.
\]

当前参考偏差可以用于 B 的方向风险过滤，或 C 的受限报价修正。平滑参数、偏差阈值、调价上限、预热和重置规则在选参集确定，验证前冻结。不能用未到达的本地报价更新基差。

USDT/USDC/USDG 的兑换、资金转移和结算成本分别记录。没有可靠转换数据时，平价只能作为明确标识的情景，不当作已验证收益。BTC/ETH 的 Binance 参考不替代 NVDA/SNDK 的股票价格参考。

### 3.3 数据与因果时间

官方优先接口为单币 `@bookTicker`（实时），可选 `@depth@100ms` 和 `@aggTrade`（100 ms 聚合）。全市场 `!bookTicker` 当前标示 5 秒，不用于快速参考。`E`、`T` 与本机接收时间含义不同；depth 按序号保持连续，缺口后重新同步。聚合成交不能当作独立逐笔数量。[B2–B4]

同一研究机为每条记录保留：

`venue / domain / native_symbol / channel / raw_source_times / receipt_wall / receipt_monotonic / available / sequence / connection_epoch`

策略回放按当时可用时间与稳定序号推进。晚收到的数据不能按其源时间挪回过去。历史下载可作微观结构探索，不能冒充 Oasis 当时已经收到的信息。writer 的 availability 与 socket receipt 也不能混称网络延迟。

按平台测量新单生效、撤单生效、对冲完成三个时域，报告 p50/p95/尾部。100/250/500/1,000/5,000 ms 只是信号诊断网格；关键是信息在实际动作生效时是否仍有用。只有 100 ms 预测力、实际撤单需 550 ms 时，不可直接宣称能够避免成交；550 ms 本身也只是待测参数示例。

### 3.4 当前代码缺口与后续实现边界

已有 `feeder/exchanges/binance.go` 的 BBO 适配器，但 `u` 未保留，`E` 没进入下游，缺少独立 receipt/availability 归档；无效 `T` 当前会回退本地时间。`perp-capture` 没有 Binance 采集入口，历史 maker campaign 固定 Lighter/HL，外部参考中位数也不等于独立 Binance 消融。

后续实现先补公共归档及时间语义，再接离线参考特征和 A/B/C/D。参考缺失不写成新鲜价格，断序不静默跨越。先保持共享内存 ABI 和线上策略不动；确认研究结果后再单独决定生产接入。本文不声称已经完成 Binance 归档或四组回放。

## 4. RWA 的独立研究组

优先核验 `io:SNDK ↔ xyz:SNDK`、`io:NVDA ↔ xyz:NVDA`，再评估 Arcus 同股腿；它们是配对候选，不因 ticker 相同就自动通过资格。

每份合约登记底层资产、每张经济单位、报价币、结算币、乘数、股息/拆股/公司行动、oracle、交易时段、价格带、保证金和最终退出条件。按美国交易日历及 `America/New_York` 处理 DST，不写死 UTC 时段。

- Arcus RWA 将 04:00–20:00 ET 与其他时段区别处理；Entropy 使用标的交易所时段与独立场外机制；XYZ 美国股票含隔夜外部参考。相同时刻的价差可能来自不同 oracle/时段规则。[A5, E3, X2]
- 当前 Entropy 普通股票资金费倍率在时段内/外为 0.5/0.125；XYZ 普通合约通常为 0.5。资金费差是合理筛选方向，但要计算每次实际结算、费率变化和双腿退出，不能从单次预测值锁定未来收益。[E3, X3]
- Arcus GLD/USO 等 ETF 与 XYZ GOLD/WTIOIL 等基准，不自动当作同单位标的；跟踪误差与展期必须显式建模。[A5, X4]
- XYZ `SKHY` 的 ADS 与 `SKHX` 韩国普通股不同；`io:ANTH` 市值单位与 `xyz:ANT` 每股单位不同。未完成转换与公司行动映射前拒绝按 1:1 配对。Pre-IPO 和 HIP-4 Events 不混进普通股票永续组。[E4, X4, X5]

### 4.1 本地接入补充：Arcus 股票现货抵押组合

按本轮讨论，股票代币现货加同标的永续空头保留为**低频、半自动的对冲套利候选**，暂不纳入近期全自动套利主线。现货通过独立 Spot Router 获取报价、钱包 EIP-712 签名、提交及查询结算；现有永续 API 接入不能代替这套执行路径。官方要求单独申请 Router 集成 API key，我们尚未验证具备权限或程序化签名、抵押到账及失败恢复能力。[Spot Router](https://docs.arcus.xyz/concepts/spot-router) · [集成要求](https://docs.arcus.xyz/concepts/spot-integration)

研究需单列现货可成交报价、股票代币换算系数、动态 USDG 借款利息和抵押清算约束。两腿没有共同的原子成交保证；现货买卖及抵押确认、永续对冲和退出分别取证。现货人工确认、永续程序化对冲只是候选执行方式，本次没有实施。奖励资格与这条组合的现金收益仍分别核算。

## 5. 评价口径与奖励账本

### 5.1 三类共同的现金边界

完整账本包含双腿交易现金、实际手续费/返费、资金费、可核实的计价转换、退出、请求配额等费用。未完成退出、断流、缺失资金费边界和未核实费用分别使结果不完整，不把缺失值填零。Markout、相对提交价滑点是诊断，不在已实现损益之外再次相加。

量化主要比较净现金收益与回撤；套利主要比较成对完整现金流与残余风险。零入场是有效的门槛结果，但不证明平台所有策略都没有机会。

### 5.2 激励的三个不同证据层

1. **目标活动**：成交量、持仓时间、有效挂单时间等可观察行为。
2. **规则下可确认的合格活动**：账户、产品、订单来源、对手等级、期间和排除项具备证据。任一关键字段未知时，合格量仍未知。
3. **平台积分/返费/奖励**：观察到的 credited、批准、可领取、已领取与事后调整分别记账。积分可被撤销，credited 不等于不可撤销现金。

只有规则明确奖励持仓或挂单时，才把相应持仓/深度时间指标作为候选产出，不能把普通 OI 小时伪装成官方计分量。周池、排名和竞争状态使“每美元固定积分”一般不成立。

在成本预算 `B` 下比较确认产出 `Q`，或在相同 `Q` 下比较净成本 `C`。两腿资本时间为实际占用资金的时间积分，不用名义交易量代替。

旧 RH 研究给出的 1.336–1.651 bps 等于每百万**目标**成交量约 $133.6–$165.1；这些是用户提供的筛选结果，本轮未重跑。若最终确认合格比例为 `q`，相同成本下每百万合格量成本为目标口径成本除以 `q`：

| 假设合格比例 | 每百万合格量成本 | 证据含义 |
| --- | --- | --- |
| 100% | $133.6–$165.1 | 情景，不是当前资格结论 |
| 50% | $267.2–$330.2 | 情景 |
| 20% | $668.0–$825.5 | 情景 |
| 0% | 无合格产出，单位成本无定义 | 不用除零或填零美化 |
| 未知 | 未知 | 当前不能由公共对手行情推断 |

确认积分数大于零后，`现金净成本 / 确认积分` 是单位成本或所需最低兑换价值；不预设未来 token 价格。10 小时可验证执行成本，周奖励还需要覆盖该周及实际结算，不能用一次 10 小时替代。

RH 可以直接增加只读积分归因：官方 WS `live_points/{ACCOUNT_ID}` 与 REST `livePoints/total` 提供累计 live points；适配器应保留小数，不能照文档类型标题截为整数。按账户、订单来源、等级、标的、时间与现金账本匹配，剔除推荐和其他同时活动的归因污染；该通道不代替 weekly points 或最终奖励记录。账户鉴权数据及响应保持在 Git 之外，本次只核对公开接口说明，没有读取用户积分。[L3]

## 6. 执行顺序与验证协议

1. **保持既有验证原样。** 用户报告的 2026-11-14 13:00–23:00 北京时间六组冻结实验，继续作为原实验；本次未连接 Oasis、重启采集或改动其参数/二进制。其结果不能改名成 Binance 验证。
2. **补主平台证据。** 将当前规则版本、合约映射、活动期间、实际费率、数据连续性、成交/资金费接口和奖励字段登记入下一轮 campaign。账户数据及原始成交证据保存在 Git 之外；本文只有公开来源。
3. **同机前瞻采集。** Binance 与主平台独立记录真实接收/可用时间。Arcus/BULK 若仍缺完整成交流，可先研究信号，不从 book 变动伪造成交。BULK 官方已提供 trades、funding 和分页账户历史接口，接入时核对环境和 coverage。[U3]
4. **选参后冻结。** 按平台/标的/场景分别定义 A/B，保留 C/D 方案；固定阈值、费率依据、延迟分布、基差预热、断流、退出、资本/损失预算、主要指标和淘汰标准。记录二进制、配置、数据、规则快照的 hash。
5. **新的连续十小时。** 起点须在冻结之后，预热和终止排空单列，禁止短片段重置拼接。完整保存所有变体、无成交和失效结果。共享同窗可减少环境差异，但不同策略状态及排队应独立，不让一个实验改变另一个的可成交量。
6. **先证经济效果，再扩展。** 看 A/B 的完整现金、风险和产出变化。样本不足时按预先说明的规则增加独立连续窗口，不反复调同一验证集。必要时用时间分块不确定区间；不把相关的逐笔成交当独立样本。奖励结果按实际活动周期补充。

晋级不依赖单个最优数字：量化需不依赖未知奖励的净收益和可接受尾部风险；套利需完整双腿现金流与可处理的退出；激励需真实资格/产出证据以及预算内的成本前沿。参考过滤后成交骤减时，必须同时报告成本和产出速度，不能只挑更好的平均 markout。

## 7. 本次已完成的代码工作

本次完成的是审计基础修复：撤单时间归因诊断、Lighter 公共执行时间字段保留、密封归档的只读时钟审计、精确现金总账与费用分解一致性。未修改策略决策或执行撮合模型，未接入新的 Binance 决策逻辑，未更改已有六组参数。详细边界和验证见 [执行与现金审计](EXECUTION_CLOCK_REVIEW_2026_11_14.md)。

## 来源

所有链接在本次查询时读取；只有明确注明的规则期间可用作历史生效证据。

- [A1 Arcus Points](https://arcus.xyz/blog/introducing-arcus-points)
- [A2 Arcus token FAQ](https://help.arcus.xyz/faq/general-faq/general/does-arcus-have-a-token.md)
- [A3 Arcus Protocol Terms，Annex C](https://arcus.xyz/legal/protocol-terms)
- [A4 Arcus fee model](https://docs.arcus.xyz/concepts/perpetuals/fees.md)；[当前 markets 公共 API](https://api.arcus.xyz/v1/markets)
- [A5 Arcus RWA](https://docs.arcus.xyz/concepts/perpetuals/real-world-assets.md)；[funding](https://docs.arcus.xyz/concepts/perpetuals/funding.md)
- [E1 Entropy rebates](https://docs.entropy.io/about-entropy/referrals)
- [E2 Entropy fees](https://docs.entropy.io/equity-perp-mechanics/fees)
- [E3 Entropy funding](https://docs.entropy.io/equity-perp-mechanics/funding-rate)；[oracle](https://docs.entropy.io/equity-perp-mechanics/oracle-price.md)
- [E4 Entropy Pre-IPO market type](https://docs.entropy.io/market-types/pre-ipo-perpetuals.md)
- [X1 TradeXYZ fees](https://docs.trade.xyz/perpetuals/mechanics/fees.md)
- [X2 TradeXYZ US stocks](https://docs.trade.xyz/perpetuals/markets/stocks/us.md)
- [X3 TradeXYZ funding update](https://docs.trade.xyz/perpetuals/changelog/funding-rate-formula-updates.md)
- [X4 TradeXYZ specifications](https://docs.trade.xyz/perpetuals/specifications-and-schedules/specification-index.md)；[Pre-IPO specifications](https://docs.trade.xyz/perpetuals/specifications-and-schedules/pre-ipo-specification-index.md)
- [X5 TradeXYZ products](https://docs.trade.xyz/)
- [U1 BULK Challenger Series](https://docs.bulk.trade/bulk-exchange/aura)
- [U2 BULK fees / Alpha Program status](https://docs.bulk.trade/bulk-exchange/fees)
- [U3 BULK API reference](https://docs.bulk.trade/api-reference)
- [L1 Lighter RH Points](https://docs.lighter.xyz/programs/lighter-on-robinhood-chain-points)
- [L2 LIT Utility](https://docs.lighter.xyz/about-lighter/lit-utility)；[Robinhood 官方 LIT 社区奖励公告](https://robinhood.com/newsroom/robinhood-accelerates-global-expansion-robinhood-chain-mainnet-stock-tokens-agentic-trading/)
- [L3 Lighter WebSocket live points](https://docs.lighter.xyz/api/websocket)；[Points API](https://docs.lighter.xyz/api/points)
- [B1 Arrakis price-discovery research](https://arrakis.finance/blog/crypto-price-discovery)
- [B2 Binance public streams](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/ws-streams/public)
- [B3 Binance market streams](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/ws-streams/market)
- [B4 Binance local order-book procedure](https://developers.binance.com/en/docs/products/derivatives-trading-usds-futures/websocket-market-streams/How-to-manage-a-local-order-book-correctly)
