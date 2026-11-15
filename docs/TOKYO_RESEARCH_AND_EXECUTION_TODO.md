# 东京低延迟阶段研究与工程待办清单（对手盘反收割与三赛道行动路线）

日期：2026-11-15。状态：活跃待办（Active Backlog & Prioritized Queue）。

关联文档：
[工程总纲](../AGENTS.md) ·
[主平台研究设计](PRIORITY_VENUE_RESEARCH_DESIGN_2026_11_14.md) ·
[独立策略契约](INDEPENDENT_STRATEGY_RESEARCH.md) ·
[50ms 阶段实证总结](FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md) ·
[量化数据湖规范](QUANT_DATA_LAKEHOUSE.md) ·
[实盘运行手册](LIVE_TRADING.md)

本文档沉淀机构级链上/跨所对手盘逆向调研（Wintermute `0x51c7...2a7f` / `Bot.sol` / `SurfNoTurf` / `Rizzolver` 实证研究，参见 Wu et al. AFT 2025、`arXiv:2506.04940`、`arXiv:2509.22143`、`arXiv:2603.20175`）对 AlephTX 可迁移的高 ROI 工程与策略启发，并锁定切换至 **`oasis-tokyo`（`<10 ms`）+ 20TB 数据湖 + `aleph` 本地工作站** 后的下一步行动排序。

---

## 1. 反收割前置红线：明确排除的三项反模式（Non-Goals）

在吸收外部顶级做市商经验时，必须严格区分“百亿美元单一资产负债表 AMM/RFQ 巨头”与“AlephTX 独立三赛道 CLOB 永续架构”的边界，以下三类做法明确禁止：

1. **严禁跨赛道内部轧差与交叉补贴（Non-Subsidy Contract）**
   - 禁止将 `03_incentive_turnover`（激励刷量）的被动成交与 `01_quant_profit`（量化盈利）或 `02_hedge_arbitrage`（对冲套利）的主动吃单合并至同一净 Delta 账本轧差。
   - 内部轧差仅允许在**同一独立赛道内部**闭环（如 `02_hedge_arbitrage` 的双腿 `Maker-First + Protected IOC`）；不同 Perp DEX 维持**独立 EVM 地址物理隔离**，规避 Arcus Annex C 与 Lighter Robinhood 的关联账户审查风险。
2. **严禁将 EVM AMM 原子回滚逻辑套用于 Perp CLOB**
   - 四大核心平台（Arcus、Entropy `io:*`、TradeXYZ `xyz:*`、Lighter Robinhood Chain）均为中心化/应用链限价订单簿（CLOB），不存在链上 AMM 依赖 `minOut` 失败 `revert` 的零滑点试探。核心约束是定序器延迟分层（如 Lighter Standard `200–300 ms` vs. Premium `0 ms`）与撤单风控规则。
3. **严禁对领先腿微幅噪音做高频全量撤单重报（FIFO Queue Preservation）**
   - 前期 32 组 `2x2` 消融已证实（见 [FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md](FIFTY_MS_STRATEGY_RESEARCH_2026_11_14.md#143-binance-reference-2x2-factorial-diagnostic-attribution-645-h-continuous-interior-32-runs)）：对 `1.0 bps` 级 Binance 微噪做全量撤单，10 小时触发超 `20,000` 次撤单，摧毁 FIFO 队列优先级并饿死 `74%` 良性均值回归成交，且在 Arcus 上会直接触发 **Rulebook Annex C §8(c)** 高撤单率动态减速带或费率重分类。

---

## 2. 下一步行动排序（Prioritized Execution Queue）

按时间因果与预期投资回报率（ROI）严格排序：

1. **P0 · 东京首轮完整 `10h` 数据封口回收与 `<10 ms` 准入标定**
   - 首轮试采残片（`22m31s`，因 KVM VDSO 双读抢占提前退出）已单独封存至 `10_bronze_raw/quarantine_partials/` 仅作 `<10 ms` 时延诊断基线，绝不计入 10h 验证。待集成 `consistentNow()` 成对 VDSO 跨度护栏的 `oasis-tokyo` 重拉连续 `10h15m` 全市场录制包（`tokyo-selection-w1-r2-*`，含 `-arcus-trades`）完整封口后，经 Google Drive `10_bronze_raw/tokyo_selection_dev/` 同步至 `aleph` Tier 1（`E:` 盘）。
   - 首步运行 [audit_tokyo_selection_readiness.py](../scripts/audit_tokyo_selection_readiness.py) 与 `quant_lakehouse.py normalize`，在完整 10h 连续窗口上标定东京 `3.6–8.8 ms` 物理网络下 Arcus、Entropy、TradeXYZ、Lighter RH/Core 相对 Binance USD-M 的真实到达时延、连续性与跨市场领先滞后（Lead-Lag）曲线。
2. **P1 · 第一批 `aleph` 连续 10h 回测主线 A：`02_hedge_arbitrage` RWA 跨场所相对价值**
   - 利用新补齐的 `arcus_trades` 真实逐笔流推进被动队列，优先验证 **Arcus（RWA `0.00 bps` Maker / `2.25 bps` Taker）↔ TradeXYZ（`xyz:NVDA`, `xyz:SNDK`）/ Entropy（`io:NVDA`, `io:SNDK`）** 的结构性基差均值回归（前期观测中位基差 `-3.09 ~ -7.34 bps`）与多日资金费差套利。
   - 显式接入 `America/New_York` 美股正常交易时段、盘前盘后与周末休市日历门控，隔离跨时段预言机跳空风险。
3. **P2 · 第一批 `aleph` 连续 10h 回测主线 B：`01_quant_profit` & `03_incentive_turnover` 对手盘成本边界护盾消融**
   - 按 [PRIORITY_VENUE_RESEARCH_DESIGN_2026_11_14.md](PRIORITY_VENUE_RESEARCH_DESIGN_2026_11_14.md) 在东京连续 10h 窗口执行 Binance 辅助信号 A/B/C/D 四组受控消融（先冻结选参集，再进独立 `tokyo_holdout_vault`）。
   - 用顶级做市商 `-4.2 bps` 隐含边际成本边界校准风险过滤阈值：对比“无过滤本地基线”与“**`4.5–8.0 bps` 单边越界撤单护盾**（[pricing.rs](../src/strategy/inventory_neutral_mm/pricing.rs) `per_side_bound_cancel`）”，同时评估净现金损益、尾部回撤与合格成交产出速度。

---

## 3. 吸收自机构对手盘调研的专项待办清单（Backlog Checklist）

### 3.1 定价模型与微观结构防御（Strategy & Pricing）

- [ ] **TODO-STRAT-01 · 植入“`-4.2 bps` 机构边际成本”点差下界与非对称单边护盾**
  - **背景**：实证显示头部多所做市商凭借顶格负 Maker 返佣与现货库存池，其真实链下对冲价较同期 Binance 公开盘口优约 `4.2 bps`，且已重金驻场 Robinhood Chain、HyperEVM 与 Lighter 生态。
  - **落地要求**：在无负费率返佣优势的账户上，严禁挂设 `<8 bps` 窄点差与 `0 ms` 机构正面零和博弈；跨所领先信号触发阈值须跨过 `4.2 bps` 成本边界（与半点差同阶），且在单向突破时仅撤销逆向风险侧挂单，保留顺向侧 FIFO 队列位置。
- [ ] **TODO-STRAT-02 · RWA 美股永续跨场所异构规则矩阵与时段状态机**
  - **背景**：机构重仓配置多发行体系美股代币（Robinhood、Base、Ondo）以捕获传统盘口与链上池割裂基差；永续 DEX 之间同样存在预言机来源与时段资金费倍率（如时段内 `0.5` vs. 场外 `0.125`）的结构性错配。
  - **落地要求**：在 `30_external_evidence/` 维护 Arcus / Entropy / TradeXYZ / Lighter RH 同标的合约单位、结算币（USDC vs. USDG）、休市价格带、资金费公式与公司行动审计表，在回放与实盘准入前强制通过合约等价性校验。

### 3.2 成本治理与配额调度（"Kairos Principle" Cost Discipline）

- [ ] **TODO-COST-01 · 严守“固定沉没成本转按需可变成本”准则**
  - **背景**：头部机构在 Arbitrum Timeboost 竞拍中放弃每分钟预付固定租金盲拍，全面转向按单笔确定性信号付费（Kairos 模式），并对外部通道死守固定净留存利润率底线（约 `18%`），拒绝为低波动垃圾时间买单。
  - **落地要求**：
    1. 在东京 `<10 ms` 连续 10h 终验集跑出稳健净现金正期望前，不采购高固定成本的 VIP 质押或付费加速席位，优先深耕 **Lighter Standard `0/0 bps`、Arcus RWA `0 bps` Maker、Entropy Early-Bird 自返费** 等零固定租金层级；
    2. 将 API 撤单频率与 IOC 发包预算与实时短期波动率/跨所基差挂钩——低波期维持长寿命挂单（`TTL 30–200s`），把撤单/吃单配额集中释放于真实错价或风控减仓窗口。

### 3.3 实盘安全边界、状态锁与开源反侦察（Execution Security & OPSEC）

- [ ] **TODO-SEC-01 · 多 DEX 账户“热端去资产化”与最小权限签名隔离**
  - **背景**：`Bot.sol` 通过“冷多签锁底仓 + 去资产化热操作员轮换 + 独立报价签名公钥秒级吊销”彻底隔离热私钥泄露风险。
  - **落地要求**：各 Perp DEX 独立 EVM 地址统一采用仅具备下单/撤单权限、**零提币与零跨账户划转权限**的原生 Agent Wallet / Session Signer / API Key；在 Lighter 上结合已实现的独立 `LIGHTER_CANCEL_*` / `LIGHTER_TAKER_*` 签名通道消除单 Nonce 队列阻塞并最小化子进程密钥暴露面。
- [ ] **TODO-SEC-02 · 对冲腿严苛 Limit 保护价“状态锁”**
  - **背景**：机构大额套利单通过亚基点（Sub-bps）`minOut` 充当盘口状态锁，一旦进块前深度被同行扫穿立即拦截逆向高滑点成交。
  - **落地要求**：在 [live_hedge.rs](../src/live_hedge.rs) 与 `maker_first` 对冲路径中，所有 IOC 对冲单必须基于最新有效盘口深度附带显式滑点上限（Protected Limit IOC），严禁无保护市价单裸奔；未成交残余敞口按既定持久化恢复日志（Journal）与降风险梯度闭环。
- [ ] **TODO-SEC-03 · 开源发布前的策略与配置反侦察审计**
  - **背景**：`Bot.sol` 因使用明文可枚举 `EnumerableSet` 暴露 `getAllApprovedTokens()`，导致新币种与新池布局被外部交易者提前逆向追踪抢跑。
  - **落地要求**：仓库未来面向全球开源时，公开代码与 `config.example.toml` 仅展示通用工程机制与合成示例参数；实盘运营的长尾标的清单、精确库存偏置参数、时间窗控制与账户拓扑严格留在 Git 之外的私有 Profile 中。

### 3.4 外部链上情报边界（Off-Hot-Path Intelligence）

- [ ] **TODO-DATA-01 · 数据湖 `30_external_evidence/` 低频链上白名单立项雷达（严禁入热路径）**
  - **背景**：头部做市金库的公开白名单差集与新链 `Nonce=0` 同址部署可反映新兴链（如 Robinhood Chain、HyperEVM）早期流动性进驻动向。
  - **落地要求**：仅允许作为 `aleph` 数据湖 `30_external_evidence/` 下的独立低频（日/小时级）Python 离线采集脚本，输出新市场研究立项参考；**严禁接入 Go `feeder` 或写入 `/dev/shm/aleph-events`（128 字节 V2 私有订单恢复通道）**，保持交易热路径与共享内存 ABI 的绝对纯净。
