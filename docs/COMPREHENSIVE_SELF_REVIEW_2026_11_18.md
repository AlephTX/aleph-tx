# AlephTX 走向大规模生产应用：全面自我审视、深度代码审计与演进路线图报告

日期：2026-11-18（Asia/Shanghai）。审计基线：HEAD `08c66b5` 及当前本地工作树。  
关联文档：[工程总纲](../AGENTS.md) · [系统总览](../README.md) · [深度优化研究总案](DEEP_OPTIMIZATION_RESEARCH_2026_11_17.md) · [Vertox 做市指南详案](VERTOX_MARKET_MAKING_RESEARCH_2026_11_17.md) · [东京研发待办](TOKYO_RESEARCH_AND_EXECUTION_TODO.md) · [实盘运行手册](LIVE_TRADING.md)

> **合规与授权声明**：本报告严格遵守 [AGENTS.md](../AGENTS.md) 安全红线（未读取或泄露任何密钥、私有配置、个人账号/网盘标识或原始私有交易记录）。全量静态源码审查覆盖 Rust `66,603` 行、Go `8,401` 行、Python `19,028` 行（`66` 个脚本）、TypeScript/React `3,606` 行（合计 **`97,638` 行源码**）。当前三条研究赛道维持 **`HOLD`**，任何实盘下单、撤单、部署或重启均须单独动作级授权。

---

## 一、 全局工作进展审视与实证真值盘点

AlephTX 已经演进为一套具备机构级工程骨架的**三平面（Three-Plane）永续合约低延迟执行与因果量化湖仓引擎**。但站在“走向大规模资金与多标的无人值守应用”的门槛上审视：**工程基础设施成熟度显著领先于策略净现金盈利验证，且实盘执行与离线回放之间存在结构性断层。**

### 1.1 三大运行平面现状评估

| 平面 | 核心组件与当前进展 | 成熟度评级 | 走向大规模生产的核心短板 |
| --- | --- | :---: | --- |
| **Plane 01 · 低延迟受控执行平面**<br>(`oasis-tokyo` · Go 1.25 + Rust 1.95) | 完成东京 `<10 ms` 边缘节点部署；`12 × 2048` 槽位 `64B` BBO（[matrix.go](../feeder/shm/matrix.go) / [shm_reader.rs](../src/shm_reader.rs)）与 `256B` Top-5 深度（[shm_depth_reader.rs](../src/shm_depth_reader.rs)）完成对齐原子读写改造（`OPT-03` 阶段性成果）；具备三通道签名隔离（Maker/Cancel/Taker）、双场所对冲 WAL 日志（[live_hedge.rs](../src/live_hedge.rs)）与会话级 SQLite 监控台。 | **B+** | 1. 仅支持 Lighter 单所做市及 `Lighter Core + Hyperliquid` 对冲，缺乏跨场所通用实盘抽象；<br>2. `sendTxBatch` 限流/拒单在下一周期会触发 `200 bps` 吃单强平全部被动库存（见 `EXEC-C1`）；<br>3. `aleph-events-v2` 与 `aleph-account-stats` 仍存在 ARM64 写重排与非原子数据竞争隐患（`ABI-C1`）；<br>4. 缺少独立进程级死人开关 `riskd`（`OPT-09`）。 |
| **Plane 02 · 7×24h 滚动采集与飞轮**<br>(`crypto` / `rwa` / `bulk` 三路隔离) | 物理隔离 systemd 服务持续滚动采集 `10h15m`（`36,900s`）窗口（`W01..W05` 已封存核验，`W06` 封窗同步，`W07` 已启动）；具备 VDSO `<25 µs` 时钟守卫、SHA-256 清单封存、`SCHED_IDLE` 上传与 `<16 GiB` 自动回收机制（`W03` 已实测安全回收）。 | **A-** | 1. 连续无缺口覆盖尚未稳定达标（`W02`、`W05` 均出现源端 WS 断连）；<br>2. VDSO 时钟守卫采用相对启动锚点累计 `±1 ms` 硬门限（[timing.go](../feeder/cmd/perp-capture/timing.go)），正常 Linux NTP `0.03 ppm` 慢速校时即可在 9 小时后终止整窗录制（`CAP-C1`）；<br>3. 定时器与滚动器并发 `ship-public` 缺少根锁 `flock`，存在云盘重名对象永久卡死风险（`OPS-C1`）。 |
| **Plane 03 · 因果回放与归因工作站**<br>(`aleph` · 3-Tier NVMe + DuckDB + GPU) | 完成三层存储隔离、`research_windows.py` 预注册防泄漏门禁（`OPT-02` 已闭环，`W04` 与 `W07–W16` 均在决策前冻结）、四分母成交额字典（`VMM-01`）与多期限因果归因（[live_attribution.py](../scripts/live_attribution.py)）。 | **A-** | 1. **实盘与回放双轨分裂**：`02_hedge_arbitrage` 与 `03_incentive_turnover` 完全运行在独立的 `6,984` 行纸面引擎 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 中，无法零改写晋级实盘；<br>2. Python 单线程解压与 `O(N)` 字典盘口扫描导致 Stage 1/2 耗时 40+ 分钟；<br>3. Bronze→Silver 各脚本场所/市场命名漂移（`OPS-H2`）。 |

### 1.2 实证研究真值盘点（为什么当前坚持 `HOLD`）

基于台湾原频率复现、东京 `W01–W05`（51 臂 v2 冻结 OOS + 26 臂 v1 对照 + 170 组历史对照）及 2023–2025 年 252 个月档 Binance 外部资金费筛选，当前三条赛道的实证真值如下：

1. **量化盈利赛道（`01_quant_profit`）**：
   - 台湾原 `100ms` 两窗在 `0.25 bps` 模型开销、`25%` 深度参与下，`$500` 单笔 BTC 录得 `+$0.218 / +$1.853`，但 ETH 录得 `-$5.583 / -$11.154`；吃单成本再增 `1 bps` 后四窗全亏。
   - 东京 `W02` Arcus ETH 叠加 Binance 门控后微盈 `+$0.445`（无门控 `+$0.333`），但成交额萎缩 `76%`；BTC 仍净亏 `-$0.29`。结论：现有启发式微价与静态风控尚未跨过扣全成本后的稳健正期望门槛。
2. **对冲套利赛道（`02_hedge_arbitrage`）**：
   - 东京 `W05` 51 臂中 `18` 臂完成现金核对、`23` 臂因断流留空（`gap_withheld`）、`10` 臂未结清；唯一微正变体仅 `+$0.0648`（双边 `$599.62`），且存在 1 分钟预热例外与 HIP-3（Entropy / TradeXYZ）逐资产部署者费率未逐一核实的问题（[venues.rs](../src/venues.rs#L84-L88) 已诚实撤回通用费率行）。
   - 三年（2023–2025）外部资金费筛选表明：2025 年正向资金费套利的全包成本上限仅 **`0.134 bps`**，难以覆盖普通双腿 Taker 进出成本。
3. **激励周转赛道（`03_incentive_turnover`）**：
   - 目标成本线 `$100 / $50M` 等价于 **`$2 / $1M = 0.02 bps`**（目标平台口径）。
   - `W05` 14 个现金核对方案 **0 个达到 `$2 / $1M`**；`W01–W03` 上美股常规盘 `SPY` 经退出消融后成本仍高达 **`$162.90 / $1M`**（反向 `RH Maker -> Arcus IOC` 为 `$121.14 / $1M` 双边、`$242.18 / $1M` Maker），根本症结在于 **Arcus 被迫吃单强退占比高达 `31%–50%`**（零补贴纯费用理论容忍上限仅为 **`0.889%`**）；跨月零奖励限制库存版单腿成本亦从 `$366.15` 降至 `$201.61 / $1M`，仍距 `$2` 目标差两个数量级。同时已核实 Lighter Core Standard 对 RH 活动合格量为 `$0`，LPP 账户条件亦未满足。

---

## 二、 三大场景策略优缺点、业界最佳实践对标与改进方案

### 2.1 场景一：量化盈利做市（`01_quant_profit`）

| 现有策略组件 | 核心优点 | 核心缺陷与实盘隐患 | 业界最佳实践对标与改进方案 |
| --- | --- | --- | --- |
| **跨所中位数公允价 + 慢速基差校准**<br>([market_state.rs](../src/strategy/inventory_neutral_mm/market_state.rs#L230-L285)) | 过滤单所异常插针，通过 `BasisTracker` 吸收永续对锚定所的稳态资金费溢价偏移。 | 1. 外部源短暂陈旧回退至本地盘口（[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L1971-L2011)）时，`pricing_mid` 发生阶跃跳变，污染 `MicrostructureTracker` 的波动率与动量估计；<br>2. 本地 `calculate_vw_micro_price` 属于启发式 L1–L5 加权，非经样本外校准的条件均值。 | **校准收缩微价（Stoikov Calibrated Microprice · `VMM-04`）**：固定预测期 $h$（匹配场所真实挂撤延迟，如 `250–500ms`），在 `(spread_ticks, L1_imbalance)` 网格上离线拟合收缩残差表 $\Delta m(I, s)$，热路径 $O(1)$ 查表；外部与本地源维护各自独立的波动率/动量基线，严禁跨源差分。 |
| **A–S 混合库存偏斜 + 自成交冲击**<br>([inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L368-L415), [#L2259-L2278](../src/strategy/inventory_neutral_mm.rs#L2259-L2278)) | 已时间化归一化 `TimeScaledVolatility`，并引入 `SelfFillAlpha` 捕捉平仓归零瞬间的逆向压力。 | 1. **量纲不一致（`VMM-07`）**：`gamma * sigma^2 * q * T` 中 `q` 为币数而非归一化美元敞口，与 `inventory_skew_bps * tanh(...)` 双重叠加；<br>2. **`SelfFillAlpha` 缺陷**：按成交笔数而非订单/名义额脉冲（一笔大单拆成多笔微碎成交即顶满 `3x` 上限），且未过滤主动 IOC 减仓成交。 | **无量纲归一化定价方程 + 被动 Maker 订单级冲击核**：<br>令归一化库存 $x = q / Q_{\max} \in [-1, 1]$，统一采用价格单位叠加：<br>`r = F * (1 + alpha_ext + alpha_self_maker - gamma_bps * sigma_sec^2 * tau_eff * x - psi(x))`；<br>`SelfFillAlpha` 仅对被动 Maker 成交按 `fill_size / base_order_size` 注入脉冲。 |
| **单侧越界撤单与滞回**<br>(`per_side_bound_cancel` · [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L2529-L2547)) | 顺向侧保留 FIFO 队列位置、仅撤危险侧，避免高频全量撤单饿死 `74%` 均值回归成交并触发 Arcus Annex C 惩罚。 | 1. `self.hedge.is_some()` 时硬编码禁用；<br>2. 任一越界挂单尚无 `order_index` 时直接退化为全撤双边；<br>3. 未显式计入 FIFO 队列前方深度 $Y_t$ 的保留期权价值。 | **队列期权价值离散动作选择（Queue-Value EV · `VMM-03/05`）**：<br>结合 Rigtorp 队列估计跟踪每笔挂单前方量 $Y_t$；比较“保留排队 vs. 撤单重挂”的条件期望收益，在 Hyperliquid 共识 Cancel-Priority 场所可适度收窄半点差，在 Lighter Standard `300ms` 撤单路径上前置计入 `300ms + RTT` 逆向储备。 |
| **事件防御机制** | 已具备基于动量、跨所压力与 `MarkoutTracker` 的被动点差扩宽（最高 `1.5x`）。 | 滞后盘口指标难以抵御突发停牌、上币或安全事件冲击，缺乏独立公共新闻与确定性防御状态机。 | **独立公共事件信号链 + 五态防御状态机（`OPT-06/07`）**：建立与私有 `aleph-events-v2` 物理隔离的公共信号通道，驱动 `NORMAL -> CAUTION -> SIDE_DEFENSE -> HALT_RECONCILE -> REDUCE_ONLY`，危险侧秒级禁挂撤单。 |

### 2.2 场景二：跨场所对冲套利（`02_hedge_arbitrage`）

| 策略子方向 | 核心优点 | 核心缺陷与亏损根源 | 改进方案 |
| --- | --- | --- | --- |
| **主流币资金费套利** | `maker_first` 支持被动建仓降费并带结算边界守卫。 | 1. 2025 年实证资金费正差全包成本天花板仅 `0.134 bps`；<br>2. 当前跨越整点强制 `flat_at_funding` 平仓（[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs#L898-L911) `maker_first_funding_lifecycle_not_supported`），无法持有跨多期收息。 | 打通基于 `source_time` 持仓水位的跨期资金费真实结算入账；引入最短回本周期门控，仅在资金费权重锁定度高的后半周期且双腿可执行基差有利时被动开仓。 |
| **RWA 美股永续相对价值** | Arcus RWA `0 bps` Maker 具备费率优势，跨 DEX 存在显著结构性基差（`-3 ~ -7 bps`）。 | 1. **异构预言机伪套利**：休市后不同部署者外部冻结与内部定价公式不同，价差未必回归；<br>2. **HIP-3 成本与稳定币隔离风险**：Entropy/TradeXYZ 逐资产 `deployerFeeScale` 不同，且 `USDC` vs `USDG`（Lighter RH）不可瞬时互转救仓；<br>3. `MakerFirstConfig` 中 `enforce_equity_session_gate` 默认 `false`（见 `STRAT-H1`）。 | 强制开启时段与参考门控，将常规套利严格限定于有同源实时外部锚的连续交易时段；收盘前强制进入 `drain`；按 HIP-3 官方公式逐资产核准费率并独立计提单腿 `3σ` 跳空保证金缓冲。 |
| **恐慌行情 CEX–DEX 错价套利** | 急跌期间 DEX 与 Binance 经常出现 `15–40 bps` 可观测错价（`ARB-STRESS-02` 已验证）。 | 断腿风险（Legging Risk）极高；实盘 [live_hedge.rs](../src/live_hedge.rs#L943-L965) 仅取 Hyperliquid L1 第一档深度，遇到 `<$10` 碎单即拒绝发单卡死（`EXEC-H6`）。 | 对接多档深度汇总保护价（Volume-Weighted Sweep Depth within `slippage_bps`）实现 `DEX Maker -> CEX Protected IOC`，并预扣四笔费用与残余断腿救援成本。 |

### 2.3 场景三：激励周转做市（`03_incentive_turnover`）

- **核心数学死结**：以 `$100 / $50M`（`0.02 bps`）为预算目标，若平仓/对冲腿 Taker 费率为 `2.25 bps`（Arcus RWA L0），**只要有 `> 0.889%` 的成交量被迫吃单平仓，仅手续费一项就已击穿 `0.02 bps`**。当前消融中 `SPY` 等标的高达 `31%–50%` 的 Arcus 强退占比是造成 `$121–$242 / $1M` 成本的直接主因。
- **破局方向**：
  1. **双零费率或负返佣配对优先**：优先验证双边 `0 bps` Taker 路由（如 Lighter Core Standard 参与对冲），禁止在 `2.25–4.50 bps` Taker 场所高频强退；
  2. **极高耐心 Maker-Maker 平仓退出（`TURN-COST-04`）**：对 `net == 0`、单腿保证金充裕且未临近资金费/收盘窗口的已对冲仓位，废除短时间强退，改由中位数基差锚定并长期排队被动平仓，将吃单强退率压向 `< 2%`；
  3. **账户实发资格前置门禁（`OPT-01`）**：单腿低波均值回归模式（Mode B）必须先由官方条款与极小单实盘账单证实 `eligible_fraction > 0` 后方可扩大规模，并叠加“Kairos 按需原则”在高波动/高毒性窗口主动空仓休眠。

---

## 三、 全量代码深度审计缺陷清单（按严重程度分级）

### 3.1 Rust 受控执行、风控与对冲内核

| ID | 级别 | 定位 | 根因剖析与线上风险 | 修复契约 |
| --- | :---: | --- | --- | --- |
| **`EXEC-C1`** | **CRITICAL** | [trading.rs](../src/exchanges/lighter/trading.rs#L425-L431), [#L1567-L1595](../src/exchanges/lighter/trading.rs#L1567-L1595)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L755-L761), [#L1484-L1542](../src/strategy/inventory_neutral_mm.rs#L1484-L1542)<br>[recovery.rs](../src/exchanges/lighter/trading/recovery.rs#L93-L139) | **单次 `sendTxBatch` 拒单/限流导致全仓 `200 bps` 吃单强平或进程崩溃**：`submit_prepared` 遇到任何错误（含常规 `23000 RateLimited`、`21301 InsufficientMargin`、`21104 InvalidNonce`）均不解除 `SubmissionGuard`，`Drop` 触发 `require_reconciliation()`。下一 `quote_cycle`（50ms 后）看到 `needs_reconciliation()`：<br>• `quant` 模式走 `clean_exchange_state` → `recover_flat_without_feed()`，**发送 `200 bps` 滑点 IOC 将账户合法被动库存强平归零**；<br>• `inventory_hedge` 模式触发 `ensure!(self.hedge.is_none())` **致命退出并双腿吃单平仓**。 | 分离**会话内无损对账（`reconcile_non_flat`）**与**停机清仓（`recover_flat`）**：盘中对账仅撤单并核对 `terminal_fill_time` 与 `position_evidence`，调用 `confirm_recovered_position` 重锚非零仓位，严禁平掉合法库存；仅在优雅停机时清仓。 |
| **`EXEC-C2`** | **CRITICAL** | [ffi_native.rs](../src/exchanges/lighter/ffi_native.rs#L70-L75), [#L284-L289](../src/exchanges/lighter/ffi_native.rs#L284-L289)<br>[ffi.rs](../src/exchanges/lighter/ffi.rs#L48-L88)<br>[trading.rs](../src/exchanges/lighter/trading.rs#L1933-L1945) | **原生 CGo `SignCreateOrder` `u32 -> c_int` 价格溢出为负数**：`encode_order` 允许 `u32::MAX`（`4.29e9`），但 CGo 签名为 `c_int`（`i32::MAX = 2.14e9`），`price as c_int` 对高价/高精度合约（如 5 位小数的 BTC `> $21,474.83`）直接截断回绕为负整数！Mock 签名直接序列化 `u32` 掩盖了该溢出。 | 在 `encode_order` 与 `sign_create_order`（原生与 Mock 一致）强制校验 `i32::try_from(price)` 与 `i32::try_from(trigger_price)`。 |
| **`EXEC-C3`** | **CRITICAL** | [order_tracker.rs](../src/order_tracker.rs#L796-L835)<br>[exchange.rs](../src/exchange.rs#L88-L96)<br>[trading.rs](../src/exchanges/lighter/trading.rs#L675-L681)<br>[execution.rs](../src/strategy/inventory_neutral_mm/execution.rs#L165) | **REST 对账晋升产生“无法撤销的僵尸挂单”**：`reconcile_with_exchange` 将漏掉 WS `OrderCreated` 的 `PendingCreate` 晋升为 `Open` 时**未绑定 `order.order_index`（保持 `None`）**，且忽略了 `oo.filled` 成交差值。后续所有撤单流程过滤 `order_index.is_some()`，导致该单永远无法改单或撤单。 | `OrderInfo` 新增 `order_index: Option<i64>`，对账时同步补齐 `order_index`；若 `\|oo.filled - order.filled_size\| > 1e-9` 立即触发 `require_reconciliation()`。 |
| **`EXEC-C4`** | **CRITICAL** | [daily_loss.rs](../src/daily_loss.rs#L116-L118)<br>[lighter_account_stats.go](../feeder/exchanges/lighter_account_stats.go#L191-L193)<br>[alerts.py](../deploy/oasis/alerts.py#L66) | **账户爆仓归零或穿仓（`equity <= 0`）时日损熔断全链路失效（Fail-Open）**：`DailyLossGuard::check` 对 `equity <= 0.0` 直接 `return Ok(0.0)` 不落盘 `breached = true`；Go REST 刷新在 `equity <= 0` 时报错保留旧值；`alerts.py` 在 `equity <= 0` 时清除 `CRITICAL` 告警并发送 `[resolved]` Webhook。 | 严格区分“缺失快照”与“真实 `equity <= 0`”；后者立即持久化 `breached = true`、触发硬停机并在监控台保留 CRITICAL 告警。 |
| **`EXEC-H1`** | **HIGH** | [order_tracker.rs](../src/order_tracker.rs#L626-L656), [#L993-L1001](../src/order_tracker.rs#L993-L1001) | **`mark_failed` 回滚后收到迟到 `OrderCreated` 被静默忽略**：本地释放预留转为 `Rejected` 后若网关推来 `OrderCreated`，`handle_order_created` 命中 `completed_orders` 直接 `Ok(())` 丢弃，形成无风险预留的实盘挂单。 | 仅对 `Filled \| Canceled` 忽略；命中 `Rejected` 时立即恢复挂单与原子预留并触发 `require_reconciliation()`。 |
| **`EXEC-H2`** | **HIGH** | [trading.rs](../src/exchanges/lighter/trading.rs#L1203-L1208), [#L1454-L1456](../src/exchanges/lighter/trading.rs#L1454-L1456) | **`reduce_only` 绕过风控但未校验方向与数量**：违反 `AGENTS.md` 硬规则。 | 在 `LighterTrading` 下单边界校验 `reduce_only` 方向严格反向且不超确认持仓（对账恢复态除外）。 |
| **`EXEC-H3`** | **HIGH** | [recovery.rs](../src/exchanges/lighter/trading/recovery.rs#L100-L116), [#L148-L150](../src/exchanges/lighter/trading/recovery.rs#L148-L150) | **`recover_flat` 第 0 轮遇 `21104` 即中止且 `canceled` 集合屏蔽重发**：批量撤单缺 `InvalidNonce` 重试；发单前即标记 `canceled` 导致异步失败挂单后续 9 轮不再重撤；手工单 `coi == 0` 永久失败。 | 恢复循环捕获瞬态/Nonce 错误并重同步；未消失挂单跨 2 轮重发撤单；妥善处理 `coi == 0` 订单。 |
| **`EXEC-H4`** | **HIGH** | [trading.rs](../src/exchanges/lighter/trading.rs#L1012-L1027)<br>[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L2803-L2839) | **混合改单在 `maker_lane` 限流时饿死紧急撤单**：`[Cancel, Place]` 混合批次仅走 `maker_lane`，被 `23000` 拒绝后未借空闲 `cancel_lane` 撤旧单。 | 混合批次失败时立即将其中撤单通过 `cancel_all_orders()`（走 `cancel_lane`）独立发出；危险越界撤单优先分道。 |
| **`EXEC-H5`** | **HIGH** | [live_hedge.rs](../src/live_hedge.rs#L263-L286), [#L614](../src/live_hedge.rs#L614), [#L783-L797](../src/live_hedge.rs#L783-L797) | **跨时钟域比较**：用本地 `sent_at_ms` 比较 Hyperliquid 远端 `state.time`，本地慢时把发单前旧快照误判为拒单确认，本地快时持续卡死至 `MAX_BLOCKED`。 | 记录发单前远端快照 `baseline_venue_time_ms` 或绑定 `orderStatus` 远端时间戳，禁比本地墙钟与远端时间。 |
| **`EXEC-H6`** | **HIGH** | [live_hedge.rs](../src/live_hedge.rs#L56), [#L468](../src/live_hedge.rs#L468), [#L943-L965](../src/live_hedge.rs#L943-L965) | **L1 碎单饥饿与 `deadband_base == 0` 死锁**：`plan_ioc` 只看 L1 第一档量，且 `deadband_base == 0` 允许 `<$10` 残仓卡死 20s 崩溃。 | 汇总 Hyperliquid `limit_price` 内多档深度；校验强制 `deadband_base` 覆盖 `$10` 最小名义额。 |
| **`EXEC-H7`** | **HIGH** | [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L926-L933)<br>[live_hedge.rs](../src/live_hedge.rs#L669-L674) | **对冲模式下按单腿权益触发日损与 95% 止损**：标的正常波动导致盈亏在两腿间迁移即触发全天误停机与双腿吃单平仓。 | 对冲模式按双场所合并净值考核日损，单腿分别用保证金健康度与可用余额兜底。 |
| **`EXEC-M1`** | **MEDIUM** | [order_tracker.rs](../src/order_tracker.rs#L603), [#L700-L715](../src/order_tracker.rs#L700-L715)<br>[rest.rs](../src/exchanges/arcus/rest.rs#L84-L92), [#L133-L151](../src/exchanges/arcus/rest.rs#L133-L151) | `POS_SCALE` 截断与 `.round()` 混用导致原子敞口漂移；`ArcusTrading` 无发单序列化锁且将重复 ID HTTP 400 误判为确定拒绝。 | 统一使用 `.round() as i64`；为 `ArcusTrading` 增加提交锁并按错误语义区分拒绝/不确定。 |

### 3.2 Go 采集守护、共享内存 ABI 与 `perp-capture`

| ID | 级别 | 定位 | 根因剖析与线上风险 | 修复契约 |
| --- | :---: | --- | --- | --- |
| **`ABI-C1`** | **CRITICAL** | [events.go](../feeder/shm/events.go#L167-L179)<br>[shm_event_reader.rs](../src/shm_event_reader.rs#L270-L303)<br>[account_stats.go](../feeder/shm/account_stats.go#L104-L186)<br>[account_stats_reader.rs](../src/account_stats_reader.rs#L59-L88) | **`aleph-events-v2` 与 `aleph-account-stats` 存在 ARM64 写重排与 Rust 数据竞争 UB**：`events.go` 用 `atomic.StoreUint64`（ARM64 `STLR` 不阻挡后续普通写向上重排越过奇数门限），且 Rust 侧用 `read_volatile` 读取双缓存行结构（LLVM 数据竞争 UB，`fence(Acquire)` 不约束 `read_volatile`）；`account_stats` 亦同。 | 开锁使用 `atomic.AddUint64(..., 1)`；两端全部改为对齐 `uint64`/`AtomicU64` 逐字原子读写后加 `fence(Acquire)`。 |
| **`BOOK-C1`** | **CRITICAL** | [lighter_private.go](../feeder/exchanges/lighter_private.go#L369-L438), [#L521-L546](../feeder/exchanges/lighter_private.go#L521-L546) | **`LighterPrivate` 无限内存泄漏与已完成订单 `orderSizes` 复活**：`orderDetails` 仅在撤单时清理、成交永不清理；且 `trade` 先于 `order(status="filled")` 到达时，`"filled"` 分支会把已删 `orderSizes` 重新写回并永久泄漏。 | 引入有界已完成集合与 `orderDetails` 环形回收；已完结且 `remaining == 0` 订单禁止回填 `orderSizes`。 |
| **`CAP-C1`** | **CRITICAL** | [timing.go](../feeder/cmd/perp-capture/timing.go#L83-L88)<br>[timing.rs](../src/capture/timing.rs#L126-L140) | **累计 `±1 ms` 相对启动偏差硬门限误杀 10h15m 录制**：在 `36,900s` 内，`1 ms` 仅允许 `0.027 ppm` 频偏，正常 Linux NTP 慢速校时即可在第 8–9 小时杀死录制进程并把整窗打入隔离区。 | *(注：不中途触碰运行中的 `W06–W16` 原合同)* 后续版本区分“瞬时阶跃跳变（`>1ms`）”与“有界线性 NTP 频偏（`<=50 ppm`）”，如实记入审计。 |
| **`ABI-H1`** | **HIGH** | [events.go](../feeder/shm/events.go#L139-L155)<br>[lighter_private.go](../feeder/exchanges/lighter_private.go#L224-L226), [#L284-L291](../feeder/exchanges/lighter_private.go#L284-L291) | **独立 Feeder 重启静默吞掉断连期成交 & 重放初始快照历史成交**：重开沿用 `writeIdx` 不发代次变更也不推 `PrivateStreamLost`；首次 `subscribed/account_market` 携带的历史 `trades` 被盖当前时间当作实时成交推入环中。 | 环头 `[16..24]` 写入递增 `producer_epoch`，重开非空环时推 `PrivateStreamLost`；初始订阅快照仅播种 `seenTradeIDs` 不发增量成交。 |
| **`BOOK-H1`** | **HIGH** | [lighter_book.go](../feeder/exchanges/lighter_book.go#L72-L80)<br>[lighter.go](../feeder/exchanges/lighter.go#L95-L132), [#L162](../feeder/exchanges/lighter.go#L162) | `offset <= b.offset` 在 `begin_nonce != b.nonce` 前提前返回掩盖盘口断档；`lighter.go` 缺时间戳时用 `time.Now()` 冒充源时间导致后续真实帧因单调门限被丢弃；公共 WS 缺 `subscriptionGuard`。 | 仅无 `nonce` 时用 `offset` 去重；删除 `time.Now()` 伪造源时间；接入 `subscriptionGuard` 与空闲超时重连。 |
| **`BOOK-H2`** | **HIGH** | [arcus.go](../feeder/exchanges/arcus.go#L136-L192)<br>[hyperliquid.go](../feeder/exchanges/hyperliquid.go#L181-L206)<br>[binance.go](../feeder/exchanges/binance.go#L198-L221)<br>[lighter_account_stats.go](../feeder/exchanges/lighter_account_stats.go#L58-L100), [#L211-L256](../feeder/exchanges/lighter_account_stats.go#L211-L256) | `strconv.ParseFloat` 允许 `NaN`/`Inf` 绕过 Arcus 比较；HL/Binance/Arcus 交叉或异常盘口不调用 `InvalidateBBO` 留下过期报价；Arcus 单品种空簿撕毁共享 WS；`LighterAccountStats` 断连不清零 `UpdatedAt` 且日志明文打印美元余额。 | 严格校验有限正数；异常即失效槽位（不断共享连接）；`user_stats` 断连调用 `InvalidateEquity()` 并脱敏日志。 |
| **`CAP-H1`** | **HIGH** | [main.go](../feeder/cmd/perp-capture/main.go#L534-L545), [#L640](../feeder/cmd/perp-capture/main.go#L640), [#L670-L692](../feeder/cmd/perp-capture/main.go#L670-L692) | `lighter_trades`/`lighter_robinhood_trades`/`extended_trades` 未将订阅确认计入存活，夜间零成交误杀 RWA 10h 整窗；轮转同步 `fsync` 阻塞单消费者易撑爆 `16,384` 队列。 | 订阅确认帧计入 `counts` 存活；旧段刷盘封口异步化。 |

### 3.3 数据湖、归因、运维飞轮与监控台

| ID | 级别 | 定位 | 根因剖析与线上风险 | 修复契约 |
| --- | :---: | --- | --- | --- |
| **`OPS-C1`** | **CRITICAL** | [flywheel_sync.py](../scripts/flywheel_sync.py#L125-L162), [#L205-L309](../scripts/flywheel_sync.py#L205-L309)<br>[perp_rolling_runner.py](../scripts/perp_rolling_runner.py#L286-L305) | **并发 `ship-public` 无 `flock` 锁导致云盘重名死锁与 TOCTOU 崩溃**：4–5GB 上传耗时 `> 15m` 定时器周期，滚动器与定时器并发 `rclone copyto` 在 Google Drive 生成两个同名文件，触发 `len(items) != 1` 永久报错，阻塞回执与磁盘回收。 | 在 `ship_public_bronze` 获取全局根级 `fcntl.flock` 并安全处理已回收文件的 `FileNotFoundError`。 |
| **`OPS-C2`** | **CRITICAL** | [flywheel_sync.py](../scripts/flywheel_sync.py#L381-L388)<br>[monitor.py](../deploy/oasis/monitor.py#L256-L263) | **SQLite 在线备份活锁**：`backup(..., pages=256, sleep=0.005)` 在 `SCHED_IDLE` 下每逢 `monitor.py` 每 2 秒写入 WAL 即重头从第 1 页重启，大库永远无法备份完成。 | 改用 `VACUUM INTO ?` 单遍 MVCC 快照导出并做完整性检查。 |
| **`STRAT-H1`** | **HIGH** | [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs#L135-L136), [#L751](../src/strategy/relative_value/paper/maker_first.rs#L751) | `MakerFirstConfig.enforce_equity_session_gate` 默认 `false` 且 `preflight()` 未在股票类资产下显式告警/门控。 | 在 `MakerFirstPreflight` 显式输出 `equity_session_gate_enforced` 并在未开启时记入 `required_evidence` 阻断实盘准入。 |
| **`OPS-H1`** | **HIGH** | [live_attribution.py](../scripts/live_attribution.py#L397-L400), [#L549-L558](../scripts/live_attribution.py#L549-L558) | 事后归因将远端 `exchange_ts_ns` 与东京采集 `available_at_ms` 直接做 `<=` 关联，引入约 `80ms` 跨洋前视偏差（Lookahead）。 | 统一与本地接收时钟关联并补充 DuckDB 同毫秒 `sequence` 确定性排序。 |
| **`OPS-H2`** | **HIGH** | [quant_lakehouse.py](../scripts/quant_lakehouse.py#L704), [#L1084](../scripts/quant_lakehouse.py#L1084), [#L1368-L1376](../scripts/quant_lakehouse.py#L1368-L1376)<br>[archive_public_capture.py](../scripts/archive_public_capture.py#L20-L32) | Bronze→Silver 跨工具 `venue` 命名分歧、Lighter `funding_and_ctx.market` 写 symbol 而非 ID 导致关联失败、`hyperliquid_trades` 漏字段静默丢笔、`normalize` 默认跳过高精度时钟检查。 | 抽取统一 `classify_stream` 注册表并在存在 `capture_timing` 时自动强制精确时钟校验。 |
| **`OPS-H3`** | **HIGH** | [perp_rolling_runner.py](../scripts/perp_rolling_runner.py#L59-L97), [#L279-L285](../scripts/perp_rolling_runner.py#L279-L285)<br>[aleph-tx-live.service](../deploy/oasis/aleph-tx-live.service#L1-L24) | 1. 三路服务共用 `quarantine/` 并发回填清单触发 `FileExistsError`，且隔离区永不清理；<br>2. `perp_rolling_runner.py` 封窗前未校验实际单调时长；<br>3. `aleph-tx-live.service` 缺 `RuntimeDirectory=aleph-tx-live` 与私有 `ALEPH_SHM_DIR` 约束。 | `backfill` 按流过滤并捕获 `FileExistsError`；`WINDOW_RE` 识别清单后缀；补齐时长校验与 systemd 目录权限加固。 |
| **`MON-H1`** | **HIGH** | [alerts.py](../deploy/oasis/alerts.py#L74-L81)<br>[telemetry_store.py](../deploy/oasis/telemetry_store.py#L405-L411), [#L598-L601](../deploy/oasis/telemetry_store.py#L598-L601)<br>[Tables.tsx](../dashboard/src/components/Tables.tsx#L614-L618)<br>[Kpis.tsx](../dashboard/src/components/Kpis.tsx#L292-L340) | `hedge_imbalance` 混加存量与流量；`session_id=None` 混加全历史不同赛道与账户盈亏；`/api/sessions` 漏返字段导致矩阵“含浮盈总计”全显 `—`；手续费未知仍标绿色“零损耗”。 | 修复对冲差公式、默认绑定最新单会话、补齐 `/api/sessions` 契约并严格呈现 `unknown` 质量徽章。 |

---

## 四、 性能优化与面向大规模扩展的架构重构空间

1. **弥合“实盘 vs. 回放”双套引擎鸿沟（Write Once, Run Everywhere）**：
   - 将策略决策核心提炼为纯确定性状态机 `StrategyCore::on_event(LogicalTime, Event) -> SmallVec<[ExecutionIntent; 4]>`，统一 [exchange.rs](../src/exchange.rs) 为支持 `Decimal` / 整数 `Lots`/`Ticks` 与三态结果 `SubmissionOutcome` 的 `VenueExecution` trait，合并四套重复的 `f64`/`Decimal` 风控与取整门控，并拆分 `maker_first.rs`（`6,984` 行）、`inventory_neutral_mm.rs`（`3,334` 行）与 `lighter/trading.rs`（`2,939` 行）。
2. **交易热路径微秒级唤醒与去阻塞**：
   - 将 [lighter_inventory_mm.rs](../src/bin/lighter_inventory_mm.rs#L108-L110) 的 `10ms` 轮询睡眠与 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs#L3022-L3040) 的 `5ms` SHM 轮询替换为事件通知/自适应退避（压至 `< 50 µs`）；把 [daily_loss.rs](../src/daily_loss.rs#L143-L160) 与 [live_hedge.rs](../src/live_hedge.rs#L879-L884) 的同步 `fsync` 移出 Tokio 工作线程；将 [telemetry_sink.rs](../src/telemetry_sink.rs#L288-L302) 改为排空后批量 `flush` 并为 `hedge_fill` 建不可丢弃通道。
3. **湖仓 20×–50× 加速与脚本治理**：
   - Go `perp-capture` 写入同时流式计算 SHA-256 与时钟统计，消除云端 3 遍 `gzip` 读取；将 [quant_lakehouse.py](../scripts/quant_lakehouse.py) 的每帧 `O(N)` Python 盘口扫描迁移为 Rust `cross_venue lakehouse-normalize`（`BTreeMap` + Arrow/Parquet Zstd 直写），并将 `66` 个分散 Python 脚本收敛为 `scripts/aleph_research/` 结构化包。

---

## 五、 融合现有 TODO 的下一步优先级路线图（P0 → P3）

1. **P0 · 安全止血与真值底座（立即执行，阻塞一切实盘授权）**
   - **P0-1**：修复 `EXEC-C1..C4` 与 `EXEC-H1..H4`（无损盘中对账、CGo `i32` 价格检验、僵尸挂单 `order_index` 补绑、`equity <= 0` 熔断告警、`reduce_only` 边界检查、`recover_flat` 重试与 `cancel_lane` 兜底）+ 推进 `EXEC-H5..H7`。
   - **P0-2**：修复 `ABI-C1/H1` 与 `BOOK-C1/H1..H2`（完成 `events-v2` 与 `account-stats` 原子化收口 `OPT-03`，根治 `LighterPrivate` 内存泄漏、历史交易重放与盘口断档掩盖）。
   - **P0-3**：修复 `OPS-C1/C2/H3`（飞轮 `flock` 锁、SQLite `VACUUM INTO`、隔离区防碰撞、systemd SHM 权限），并在安全切换窗口推进 `CAP-C1/H1`。
   - **P0-4**：修复 `STRAT-H1`、`OPS-H1/H2`、`MON-H1` 并完成 `OPT-01` 全 runner 能力表统一与 HIP-3 逐资产核实。
2. **P1 · 连续采集、风控闭环与制度门控**
   - 完成预注册 `W07–W16` 前瞻评分与断流韧性验证；打通 `RWA-02/03/06` 八类边界用例；落地 `OPT-05` 配额账本与 `OPT-09` 独立 `riskd` 守护原型；接入只读公共新闻通道与 `OPT-06/07` 五态防御机。
3. **P2 · 三赛道净期望单因子破局攻关**
   - **量化 `01`**：修 `SelfFillAlpha` 与无量纲 A–S 库存项，完成校准微价 + Rigtorp 队列期权保留消融（`VMM-02..07`）；
   - **套利 `02`**：支持 `maker_first` 跨周期真实 `source_time` 资金费结算，完成恐慌行情 CEX–DEX 多档对冲三臂与前瞻验证（`ARB-STRESS-03..05`）；
   - **激励 `03`**：账户实发资格取证 + 双零/高耐心被动平仓攻关（`TURN-COST-01/04/05`），将强退吃单率压至 `< 2%` 后重测成本—容量曲线。
4. **P3 · 统一核重构与极小单实盘标定（仅在连续 `K >= 10` 个 10h Holdout 证真后启动）**
   - 统一 `StrategyCore` / `VenueExecution` 实盘回放同核；落地 `<50 µs` 唤醒与 Rust 湖仓归一化（`OPT-12`）；补齐 `-bulk-trades`（`TODO-DATA-02`）；在单独人工授权下以极小单实测 `p50/p90/p99` 延迟与资格账单（`TODO-EXEC-01`），收敛后再分阶段审批规模放量。
