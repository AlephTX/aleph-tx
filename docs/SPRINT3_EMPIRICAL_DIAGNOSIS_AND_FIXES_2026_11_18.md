# AlephTX Sprint 3 实证归因、代码级缺陷闭环与策略/微结构修复报告（2026-11-18）

> **边界与合规声明**：本文基于本地仓库源码审计、Sprint 2/3 单元测试以及 `aleph` 离线因果回放汇总编写，严格隔离私有身份、密钥与远端运行时文件。三赛道实盘状态维持 **HOLD**。

---

## 1. 双向代码审阅回执：`scripts/quant_lakehouse.py`（W06 冲突隔离投影）

对 Codex 提交的 `--withhold-arcus-trade-market`（`scripts/quant_lakehouse.py` 与 `scripts/test_quant_lakehouse.py`）已完成只读源码逐行核验：

- **已验证的工程亮点**：
  - 默认 `normalize_capture(...)` 严格模式零放宽（同一 `tradeId` 微秒时间戳冲突仍触发 `ValueError("conflicting duplicate Arcus trade")`）。
  - 显式传入 `--withhold-arcus-trade-market` 时，仅过滤 `raw_source == "arcus_trades"` 且 `event in ("subscribed", "message")` 的指定市场消息（`scripts/quant_lakehouse.py:764-771`）；同时对信封错配（`market != envelope_market` 或 `channel != "trades"`）立即抛出 `ValueError("ambiguous Arcus market in withheld trade stream")`。
  - 每条原始捕获行的 `sequence`、`available_at_ms >= previous_at` 与 `ClockAudit` 收件单调性仍全量校检；所有非隔离市场保持严格校验；`silver_manifest.json` 明确标注 `artifact_kind = "silver_partial_market_projection"` 并计入 `withheld_arcus_trade_messages`（`scripts/quant_lakehouse.py:1575-1577`）。
- **建议 Codex 在后续阶段补齐的两项防呆约束**：
  1. **(`LKH-1`) DuckDB `build-db` 显式隔离门禁**：`build_duckdb_lakehouse`（`scripts/quant_lakehouse.py:1805`）当前未检查 `manifest["artifact_kind"]`，而 `generate_duckdb_sql` 对 `(collector_region, venue, utc_date, campaign_id)` 使用 `OVERWRITE_OR_IGNORE true`（`1645/1690/1730/1762`）。W06 本轮通过独立目录与独立命名人工避免了污染；建议为后续 `W07–W16` 自动化流水线在 `build_duckdb_lakehouse` 中增加 `--allow-partial-projection` 开关，未显式声明时拒绝将 `silver_partial_market_projection` 写入主 `parquet_root`。
  2. **(`LKH-2`) `v_stream_health` 与逐笔覆盖率联动提示**：`v_stream_health`（`scripts/quant_lakehouse.py:1779-1791`）按 `(venue, domain, market)` 汇总 `v_feed_lifecycle`，而被隔离逐笔的 RWA 市场（如 `SPY-USD` / `QQQ-USD`）仍保有 Arcus BBO 与共享 WS 连接事件。下游 `public_trades` 研究消费脚本必须联合读取 `silver_manifest.json["withheld_arcus_trade_messages"]` 或检查 `raw_source = 'arcus_trades' AND event = 'subscribed'`，不可仅凭 `v_stream_health` 断定该市场具备完整逐笔带。

---

## 2. Sprint 2/3 核心实证归因与源码修复清单

结合 `aleph` 最新六大离线实验（`venue_delay_v1`、`quality_windows_v1`、`alt_asset_transfer_v1`、`fair_value_arrival_label_v2`、`sprint2_mechanisms_v1`、`sprint2_reversal_bounds_v3`、`pair_protection_ablation_v1`、`basis_hours_v2` 与 `order_sizes_summary.json`）逐项穿透源码，定位并修复了以下五类根因：

| 编号 | 实证现象 / 失败证据 | 源码根因定位 | Sprint 3 代码修复（全部默认关闭，零破坏基线） |
| --- | --- | --- | --- |
| **S3-1** | `sprint2_reversal_bounds_v2` 出现 `local_signal.rs:228` 运行时 panic；修复参数后 `v3` 六窗受理 **1,006 笔零成交** | (a) `LocalReversalFilter::validate()` 未校验 `0.2 * floor <= recovery_bps` 及省略 `floor` 时的隐式默认 `0.5 / 0.10`，触发 `f64::clamp` `min > max` panic；(b) `InventoryNeutralMM` 基础挂单距买/卖一深达 `4–8 bps`（`min_profit_bps = 8–16`），而 `entry_hold_ms <= 1000ms` 的反转窗口在恐慌砸盘见底后已停止下探，深挂 1 秒几乎不可能成交 | [local_signal.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal.rs)：新增 `min_recovery_bps_floor`、`effective_vol_floors()` 与非 panic `ordered_clamp()`，并在启动前 `validate()` 拦截上下界倒置；新增 `entry_touch_offset_bps` + `LocalParticipation::apply_with_book()`，在买一/卖一附近及 `MakerPriceBounds(0 profit)` + `PostOnly` 不跨盘口约束内贴盘入场；[local_signal/tests.rs](../src/strategy/inventory_neutral_mm/replay/recorded/local_signal/tests.rs) 补充完整合成回归 |
| **S3-2** | 做市基线 `-7.12` -> 加 `local_toxicity` `-8.23` -> 再加 `entry_requote_threshold_bps = 4bp` 后恶化至 **-9.28 美元** | `components.rs:386-416` 在 `calm_mode && level_idx < 2` 下把阈值悄然放大 **2.0 倍**（`4bp -> 8bp`），且对价差偏离做**对称容忍**。当公允价向我方挂单有利方向漂移时保留队列虽正确，但在不利方向（公允价逼近或穿越我方挂单）时反而把陈旧高危挂单留在盘口长达 `CALM_REQUOTE_MIN_LIFETIME = 8s` 被精准收割 | [config.rs](../src/config.rs) + [components.rs](../src/strategy/inventory_neutral_mm/components.rs) + [execution.rs](../src/strategy/inventory_neutral_mm/execution.rs) + [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)：新增 `QuoteHysteresisConfig::{asymmetric_entry_ratchet, adverse_retreat_threshold_bps}`（**单边棘轮队列保护**）：关闭 `2x` 静默膨胀；仅对有利/窄幅收紧方向沿用 `entry_requote_threshold_bps` 保队；一旦新目标价向**不利回撤方向**移动超过 `adverse_retreat_threshold_bps`，绕过 `8s` 冷却在当前周期**0 秒强撤并重挂** |
| **S3-3** | 开启 `local_ofi` 与 `cancel_speed_bump_ms` 温和扩宽后成交额降 21%（`$40.1k -> $31.7k`），净现金反而从 `-7.12` 降至 **-8.23 美元** | **尾部选择悖论（Tail-Selection Paradox）**：在已设 `min_profit_bps = 16 bps` 的做市策略中，温和均值回归噪声本就不会打到 8bp 半价差；再额外加宽 2–4bp 过滤掉的是尚可盈利的中幅震荡单，却对 `30–80 bps` 单边急跌/拉升毫无拦截能力，导致留下的成交几乎全是被毒流量打穿的单子 | [config.rs](../src/config.rs) + [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)：新增 `LocalToxicityConfig::{ofi_withdraw_threshold, vol_pause_bps}`。当归一化本地 OFI 超限或瞬时短周期波动 `vol_bps` 超阈值时，仅将**增仓侧**目标手数清零（`size = 0` 硬撤单，而非仅加宽 2bp），同时严格保留**减仓侧**报价 |
| **S3-4** | `alt_asset_transfer_v1` 中 `84` 个 XRP/DOGE 量化配置启动前失败；做市 `$500` 基础报价中 **97.68% (`13,699 / 14,025`)** 挂单低于 `$495`（区间 `$19.83–$999.89`） | (a) `src/config.rs:964-986` 原先无条件检查 `step_size <= max_position`，即使用户已明确配置 USD 名义额上限（如 `max_position_notional_usd = 150.0`），仍被继承自 ETH 模板的 `max_position = 0.2 < step_size = 1.0` 拦截；(b) `$495` 截断与离散区间成因（见第 3 节） | [config.rs](../src/config.rs)：当 `max_position_notional_usd > 0`（及 `inventory_urgency_notional_usd > 0`）时改为校验 USD 名义额边界关系 `base_order_notional_usd <= max_position_notional_usd` 与 `inventory_urgency_notional_usd <= max_position_notional_usd`；新增可选 `round_base_notional_nearest` + [components.rs](../src/strategy/inventory_neutral_mm/components.rs) `round_nearest_to_step()`（保留硬上限 `.min(max_position)` 与杠杆保证金裁剪） |

---

## 3. 代码级精算拆解：为何 `$500` 做市 97.68% 受理挂单低于 `$495`

针对 Codex 在 `order_sizes_summary.json` 中核对到的关键数字（`$500` 基础报价、14,025 笔受理、`97.68%` 低于 `$495`、区间 `$19.83–$999.89`、318 次被动成交均额 `$126.16`），对应当前仓位与风险缩放管道（[inventory_neutral_mm.rs:1079-1165](../src/strategy/inventory_neutral_mm.rs#L1079-L1165)、[components.rs:51-67](../src/strategy/inventory_neutral_mm/components.rs#L51-L67)、[components.rs:291-293](../src/strategy/inventory_neutral_mm/components.rs#L291-L293)、[components.rs:498-614](../src/strategy/inventory_neutral_mm/components.rs#L498-L614) 与 [inventory_neutral_mm.rs:1346-1405](../src/strategy/inventory_neutral_mm.rs#L1346-L1405)）可完全精确解释如下：

1. **向下取整与买单贴水叠加**：`build_risk_snapshot` 使用 `round_down_to_step(500.0 / mid, step_size)`。以 BTC 约 `$90,000`、`step_size = 0.0001` 为例，`500 / 90000 = 0.005555... -> 0.0055 BTC`（中间价名义额 `$495.00`，挂在低于中间价的买盘时名义额直接跌至 `< $495`）；而向上一个步长即跳至 `0.0056 BTC`（约 `$504`）。单个步长约 `$9` 跨度几乎恰好对应 `[$495, $505)` 仅 10 美元宽度，任何一点向下取整或减量都会落出该窄桶。
2. **多档网格权重衰减与总头寸除数**：当 `grid_levels > 1` 时，`grid_multiplier = Σ 0.7^i`（2 档 `1.70`，3 档 `2.19`）。第 1、2 档按 `0.7x / 0.49x` 衰减，且 `apply_risk_limits` 用 `bid_top_headroom = (max_position - worst_case_long) / grid_multiplier` 限制首档上限；仅在持有反向库存享受 `1.35x / 1.5x` `flattening_boost`（受 `flattening_cap_mult = 2.0` 封顶到 `$999.89`）时才会突破 `$505`（对应 `323` 笔 `>= $505`）。
3. **Deadband 内 30% 软倾斜与部分成交后尾单**：即使 `position` 未超过 deadband，`calculate_asymmetric_sizes_for_config` 也会对同侧挂单施加最高 `-30%` 缩量、对减仓侧 `+30%` 增量（因此除极少数特定仓位外极少落在 `[$495, $505)` 之间，仅占 `3 / 14,025`）；一旦部分成交或接近风控上限，剩余 `bid_top_headroom` 会下探至交易所最小可挂额缓冲 `min_quotable_size`（`round_up_to_step(12.5 / price.max(tick_size), step_size)`，对应最低受理单做市 `$19.83`）。
4. **方向量化（`$100` 基础单，`$14.75–$200.12`）同理**：`flattening_cap_mult = 2.0` 决定最高 `$200.12`，`min_quotable_size` 与剩余风控额度裁剪决定下限 `$14.75`。

补充配对提示：若离线研究在含 `quote_hysteresis` 的配置下开启 `LocalReversalFilter.entry_touch_offset_bps`，建议将 `cancel_min_profit_bps` 设为 `0.0`（与 `apply_with_book` 的零利润保本上界一致）并开启 `per_side_bound_cancel = true`，防止次周期 `breaching_any` 立即撤掉前一周期刚刚贴盘挂出的反转单。

Sprint 3 新增的 `round_base_notional_nearest = true` 已通过单元测试验证在 BTC `$90k / step_size=0.0001` 下可精确就近取整至 `0.0056 BTC`（`$504`），同时严格不突破 `max_position` 与保证金约束。

---

## 4. 三赛道战略推演（基于最新 `venue_delay_v1` 与 `basis_hours_v2`）

1. **`01_quant_profit`（零奖励纯盈利做市与方向量化）**：
   - `venue_delay_v1` 已在数据上证伪两项错误幻想：(a) 在 Lighter Free Standard 升级为 0-stake Premium（`0.4/2.8 bp`, 0ms cancel）几乎无收益提升（`-$16.93 -> -$16.20`），因为手续费吞噬了撤单降延；(b) 即使在 **1ms 传输 + 0ms 交易所延迟 + 0 费率**的理想物理环境下，连续双边对称挂单在 8 窗仍亏损 **`-$6.66`**。
   - 结论：**连续无条件双边做市已达机制天花板**。后续只有三条清晰出路：(1) 利用 Sprint 3 `asymmetric_entry_ratchet` + `ofi_withdraw_threshold` / `vol_pause_bps` 做严格状态机过滤的非连续做市；(2) 利用 `entry_touch_offset_bps` 解锁的贴盘脉冲反转做市；(3) 基于 `basis_hours_v2` 跑出的 **4 币截面强弱动量信号**（78 窗 `0.25bp` 盈利 `+$2.68`、`1.25bp` 盈利 `+$1.13`，每美元换手可承受 `1.98 bp` 成本，仅在最差 5m K 线模拟下转为 `-$13.84`）开发独立低换手 **Maker-First / PostOnly 贴盘追价执行器**（严格分账、独立预算，与东京 W07–W16 隔离研究）。
2. **`02_hedge_arbitrage`（双腿跨市场对冲套利）**：
   - `pair_protection_ablation_v1` 50 组对照证明，在冻结模型假设下单纯收紧/放宽双腿滑点保护（1.0–5.0 bp）无法独自将历史九窗转正（`-2.06` 至 `-1.83` 美元）。核心瓶颈仍是**低频高确定性资金费/价差基差入场阈值过低**与**对冲腿延迟滑点预算**；继续维持冻结基线完成 `W07–W16` 前瞻检验，不在未获前瞻证据前强行调整对冲参数。
3. **`03_incentive_turnover` / W07–W16 联合门禁与标签口径**：
   - `fair_value_arrival_label_v2` 证明抵达后标签（`arr -> arr+1000ms`）在全部 4 个后段窗 MSE 严格低于原标签；待 `W07–W16` 冻结前瞻窗口积累充足独立样本后，下一版多特征模型可直接切换为“抵达后净收益 + 手续费扣除后的 PostOnly 填充加权标签”。
