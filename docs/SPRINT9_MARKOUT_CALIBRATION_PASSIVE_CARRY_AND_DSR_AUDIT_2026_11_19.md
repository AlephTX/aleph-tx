# Sprint 9 策略与基础设施升级报告：Markout 驱动校准、纯被动循环、多日 Carry 与多重检验审计（2026-11-19）

**状态**：代码、脚本与预注册清单已完成本地实现；三赛道**继续维持 `HOLD`**（零实盘下单、撤单、部署或远端变更）。本文档综合 Claude Code 五项优先建议（[.agent-state/research/DEEP_RESEARCH_THREE_SCENARIOS_2026_11_19.md](../.agent-state/research/DEEP_RESEARCH_THREE_SCENARIOS_2026_11_19.md)）与 Codex (`Astra 6.2`) Sprint 8 验证结论（[.agent-state/research/SPRINT8_QUEUE_CAPITAL_AND_W07_2026_11_19.md](../.agent-state/research/SPRINT8_QUEUE_CAPITAL_AND_W07_2026_11_19.md)），交付 `S9-0` 至 `S9-5` 升级并邀请跨 Agent 同行复核。

---

## 一、 Sprint 8 / 跨团队交接复核与 `S9-0` 队列边界加固

1. **Codex Sprint 8 验证要点与本轮接力**：
   - `W07` 保留为正在进行的独立验证采样，本轮严格只读 `.agent-state/forward_ops_20261118/` 与 `W07` 产出；所有前瞻留出清单元预注册至 untouched `W08–W16`（`S9-5`）。
   - Codex 实测台湾 `$1,000 / 25×` 方案在 `7/18`、`7/21` 两日新样本亏损，`7/24` 进入 `trade_withheld`；下文 `§2` 给出剥离 `0.25 bp` 模型税后的完整三分解与项目合格得分重算结果。
2. **`S9-0` Sweep-Through 成交穿透单调性加固**：
   - 在 [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) 与 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 的 `ReplayQueueDepth::trade`（及 `depth.trade`）放宽了逐笔成交价格过滤边界，使主动买扫穿挂单价 `price >= q.price - tick * 0.1`、主动卖击穿挂单价 `price <= q.price + tick * 0.1` 的穿透成交（sweep-through prints）均能正确扣减队列前方与自身深度，同时保持未激活挂单（pre-activation）不误计为撤单扣减，新增单元测试 `pre_activation_sweep_through_print_is_not_credited_as_cancellation` 锁定回归行为。

---

## 二、 优先动作 #1（`S9-1`）：可审计三分解摩擦账本、`OPT-01` 与 `$1k/25×` 重算

### 2.1 三分解执行摩擦与分项目合格得分指标

- 在 [audit_lighter_program_scope.py](../scripts/audit_lighter_program_scope.py) 新增：
  - `decompose_execution_friction`：将总价差/实现PnL、固定每日基建摊销（默认 `$50 / 30d = $1.6667/d`）与三项可执行摩擦 `simulated_slippage`、`rejection_and_retry`、`partial_fill_residual` 显式拆分，彻底剔除 `0.25 bp` 拍脑袋模型税。
  - `score_program_cost_effectiveness`：按项目核算 **`eligible_score_per_usd_cost`** 与 `usd_cost_per_eligible_score_unit`（覆盖 `rh_points`、`lighter_lpp`、`arcus_rwa`、`hip3_tier_volume`、`zero_reward`），当 `eligible_maker_turnover == 0`（如 Core Standard 单场所自对自，无 RH 合格量）直接判定 `zero_eligible_rh_maker_turnover`，拒绝使用虚高总成交额充当分母。

### 2.2 HIP-3 费用公式与 `cross_venue capabilities|capability-check`（`OPT-01` 闭环）

- 在 [venues.rs](../src/venues.rs) 新增 `Hip3AssetFeeSchedule` 与 `compute_hip3_execution_fees`，编码 HIP-3 机制边界：部署者 2× 基础费率乘数（`2 × deployerFeeScale`）、Growth Mode 下费用与 14 天阶梯成交量积分同步削减 `90%`（`0.10×` 保留），并在结构上拒绝把 HIP-3 核心加密货币集合混入零费/低费激励资格（unit test `hip3_execution_fees_enforce_growth_mode_cuts_and_core_crypto_disjointness` 覆盖）。
- 在 [cross_venue.rs](../src/bin/cross_venue.rs) 新增 CLI 子命令 `cross_venue capabilities` 与 `cross_venue capability-check <VENUE>`（对接 `audited_execution_capabilities`），关闭 `OPT-01` 交易所能力审计运行入口缺口。

### 2.3 Codex Core BTC `$1,000 / 25×` 全 12 个已结算窗重算结果

由 [rescore_decomposed_overhead.py](../scripts/rescore_decomposed_overhead.py) 对 `.agent-state/forward_ops_20261118/sprint8_capital_transfer_v2` 重算（含 `taiwan`、`monthly`、`independent_july`、`fresh_july` 的 `7/18`+`7/21`，另有 `fresh_july 2026-07-24` 为 `trade_withheld` 未纳入已结算汇总）：

| 队列分组 | 已结算窗 | Maker 成交额 (`$`) | 总成交额 (`$`) | 剔除 `0.25 bp` 税 (`$`) | 变动净盈亏 (`$`) | 变动成本 (`$/M` Maker) | 全包净盈亏 (`$` 含固定基建) | RH 合格量 / `program_score_rh` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| `taiwan` | 2 | `35,104.97` | `55,493.92` | `1.3873` | `+29.7239` | `-846.71` | `+28.3350` | `0`（`zero_eligible_rh_maker_turnover`） |
| `monthly` | 6 | `45,148.82` | `80,969.71` | `2.0242` | `-13.5544` | `300.22` | `-17.7211` | `0`（`zero_eligible_rh_maker_turnover`） |
| `independent_july` | 2 | `9,234.95` | `18,472.26` | `0.4618` | `-2.3619` | `255.76` | `-3.7508` | `0`（`zero_eligible_rh_maker_turnover`） |
| `fresh_july` (`7/18`, `7/21`) | 2 | `11,289.07` | `22,563.68` | `0.5641` | `-21.5396` | `1,908.01` | `-22.9285` | `0`（`zero_eligible_rh_maker_turnover`） |
| **合计 (`all_settled`)** | **12** | **`100,777.82`** | **`177,499.58`** | **`4.4375`** | **`-7.7321`** | **`76.72`** | **`-16.0654`** | **不可靠：台湾单窗过拟合证伪，且无 RH 积分** |

> 结论：即使全额剥离 `$4.4375` 的 `0.25 bp` 模型税，`$1,000 / 25×` Core BTC 在样本外月度、7 月独立样本及 7 月新增窗（`7/18`+`7/21`）全部显著亏损（全包 `-16.0654` 美元 / 实测 `$159.41/M`），证实单纯调资金与杠杆无法绕开逆向选择与拥挤成交质量问题，必须启用 `S9-2` 的 Markout 报价保护与经验校准。

---

## 三、 优先动作 #2（`S9-2`）：Markout 驱动做市下沿与 `(κ, γ)` 解耦校准

1. **零分配多周期 Markout 曲线与分位保护（[markout.rs](../src/strategy/inventory_neutral_mm/markout.rs)）**：
   - 在固定容量环上无堆分配维护 `[1s, 5s, 30s, 60s]`（`[1.0, 5.0, 30.0, 60.0]` 秒）四档实现 markout 分布，新增 `adverse_quantile_loss_bps` 与 `curve_snapshot`；单元测试 `multi_horizon_curve_and_adverse_quantile_drive_side_loss_without_allocating` 验证逆向 `p` 分位损失提取。
2. **非减仓入场报价下沿与风险预算 `γ`（[config.rs](../src/config.rs)、[inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs)）**：
   - 新增 `MarkoutReserveConfig::quantile_floor_p`（校验范围 `(0.0, 0.5]`）：在 `calculate_optimal_quotes` 中仅对**非减仓开仓侧**强制施加 `fee + markout_quantile + min_profit` 半价差/入场边界下沿，同时放行减仓侧（flatten side）以防被高估价差卡死库存（由 `risk_budgeted_gamma_and_markout_quantile_floor_scale_quotes_by_actual_risk` 覆盖验证）。
   - 新增 `InventoryNeutralMMConfig::as_risk_budget_bps_at_max_pos`（校验范围 `(0.0, 500.0]`）：在保留价公式中从最大持仓风险预算独立反推 `gamma_inv`，与价差宽度里的 `(as_gamma, as_kappa)` 彻底解耦，解决默认参数下 `γ` 对保留价仅移动 `~0.005 bps` 却靠固定 `κ` 拉宽 `2.5 bps` 的严重错配。
3. **离线经验求解器（[calibrate_mm_markout_and_as.py](../scripts/calibrate_mm_markout_and_as.py)）**：
   - 实现 `fit_exponential_kappa`（暴露加权对数线性回归估计成交强度衰减 `κ`）、`summarize_multi_horizon_markout`（提取 `[1s, 5s, 30s, 60s]` 逆向分位）与 `calibrate_mm_parameters`（输出 `as_kappa`、`as_gamma_inv` 与 `markout_quantile_floor_bps`），支持独立验证 `S9-2` 参数配置。

---

## 四、 优先动作 #3–#5（`S9-3`..`S9-5`）：纯被动套利循环、多日 Carry 子赛道、DSR/PBO 与 `W08–W16` 预注册

### 4.1 `S9-3`：套利仅留纯被动循环 + 因果 Markout 入场闸门 + 隔离多日 Carry

- **纯被动循环与入场距离闸门（[maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs)）**：
  - 新增 `PassiveOffset::passive_loop_only`：启用后即使超过 `max_wait_ms`，只要尚未触发风控硬止损（如 `adverse_move` 或 `unhedged_timeout_ms`），即放宽 `min_offset_edge_bps` 至非正继续纯被动挂单退出，避免触发跨所 taker `externalize` 吃单（`ioc_submitted = 0`）。
  - 新增 `EntryMarkoutGate`（`horizon_ms`、`prior_adverse_markout_bps`、`min_profit_bps`、`quantile_p`、`max_samples`、`include_hedge_taker_fee`）：强制新开仓报价距离 `>= 对冲腿手续费 + 条件 markout 分位 + 最低利润`，并通过 `std::cell::Cell` 原地状态追踪。由 `passive_loop_only_and_entry_markout_gate_enforce_s9_3_arb_rules` 联合对偶验证。
- **独立多日资金费率 Carry 子赛道（[multiday_carry.rs](../src/strategy/multiday_carry.rs)、[run_multiday_carry_subtrack.py](../scripts/run_multiday_carry_subtrack.py)）**：
  - 按 `AGENTS.md` 研究隔离契约实现完全独立的策略模块与 `cross_venue multiday-carry FILE` CLI：独立 `capital_usd` / `max_loss_budget_usd` / 固定基础设施费率、因果预锁开仓方向（禁止窗口中途反向翻仓）、入场价差校验 `partner_fee_bps + conditional_markout_bps + min_entry_profit_bps`，以及连续 `10h`（`36_000_000 ms`）无重置核算单元与完整见证资金费结算对账。

### 4.2 `S9-4`：截面量化回溯多重检验审计（冻结新增变体）

严格遵守“先完成全量回溯 DSR / CSCV PBO 审计，在此之前不在 `cross_sectional.rs` 新增变体”的纪律，执行 [audit_multiple_testing_overfitting.py](../scripts/audit_multiple_testing_overfitting.py) 对 `.agent-state/forward_ops_20261118/early_rank_2026_v1/units.json` 审计结果如下：

| 回溯组别 | 覆盖窗数 `T` | 候选策略数 / 等效 `N_eff` | 最优组别净额 / `t_stat` | `PSR(0)` / `DSR(raw)` / `DSR(eff)` | `MinTRL (95%)` / `MinTRL (t=2)` | CSCV `PBO` (`splits=70`) | 结论 |
| --- | ---: | ---: | --- | ---: | ---: | ---: | --- |
| `early_rank_2026_3arms` (`fixed`, `reverse_fixed`, `risk_scaled`) | `206`（CSCV 使用 `200`） | `3` / `1.0318` | `reverse_fixed` `+$1.9249`（`t=0.2064`） vs `fixed` `-$3.9712` | `0.5814` / `0.4737` / `0.6515` | `13,136` / `19,420` 窗 | **`0.8286`**（IS 胜者 OOS 非正概率 `0.7714`） | **无统计显著 alpha，`cross_sectional.rs` 保持冻结** |
| `sprint8_core_btc_1k_25x_12w` | `12` | — | 合计 `-$7.7321`（均值 `-$0.6443`, `t=-0.1877`） | `0.4304` / — | `null`（均值为负） | — | 单窗台湾样本不具外推显著性 |

### 4.3 `S9-5`：`W08–W16` 前瞻冻结清单与跨 Agent Peer-Review 清单

- [freeze_sprint9_w08_holdout_manifest.py](../scripts/freeze_sprint9_w08_holdout_manifest.py) 在结构上仅允许注册 untouched 的 `W08–W16`（`w08..w16`，绝不污染 `W07`），强制要求 `frozen_at_ms < decision_start_ms`，计算各臂 SHA-256 哈希，并在未达 `DSR >= 0.95` 且 `CSCV PBO <= 0.25` 前拒绝独立量化方向准入。
- **邀请 Codex (`Astra 6.2`) 与 Claude Code 同行复核清单**：
  1. 复核 `Recorded` 与 `MakerFirst` 中 `ReplayQueueDepth::trade` 对 sweep-through 成交边界及 pre-activation 单调性的处理是否与 Codex Sprint 8 队列修复完全一致。
  2. 复核 `calculate_optimal_quotes` 中 `gamma_inv` 保留价与 `placement_bounds.min_ask` 安全底线、`quantile_floor_p` 仅限非减仓侧的边界交互，并用实测数据喂入 `calibrate_mm_markout_and_as.py` 在 `W08` 上做盲测验证。
  3. 复核 `passive_loop_only` + `EntryMarkoutGate` 在东京/台湾样本上的纯被动成交率与 `multiday_carry.rs` 独立资金账本隔离。

---

## 五、 本地验证门禁记录

所有验证均在最后一轮代码与格式化修改后本地全量通过（无实盘下单或远端状态更改）：

| 门禁命令 | 结果 | 详情 |
| --- | --- | --- |
| `make fmt-check` | PASS | `cargo fmt --check` 与 `gofmt -l .` 零格式差异 |
| `make check` | PASS | Rust (`--features research-replay --lib --bins`) + `go vet ./...` 全通过 |
| `cargo clippy --locked --all-targets -- -D warnings` | PASS | **0 warnings / 0 errors** |
| `cargo clippy --locked --all-targets --features research-replay -- -D warnings` | PASS | **0 warnings / 0 errors** |
| `make test` | PASS | Rust 单元测试 `658+5+8` 通过（`6 ignored`）；Go 全 5 测试包（`-race`）通过；`scripts` `182` tests (`3.670s`) 全通过 |
| `cargo test --locked --features research-replay` | PASS | Rust `--features research-replay` 全套 `658+5+8` 通过（`6 ignored`） |
| `make test-python` | PASS | `deploy/oasis` Monitor & Telemetry Store `42` tests (`0.204s`) 全通过 |
| `make dashboard-build` | PASS | `oxlint` `0 warnings / 0 errors`（19 files）、`vitest` `19/19`、`tsc -b && vite build`（80 modules） |
| `make build` & `make trade MODE=check CONFIG=config.example.toml` | PASS | Rust `--release` 与 Go feeder 构建成功；离线配置检查 `OK` |
| `git diff --check` & Git 暂存区检查 | PASS | 无空白错误；`git diff --cached` 为空（含未脱敏分享链接文档仅留本地） |

### 11 月 19 日独立复核补记

保留上表作为交付时的结果。按原研究账本重新对齐三个量化配置，有效零成交窗应保留，缺失现金的窗口应排除。285 个计划窗有 209 个公共有效窗，固定反转压力净额 +4.170 美元，原始 DSR 为 0.4883，CSCV 用到其中 208 窗，PBO 为 0.8571。结论仍为 HOLD。两条晚期研究分别使用套利和量化自己的样本交集。

W08 采集早于本报告交付。已核实开窗前冻结的方案为原 W07–W16 协议的 14 个配置。Sprint 9 的清单生成器需要配套可运行配置、实际冻结时间及可核查的落盘记录。此次在 Mac 与 aleph 的项目证据和 campaign 目录中没有找到开窗前的 Sprint 9 三臂 W08 清单。后加的 W08 测试应登记为回放研究。新增策略的前瞻窗口从实际完成冻结后的首个未开窗单元开始。
