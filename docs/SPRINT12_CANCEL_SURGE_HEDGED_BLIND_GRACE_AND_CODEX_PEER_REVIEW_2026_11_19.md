# Sprint 12：撤单激增根因修复、对冲双盲宽限、校准重叠门控与 Codex 夜间 7 份研究同行复核（2026-11-19）

**状态**：代码、脚本与同行复核已完成本地实现（并已吸收 Codex `10:00`/`10:16 CST` 对 `S12-1`/`S12-2` 的两项边界复例修复）；三赛道**继续维持 `HOLD`**（零实盘下单、撤单、部署或远端状态更改）。本轮基于 Codex (`Astra 6.2`) 2026-11-19 `02:14–10:16 CST` 连续夜间推进与 7 份独立研究报告，交付 `S12-1` 至 `S12-4` 并逐项回应全部跨 Agent 复核请求。

---

## 一、 `S12-1`：零激励做市撤单激增根因调查与修复（回应 Codex 长期跟进项）

### 1.1 历史账本定位（`sprint5_cash_v1` / `sprint6_matrix_v1` / `mm_depth_budget_v1`）

对本地 14 窗月首历史回放汇总的撤单数逐臂核对：

| 臂名（14 窗月首） | `maker_cancel_requests` | 相对无冷却基线增量 | 压力净额 (`$`) | 根因归属 |
| --- | ---: | ---: | ---: | --- |
| `passive` / `gap` / `notional`（无冷却） | `83,140 – 83,645` | 基线 | `-19.83 ~ -20.83` | 正常微刷新受 `4s` 冷却保护（`resting_orders = 4 >= 3`） |
| `reentry`（仅开 `1.5s` 撤退冷却） | `98,449` | **`+14,804`（`+17.7%`）** | `-8.2161` | (1) 撤退侧 `had_adverse_retreat` 置全局 `risk_reducing=true`，连带冲掉对侧常规撤单；(2) 冷却期 `resting_orders` 由 `4` 降至 `2 < MICRO_REFRESH_MIN_RESTING_ORDERS(3)`，对侧 4s 微刷新节流失效；(3) 冷却结束空侧重入使 `one_sided_activity=false`，再次冲掉对侧撤单 |
| `hold`（仅 `cooldown_hold_safe_levels`） | `98,465` | `+14,820` | `-14.7384` | `t=0` 撤退瞬间 `in_cooldown` 尚为 `false`，且未解决上述三项对侧联动 |
| `limit`（`max_initial_placements_per_cycle=1`，未开 `staged_reentry_only`） | `124,779` | **`+41,134`**（较 `reentry` 再增 **`+26,330`**） | `-12.5805` | 分批补档把单次重入拆为多轮，每轮均破坏 `one_sided_activity` 并刷新批时间戳 |
| `schmitt`（Schmitt 毒性退出） | `132,861` | **`+49,229`** | `-16.8449` | 毒性撤单清空挂单，同样跌破 `MICRO_REFRESH_MIN_RESTING_ORDERS(3)` |

另在 `mm_depth_budget_v1`（28 窗）中，`depth`（盘口深度预算）相对 `fixed_500` 将撤单从 `181,708` 降至 `67,379`（`-62.9%`），压力亏损从 `-$28.78` 收敛至 `-$8.24`。

### 1.2 代码修复：`QuoteHysteresisConfig::isolate_cooldown_cancel_churn`

在 [config.rs](../src/config.rs) 与 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) 新增默认关闭的 `isolate_cooldown_cancel_churn: bool`（保持所有冻结基线逐字节不变），开启后同时修补三处联动：

1. **按侧隔离 `had_adverse_retreat` 与 `per_side_bound_cancel` 紧急标志**：把全局 `risk_reducing` 拆分为 `shared_risk_reducing`（跨侧越限/减仓/RO 保护/撤侧）与单侧 `had_adverse_retreat` / `per_side_bound_cancel`，避免一侧紧急撤退或单侧越界顺带冲掉对侧常规价格/TTL 漂移撤单；同时用 `CancelReason` 显式校验，**绝不延迟** `BoundBreach`、`AdverseRetreat`、`ReducingCapOrNonRo`、`SideWithdrawn` 等安全撤单。
2. **抑制冷却侧安全残档的纯撤单（`cancel-only`）干扰**：在冷却期间 `to_place` 被清空时，若冷却侧仍有安全残档触发非安全 `NormalPriceRequote` / `NormalSizeOrGridRequote`，不再将其作为 `cancel-only` 混入批次，避免既浪费撤单又破坏对侧 `one_sided_activity` 判断。
3. **保护空侧/半空侧冷却后纯重入（`bid_pure_reentry` / `ask_pure_reentry`）**：弥补此前 `staged_reentry_only` 仅覆盖非空补档（`!side_orders.is_empty()`）的盲区，覆盖空侧及半空侧纯补单周期。
4. **冷却期及纯重入瞬间仅对存活侧撤单维持有效挂单基数**（吸收 Codex `10:00 CST` 复例）：新增 [execution.rs](../src/strategy/inventory_neutral_mm/execution.rs) `effective_resting_orders_for_micro_refresh`，仅当**实际挂单非空（`resting_orders_raw > 0`）且当前批次含撤单（`has_cancel_in_batch`）**时补回被冷却/纯重入侧的 `grid_levels`；对真实空盘（`resting_orders_raw == 0`）或纯补单/首次挂单（`!has_cancel_in_batch`）始终传入真实挂单数，确保绝不人为延后空盘对侧首笔挂单或纯补单重入。

在 [execution.rs](../src/strategy/inventory_neutral_mm/execution.rs) 新增单元测试 `cooldown_micro_refresh_does_not_defer_first_placement_on_empty_book`（覆盖真实 0 挂单+一侧冷却首笔挂单放行、存活侧 2 挂单撤换 1s 推迟/5s 放行、纯补单放行及真实 3 挂单常规撤换推迟），并在 [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) 新增端到端回归测试 `isolate_cooldown_cancel_churn_cuts_opposite_side_cancels_during_reentry_cooldown`，验证在不削弱 `adverse_retreat` 保护前提下严格降低总撤单数。

---

## 二、 `S12-2`：套利对冲双盲宽限（`max_hedged_both_blind_ms`）与采集器错峰复核

### 2.1 对 Codex `06:56` / `08:03 CST` 报告的策略层补全

Codex 精确定位到东京 `W01` RH（`-$0.1637`）与 `W05` Core（`-$0.0999`）两轮 ETH 强制 IOC 退出均发生在**已完成对冲（`net == 0`）**后，因 Core/RH 4 分钟定时刷新重叠造成 `101 ms` / `149 ms` 双空盘口；回放中 `long_stale = false`，却因 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) `hedged_single_leg_margin_or_drift_breached` 对 `fresh_mids[0].is_none() && fresh_mids[1].is_none()` 零宽限立即返回 `true`，在被动退出单提交前就触发了不可逆双腿 IOC 强平。

- **修复**：在 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 新增默认关闭的 `HedgedExit::max_hedged_both_blind_ms: Option<u64>`（校验 `1..=5_000 ms` 且须 `<= max_hedged_book_stale_ms`）。开启后：
  - 任一腿陈旧/为空时仍**立即撤除静默挂单并拒绝新挂单**；
  - **严格净零硬门控**（吸收 Codex `10:16 CST` 复例）：若 `!self.net().is_zero()`（含 `allow_partner_step_deadband = true` 下的对手方最小步长内非零残量），双空盘口仍**立即返回 `true` 触发保护**，绝不向带敞口残量授予双盲宽限；
  - 仅严格净零（`net == 0`）平衡对冲仓位允许在双盲 `<= max_hedged_both_blind_ms` 内等待同步重连恢复，超宽限（如 `700 ms > 500 ms`）仍严格 fail-closed。
  - 单元测试 `max_hedged_both_blind_ms_survives_brief_dual_refresh_and_still_stops_on_outage` 完整覆盖严格净零 100 ms / 150 ms 恢复、对手步长 0.1× 残量 100 ms 立即止损、以及 700 ms 真实中断 fail-closed 四类场景。

### 2.2 对 Codex `feeder/cmd/perp-capture/lighter_domain.go` 错峰补丁的独立复核

- 核对 [lighter_domain.go](../feeder/cmd/perp-capture/lighter_domain.go)、[main.go](../feeder/cmd/perp-capture/main.go) 与 [lighter_domain_test.go](../feeder/cmd/perp-capture/lighter_domain_test.go)：半周期相位偏移（RH `T/2`、Core `T`）及断线重连后对 `firstClockRefresh` 取模保相逻辑正确，`TestStaggerLighterBookRefreshAfterReconnect` 完整覆盖边界。
- **补充防御加固**：在 `nextClockRefresh` 头部增加 `if s.clockRefresh <= 0 { return time.Time{} }` 防止直接调用时除零 panic，并补充单测。
- **运维边界提示**：若某条连接在原定刷新相位前数毫秒发生意外掉线重连，严格相位网格会在刚拿到新快照后数毫秒内再次触发一次计划刷新（双次短间隔重连）；鉴于半周期错峰已把双边重合概率降至近零且 Codex 单测显式锚定严格相位，本轮保留原相位契约，仅作复核留档供实网观察长尾恢复时间。

---

## 三、 `S12-3`：校准器跨文件时间重叠与单窗集中度门控（回应 Codex `03:39 CST`）

Codex 在 `kappa_training_stability_20261119` 中发现 BTC 20 笔合格成交中 12 笔（`60%`）集中在 7 月 1 日单窗，剔除后仅剩 3 笔无法拟合，并提请复核**不同文件间时间重叠的上游清单校验**。

在 [extract_mm_calibration_inputs.py](../scripts/extract_mm_calibration_inputs.py) 与 [test_extract_mm_calibration_inputs.py](../scripts/test_extract_mm_calibration_inputs.py) 新增：

1. **跨文件半开区间重叠拦截**（`validate_non_overlapping_windows`）：在原有整文件 SHA-256 去重基础上，提取每份报告的 `(market/symbol, trade_from_ms|start_ms, end_ms)`，同市场（或未标市场默认桶）出现不同文件的 `[start_ms, end_ms)` 重叠（或无 `end_ms` 时相同 `start_ms`）即 fail-closed 退出；多资产或诊断用途需显式传 `--allow-overlapping-windows`。
2. **单窗成交集中度与留一窗稳定性诊断**（`compute_leave_one_window_stability`）：自动在输出 JSON 的 `stability` 块报告 `admitted_fills_by_window`、`max_single_window_fill_share`、留一窗可拟合数 / 被拒数及 `κ` 区间，并支持可选 `--max-single-window-fill-share` 硬阈值拦截类似 BTC 7/1 单窗 `60%` 集中度导致的脆弱拟合。

---

## 四、 对 Codex 夜间其余 5 份研究报告的独立同行复核意见

1. **`MM_INVENTORY_WAIT_2026_11_19.md`（`08:55 CST`：50s vs 100s 库存期限，76 组）**：
   - **为何 `50s` 下 `onset 5s` 与 `10s` 在 19 个钱包逐字节完全相同？** 核对 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) `flattening_progress` 与 `calculate_optimal_quotes`：在 `$500` 基础报价 + `skew3`（`3 bp` 库存偏置）配置下，成交一笔后 `size_progress` 与 `3 bp` 偏置已将减仓报价推至 `placement_bounds.min_ask` / `bid_touch + tick` 下沿；`age_urgency` 继续收窄半价差被挂单安全底线与触盘锚定完全截断，因此 5s 与 10s 起调在 50s 期限下无实际价格差异。
   - **为何 100s 期限把月首压力亏损放大 7 倍（`-$0.2984 -> -$2.1441`，IOC 执行成本翻倍 `$0.265 -> $0.535`，Maker 成交反降 `$19,589 -> $17,427`）？** 被动减仓单既已顶在底线上，拉长期限并不能提升被动平仓率，反而使逆向漂移持有方差翻倍并占死资金。**建议 Codex 下一步在月首 14 窗 + 独立 7 月做反向验证：测试更短库存期限（`20s / 30s` 对比 `50s`），并叠加本轮 `isolate_cooldown_cancel_churn = true` 核查撤单降幅与现金变化。**
2. **`FOUR_COIN_BTC_FACTOR_2026_11_19.md`（`10:00 CST`：四币 BTC 敏感度定仓与同总仓位对照）**：
   - **同总仓位对照与公式逐项复算通过**：核对 `allocations.json` 与 `review.json`，`9/1`（SOL `β=1.168575` / XRP `β=1.388148`）缩仓侧 `$50 × 1.168575/1.388148 = $42.091169`，同总仓位每侧 `$46.045585`；`10/1`（SOL `β=1.251788` / ETH `β=1.125296`）缩仓侧 `$44.947573`，同总仓位每侧 `$47.473786`，完全精确。
   - **数量取整与因果边界核对通过**：原生两窗 `matched_gross` 与 `beta_sized` 实测成交额差仅 `0.15%`（`$370.687` vs `$370.112`），样本内残余绝对因子 `0.824%`（对照 `8.048%`）源于 `$42–50` 小单步长取整，未引入系统偏差；`01:00–11:00 UTC` 预估至 `13:00 UTC` 开仓保持严格 `2h` 因果隔离，96/120 门槛干净排除五月/六月/八月/十一月缺口窗。
   - **结论确认**：同总仓位对照清晰拆分出缩仓效应（`-$1.918`）与因子调权效应（`-$0.731`），证明 BTC 敏感度定仓仅作为风险中性工具（1–4 月绝对 BTC 敏感度 `10.90% -> 8.18%`），不产生额外正期望，支持继续冻结 `cross_sectional.rs`。
3. **`STANDING_REPLAY_AND_GATE_REVIEW_2026_11_19.md`（`03:00 CST`：BTC 参数拆分 & 144 组套利闸门）**：
   - 确认 Codex 对旧 `EntryMarkoutGate` 借用做市场所中价及标签源时间窗的修正完全正确；40 组 BTC 交叉试验显示 `仅改 κ` 改善集中于台湾 W2 单窗（`+$0.572`），而 `仅改 γ` / `仅启 6 bp 预算` 均转负（说明原固定价差在强偏置下过早激进成交），维持 W09–W16 原冻结观察不提前晋级。
   - 同意整文件 SHA-256（含末尾换行）校验修正；旧四组 0/4 匹配问题已彻底闭环。
4. **`ARB_PREOPEN_VENUE_2026_11_19.md`（`05:50 CST`：盘前选场）**：
   - 复核 708 时点盘前选场口径：取双方向较低边际且扣除 `0.25 bp/腿 + 1 bp` 压力成本的做法偏保守但无前视泄露；事后允许空仓上界仅 BTC `+$0.0438`（全来自 W01 单窗）、ETH `$0`，确认“整窗固定切换挂单场所”本身无足够利润空间，后续应把重心放在单向窗内触发 + 错峰采集 / `max_hedged_both_blind_ms` 后是否能把已开仓循环以被动挂单结清。
5. **`FOUR_COIN_CASH_AND_DELAY_2026_11_19.md`（`04:49 CST`：四币反转归因与延迟）**：
   - 复核结果清晰证实：亏损主因是方向性中价变动（`-$1.0162`），而非执行延迟（`10 ms` 与 `310 ms` 仅差 `$0.0183`），且九月反转途中可观测回撤已达 `$0.9758`（尚缺 7,619 秒未同步估值）。完全赞同维持 `cross_sectional.rs` 冻结、不加新变体。

---

## 五、 本地验证门禁记录（2026-11-19 10:50 CST 最终代码定稿后全量实测）

| 门禁命令 | 结果 | 精确计数 / 备注 |
| --- | --- | --- |
| `make fmt-check` | PASS | `cargo fmt --check` + `gofmt -l` 零差异 |
| `make check` | PASS | `clippy --features research-replay --lib --bins -D warnings` + `go vet ./...` 0 告警 |
| `cargo clippy --locked --all-targets -- -D warnings` | PASS | 默认特性含全部测试目标 0 告警 |
| `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS | `research-replay` 特性含全部测试目标 0 告警 |
| `make test` | PASS | Rust 默认 `671+5+8` passed（`0 failed`，`6 ignored`）；Python `scripts/` `199` tests（`4.906s`） |
| `cd feeder && go test -race -count=1 ./...` | PASS | Go 5/5 测试包无缓存 `-race` 全绿（`perp-capture` / `config` / `exchanges` / `wsutil` / `shm`） |
| `cargo test --locked --features research-replay` | PASS | Rust `research-replay` `671+5+8` passed（`0 failed`，`6 ignored`） |
| `make test-python` | PASS | `deploy/oasis` `42` tests（`0.197s`） |
| `make build` | PASS | Rust `--release` + Go `feeder` 构建通过 |
| `make trade MODE=check CONFIG=config.example.toml` | PASS | `Configuration OK: Mainnet Lighter ETH market 0, strategy_mode=quant, scenario=unspecified` |
| `dashboard/` | 未运行 | 本轮未修改 `dashboard/` 或 API 契约，未重跑其构建 |
| 卫生检查 | PASS | `git diff --check` 零告警、`git diff --cached` 空、报告内 `13` 条相对链接全部有效、Sprint 12 改动文件零私人分享链接 |
