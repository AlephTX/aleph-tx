# ANT 活动研究复核与 48 小时实施计划

日期：2026-11-20。状态：P0，第一批工程已实现；尚未完成实盘准入。
用户指定未来两天优先完成 ANT。oasis-agent、MAC/on-chain、Extended、新机器采购均降级；Tokyo 采集与回传仍由同事负责。本轮只读官方公开资料和现有代码，没有交易、账户操作、部署或重启。

## 1. 研究结论与报告处理

已阅读全文《Anthropic（ANT）上市跨所套利与积分最大化战略：aleph-tx v6.0 架构演进与 oasis-agent 协同指南》的正文和表格；页面四个图表 iframe 未提供可读内容，未取得图表模型或模拟源代码。分享链接及阅读凭据仅保留在忽略目录 `.agent-state/research/ant_p0_20261120/report-review.json`。

**值得立即推进的是 Arcus ANT / TradeXYZ xyz:ANT 的独立现金套利研究与执行保障；报告的收益预测不能当成验证结果。** 收敛到一个市场对、现有适配器和真实成交后的对冲，不重写整套架构。截止时间应推动交付，不能替代现金核账。

| 报告主张 | 复核结果 | 本轮决定 |
| --- | --- | --- |
| ANT 已 IPO，有两周优惠窗口 | Anthropic 官方定价公告及 Arcus 11 月 19 日上线公告相互支持；Arcus 宣布两周 taker 减费、正 maker 费归零及积分加成，但未给精确 UTC 截止或积分倍率 | 活动真实；12 月 3 日只能作规划日期，不能硬编码为官方截止 |
| ANT Tier 0 taker 为 0.5625 bp | 本轮两个公开 API 同时返回 112 ppm，即 **1.12 bp**；maker 0 | 使用实时市场费率，不将 RWA 历史折扣再乘活动折扣 |
| HL growth mode 为 0.3 / 0.9 bp | xyz:ANT 元数据 growthMode=enabled、deployerFeeScale=1；0.3/0.9 是 Tier 0、无额外账户优惠和 aligned quote 调整的情景 | 已有 HIP-3 费用实现可复用；实际账户费用仍需核实 |
| 盘后保证金 15%，价格带 ±7.5%，扩展等一小时 | ANT 是 FAST；初始带宽采用**基础** IM 10% 的一半，即 ±5%；持续触发条件是 **10 分钟** | 不使用盘后 IM 重新推算当前价格带；读取同一快照上下界与 session |
| 25 万美元本金、两周约 1.092 亿美元成交、现金 +2,096、积分 EV 164,408 | 属报告情景；其中 maker 层现金约 −3,752，另两层 +5,848 才得到净额，且没有完整成交/库存/资金费账本 | 三赛道独立，无交叉补贴；积分现金记 0；25 万本金和 2 万 pilot 都不是用户授权 |
| 升 Tier 5/6 可显著提高利润 | 份额按全市场滚动 30 天 maker 分母，按钱包计算，资格每日评估且有生效延迟 | 主情景保守使用公开 Tier 0；不预支返佣 |
| 市场中性交易可稳定积累高价值积分 | Annex C 明确覆盖主要为积分进行的对冲、市场中性及循环活动；积分没有保证兑现权 | 正常交易与风险管理可以研究；奖励资格未知，不设计虚假交易、循环刷量或规避审查 |
| 当前系统缺 Arcus/HL/Binance 与外部锚 | 与当前源码不符：已有 Arcus 签名/REST/WS、HL HIP-3 与 Binance 适配器、外部锚及通用 hedge machine | 复用现有能力；真正缺口是 Arcus–xyz 的持久化执行、回执/成交对账与恢复监督 |
| 扩充私有 SHM 放链上成交/AI 告警 | 当前私有事件是 Go/Rust 128 字节 V2 ABI，来源权威性与发布顺序有明确约束 | 公共规则走独立版本化快照；本轮不改私有 ABI |
| 60% 被动 / 40% IOC 对冲是最优 | 未给足量排队、残腿、拒单与平仓证据 | 不设置固定比例；实际确认成交驱动对冲，所有未决订单保留敞口 |
| ETF、其它 TGE、新链机会应同步扩张 | 缺少本轮独立核验且分散两周窗口 | 不进入本轮关键路径 |

官方来源：[Anthropic IPO](https://www.anthropic.com/news/anthropic-prices-initial-public-offering)、[Arcus ANT 上线](https://arcus.xyz/blog/anthropic-ipo-is-live-with-ant-usd-perpetuals-now-trading-on-arcus)、[Arcus 费率](https://docs.arcus.xyz/concepts/perpetuals/fees.md)、[费率 API](https://api.arcus.xyz/v1/markets/feeSchedules)、[市场 API](https://api.arcus.xyz/v1/markets)、[RWA 规则](https://docs.arcus.xyz/concepts/perpetuals/real-world-assets.md)、[HL 费用](https://hyperliquid.gitbook.io/hyperliquid-docs/trading/fees)、[Arcus 条款 Annex C](https://arcus.xyz/legal/protocol-terms)。这些是时间快照，不保证未来不变。

## 2. 盈利机制必须按完整周期计算

首选研究候选：Arcus ANT 真实挂单成交后，按确认数量在 xyz:ANT 对冲；退出两腿完整结清。只有可执行价差覆盖开平仓费用、滑点、残腿风险、资金费和抵押品差价后才生成候选。一个交易所出现报价并不证明另一边能成交；持仓账本必须拆开新开、减仓、穿零、最终库存，不能用短期 markout 代替现金。

### 2.1 四腿费用下限

假设两边开仓本金均为 N、平仓名义金额也为 N，只为了统一费用分母；实际账本按每笔成交价格计费。HL 使用上述 0.9 bp taker 情景。以下不含滑点、买卖价差、资金费、借贷、汇兑、停市与强平。

| Arcus 开仓 / 平仓 | HL 开平两腿 | 总费用 / N | 每百万 N 费用 | 每百万四腿总成交额费用 |
| --- | --- | ---: | ---: | ---: |
| Maker / Maker | Taker / Taker | 1.80 bp | $180 | $45 |
| Maker / Taker | Taker / Taker | 2.92 bp | $292 | $73 |
| Taker / Taker | Taker / Taker | 4.04 bp | $404 | $101 |

上表是零价差收益条件下的费用下限，不是预测净亏或可盈利阈值。持有期净简单年化 **12% 是假设**，按 365 天折算，8 小时收益仅 1.096 bp；覆盖三种费用分别需约 13.14、21.32、29.49 小时。因此不能把“APR >12%、持有 8 小时”当作固定入场门槛。

Carry 独立立账：按每个资金费支付时间、方向、数量及 oracle 名义金额计算，不将小时率直接与 8 小时率相减。Arcus 盘后资金费根据 SOFR 每小时重算，日计数 360；这里 12% 的示例使用 365，不能混用。[Arcus Funding](https://docs.arcus.xyz/concepts/perpetuals/funding.md)

Arcus metadata 的 `quoteAsset=USD` 是价格计价字段，不证明抵押品与 HL 等价；HL 的 `collateralToken=0` 也必须通过 token 元数据解析，不能仅凭索引默认为可无损互换。合约跟踪对象、公司行动、指数与停市行为尚需逐项一致性核验。

### 2.2 Maker 份额不能固定分母

若当前总 maker 成交 D、本人已有 V、目标份额 s，追加 x 后份额是 `(V+x)/(D+x)`，阈值为 `x > (sD−V)/(1−s)`，不是 `sD−V`。以报告 D=$9.075B、V=0 为静态示例，1.5% 需超过约 $138.198M，4% 需超过 $378.125M；这还没有计入其它用户增量、旧成交滚出和资格生效延迟。该计算不构成成交量目标。

### 2.3 与前两天研究的关系

Claude 短板报告和本地完整库存周期研究已经表明：1 秒待撤成交改善不等于最终现金改善，缩小保护幅度、持仓超时和多轮参数扫描未提供稳定收益。本轮沿用**完整周期现金、真实开平仓分类、独立预算、事前冻结、全试验登记**的结论，不把 BTC/ETH 历史失败或正值直接外推为 ANT 表现。新 IPO、FAST session 和两所制度差异是新假设，需 ANT 自身证据。

## 3. 已实施的第一批代码

1. [Arcus 定时撤单](../src/exchanges/arcus/scheduled_cancel.rs)：`ArcusSigner::schedule_cancel` 使用官方 legacy Scheme 2；签名时间纳秒、撤单时间微秒，严格检查 5 秒至 5 分钟窗口，不通过取整放行边界外请求。账户级与市场级是独立 scope；无 time 为该 scope 的 disarm。
2. [REST](../src/exchanges/arcus/rest.rs)：`ArcusTrading::schedule_cancel` 只将 HTTP 200 且地址、子账户、市场、deadline、status 全部匹配的回执作为成功；到达时已过期、202、损坏/错位响应、503 都不算保护成功。400/401/403/429 分类为拒绝，无自动重试、无自动 disarm、无输出私有响应体。
3. [离线规则工具](../scripts/audit_ant_campaign.py)：只允许三种固定公共数据请求。快照独立目录、正常 TLS、请求开始和接收时间分别记载、哈希/大小/身份检查。两处 Arcus 费率交叉比对，检查 session/价格带、HL growth mode/scale；四腿费用按明确分母计算，不将积分当现金。
4. [合成回归](../scripts/test_audit_ant_campaign.py)及 Rust 假交易所测试覆盖签名、单位、scope、过期、拒绝和未知结果；元数据错误、费率变更、折扣取消、重复身份、时钟与内容篡改均不能取得一致性结果。

这是接口与研究准入工具，**尚未将 DMS 接入 live supervisor**。没有生成或运行新的实盘 profile。现有 `place_limit` 不会因新增接口而自动获得 session/band/DMS 保护；后续必须在监督器风险边界接线。DMS HTTP 成功也不是订单终结、仓位归零或释放 reserve 的证据。

DMS 每个子账户每日成功触发次数有限（官方当前 10 次），另有活跃 switch 数与速率限制。研究初始候选为 60 秒 deadline、10 秒刷新，但须把时钟、RTT、最长失联容忍和未对冲预算冻结后再接线；本轮没有后台刷新进程。[DMS 官方接口](https://docs.arcus.xyz/api-reference/exchange/schedule-cancel-all-dead-mans-switch.md)

### 可复验的只读命令

```sh
# 使用全新的目录；不会覆盖已有快照，也不会加载账户凭据。
python3 scripts/audit_ant_campaign.py capture .agent-state/research/ant_public_example_001
python3 scripts/audit_ant_campaign.py audit .agent-state/research/ant_public_example_001
python3 -m unittest discover -s scripts -p test_audit_ant_campaign.py -v
```

第二条默认当前 UTC，规则超过 60 秒或跨源抓取跨度超过 10 秒即拒绝一致性。两者是本地研究政策，不是交易所新鲜度保证；没有源事件时间仍不能证明可成交。历史复核可显式 `--as-of <UTC>`，输出带 `historical_audit=true`，不可作为当前实盘准入。通过时始终保留 `live_ready=false` 与未完成证据清单。

## 4. 48 小时计划与阶段验收

T0 为本轮 11 月 20 日认领。时段是工程目标，不保证在期限内满足实盘门槛；未过门槛留在离线/只读。同事接续应在最新 handoff 认领具体文件，避免同时修改 supervisor。

| 顺序 / 目标时段 | 交付与负责人 | 验收证据 / 止步条件 |
| --- | --- | --- |
| P0-A，T0–6h | 本轮 Codex：报告逐项校核、动态费用审计、DMS 接口与故障测试 | 官方来源、哈希、合成拒绝路径、四腿现金分母；已实现，实际门禁结果见交接 |
| P0-B，6–18h | 下一位接手者优先认领：Arcus–xyz 持久化 intent/order/fill 映射与恢复审计；接入 DMS 和新开风险 gate | 先写 intent 再发送；同一 client ID 幂等；ACK 丢失、重复/迟到成交、部分平仓和穿零、pending cancel、DMS 续租失败、断流、重启均保留最坏敞口；未对账禁止新开 |
| P0-C，12–30h | 与 B 不冲突的只读研究：核 ANT 两合约/抵押品、账户实际费率与条款，消费 aleph 已签收 ANT 连续数据 | 不向 Tokyo 加采集任务；登记确切 ANT 覆盖，缺什么写什么。没有 ANT 证据不使用 BTC 窗替代；不把 metadata 快照称 10h 验证 |
| P0-D，24–42h | 冻结一个 Arcus maker + HL 保护 IOC 的完整周期候选；carry 作为独立配置/账本 | 先冻结候选、费用、深度比例、库存/损失预算、数据与代码 hash、10h 边界、预热/排空和停机条件；验证实际退出可行性、费率回升/盘后扩带/残腿压力；缺口保留，不事后择窗 |
| P0-E，36–48h | 形成可审查的最小 canary 包与操作步骤 | 明确实际网络/账户、总资本/每腿/未对冲/日损失上限、合约与费率凭据、熔断/撤单/恢复路径、shadow 和故障测试结果；最后由用户确定资金与实盘动作授权。未准备好不以活动倒计时强行上线 |

三条研究线独立：① ANT 现金套利为主线；② carry 只有支付期和退出双腿证据完整后晋级；③ 积分/成交量仅作为资格不确定的附加观察，收益账本计零。若另保留纯量化方向，必须有自己的方向风险预算，不能替 maker 补亏。

## 5. 证据、限制与交接入口

- 本轮 `.agent-state/research/ant_p0_20261120/` 保存原报告阅读凭据、官方原件与 SHA-256、公共快照、审计结果、测试日志和逐阶段交接；都不入 Git。
- 第一版 Python urllib 因本机 CA 信任失败，失败凭据保留；系统 curl 使用正常 TLS 成功。初次抓取程序把**请求开始时间误命名为 received_at**，其原件/收据保留为研究来源，不能用于精确时效准入。新版 `public_snapshot_v2` 分开 requested/received，实际离线审计使用新版。
- 两份市场 API 快照只能证明该时刻的公开规则一致；不能证明账户获优惠、积分获认可、当前 order book 可成交或两周可持续盈利。
- 现有 10h 冻结回放由同事继续；本轮没有宣称新 ANT 10h 盈利结果，也没有启动交易、取消、部署或 Tokyo 工作。
- 最新执行状态、实际测试数量、Mac/aleph 同步哈希和下一项待办以 `AGENT_HANDOFF.md` 与该目录 `HANDOFF.md` 为准。

## 6. 本轮实际验证（2026-11-20）

- Mac：`make fmt-check`、`make check`、`make test`、`make build` 全通过。Rust 库 699 passed / 6 ignored，另两组 5、8 passed；Go 检查/测试/构建通过；研究 Python 252 passed。首次默认沙箱不允许假交易所绑定本机端口，保留失败日志后在允许本地端口的测试环境通过。
- aleph：以基线提交 `0ab1252356a535d443965f2b36ca42d699debbb6` 建独立源码目录，应用本轮白名单源码；`cargo test --offline --locked --lib exchanges::arcus` 11 passed，新增 Python 7 passed。没有把这一边界复验描述成 Linux 全量门禁。
- 同一 ANT 公共快照在 Mac/Linux 的审计输出逐字节一致，公开规则一致性通过、`live_ready=false`。文档相对链接和 `git diff --check` 通过。
- 传输最初的宽泛归档被自动审批拒绝；随后使用明确的 18 项文件白名单（419,840 bytes）成功同步，排除了私有 Gemini 分享链接/阅读凭据。官方完整网页与私有阅读凭据留在 Mac；aleph 持有本轮源码、结论文档、公共市场快照与聚合回执。
- 原有双小时协作任务已更新为 ANT P0，保留频率、协作边界与离线默认；没有新建重复调度。

## 7. 后续工程：DMS 单调时钟续租门禁（2026-11-20）

`CancelLease` 与 `ArcusTrading::renew_cancel_lease` 已实现一个账户/市场 scope 的独立保护状态和单次 REST 续租。请求开始即暂停新开；回执延迟不能延长本地保护；进入安全余量或超时即拒绝新开。失败、错位/迟到 ACK、异步 future 被取消、旧 deadline 可能触发均要求重新对账。同一 guard 不会因随后成功 ACK 自动恢复，须由持久化 supervisor 完成两腿对账后新建 guard。没有自动刷新后台任务，也没有接入 live 下单路径。

本轮复核主动收紧了初稿的“失败后重新续租即可恢复”：那无法排除旧请求在远端仍未决或旧 DMS 已触发。初稿测试/构建结果单独留档，最终版本另行验证；这不是交易参数变体。控制时钟与本机假交易所覆盖回执迟到、旧 deadline 保护缺口、掉线、账户错配、时钟跳变、重复请求和 future 取消。实际验证与跨主机哈希见 `.agent-state/research/ant_cancel_lease_20261120/HANDOFF.md`。

同事已独立认领 ANT 长窗采集及公开价差诊断，本组不重复。下一主项仍是 Arcus–xyz 持久化 intent/order/fill 与恢复监督器；新保护模块不会代替它，也不是 ANT 经济盈利结果。
