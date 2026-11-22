# Sprint 10：做市校准数据闭环（`S10-1`）与估值/现金证据分级（`S10-2`）（2026-11-19）

**状态**：本地代码与脚本已完成，三赛道继续 `HOLD`（零实盘下单、撤单、部署或远端变更）。本轮不触碰 Codex 正在复核的 `multiday_carry.rs`、`run_multiday_carry_subtrack.py`、`freeze_sprint9_w08_holdout_manifest.py`、`audit_multiple_testing_overfitting.py`、`docs/PERP_RELATIVE_VALUE.md` 与数据源登记。

---

## 一、 要解决的缺口

Sprint 9 交付了 `quantile_floor_p`、`as_risk_budget_bps_at_max_pos` 与离线求解器 [calibrate_mm_markout_and_as.py](../scripts/calibrate_mm_markout_and_as.py)，但求解器需要的三类输入在回放报告中并不存在：

| 求解器输入 | Sprint 9 回放报告中已有 | 缺口 |
| --- | --- | --- |
| 各报价距离的挂单暴露时长与成交次数（拟合 `κ`） | 无 | 无法拟合成交强度衰减 |
| 每笔成交在各周期的 markout 分布（取分位） | 仅 `maker_markouts` 三档**汇总**金额 | 无法取分位，只能取均值 |
| 实现波动率 `σ`（推导 `γ`） | 无 | 需人工指定 |

因此 Sprint 9 的校准参数此前只能手填，等同于再次“拍脑袋”。

## 二、 `S10-1` 交付

1. **回放端可选遥测**（[calibration.rs](../src/strategy/inventory_neutral_mm/replay/recorded/calibration.rs)，接入 [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs)）：
   - 新配置 `calibration_telemetry`（默认关闭；开启时要求 `diagnose_cross_market = true`，否则配置校验失败）。
   - 报告新增 `calibration` 块：`0.5 bp` 宽、`0–20 bp` 共 40 档加 1 个溢出档的**距离暴露秒数 / 成交次数 / 成交额**；`1s/5s/30s` 三档每笔 markout 的 `0.5 bp` 宽直方图（`[-50, +50) bp`，尾部单独计数）；以及 `sum_sq_log_return`、`sigma_observed_secs` 与合并 `σ`（间隔超过 5 秒视为数据缺口，不计入方差）。
   - 全部为定长数组，热路径零分配。暴露按盘口事件分段常数计（订单在被观察到的那次事件前整段计入），误差上限为相邻盘口事件间隔；成交距离使用成交前最后一个盘口中价，无前视。
   - 单元测试 `exposure_fills_markouts_and_sigma_are_binned_without_lookahead` 覆盖分档、缺口与 `σ`；集成测试 `calibration_telemetry_preserves_baseline_json_and_reconciles_fill_counts` 验证开启后**原有全部报告字段逐字段不变**、分档成交数加未定价成交数等于 `maker_fills`、各周期直方图总数等于对应 `marked_fills`，并验证缺少 `diagnose_cross_market` 时拒绝配置。
2. **聚合器**（[extract_mm_calibration_inputs.py](../scripts/extract_mm_calibration_inputs.py)）：汇总多份回放报告，合并 `σ = sqrt(Σr² / Σ秒)`，丢弃暴露不足 `--min-bin-exposure-secs`（默认 60 秒）的稀疏档与开放溢出档，输出求解器输入；`--calibrate` 同时给出 `strategy_overlay`（`as_kappa`、`as_gamma`、`as_risk_budget_bps_at_max_pos`、`markout_reserve.quantile_floor_p`）。强制 `--split-label`（仅 `selection_dev`/`historical_regression`/`diagnostic`），并记录每个输入文件的 SHA-256。
3. **求解器扩展**：`calibrate_mm_parameters` 现可直接接受 `markout_histograms_by_horizon`；分位落入直方图尾部时标记 `censored` 并拒绝校准（避免把 `-50 bp` 边界误当估计值）。

> [!IMPORTANT]
> `κ` 拟合只覆盖策略实际报过的距离，属于内生样本，**不得外推到未覆盖的距离档**；`σ` 是回放窗口的实现值，不是前瞻预测。

## 三、 `S10-2`：估值与现金证据分级

按 Codex `2026-11-19 01:18 CST` 建议，[rescore_decomposed_overhead.py](../scripts/rescore_decomposed_overhead.py) 为每个输出组标注 `evidence_class` 与 `counts_as_settled_replay_cash`：

| `evidence_class` | 含义 | 本地重算中的组数 |
| --- | --- | ---: |
| `replay_settled_cash` | 回放已结清现金（仍是回放，不是实盘证据） | `cohorts` 段 13 组；另含 `sprint8_core_btc_1000_25x` 各分组及汇总 |
| `replay_modeled_overhead_net` | 台湾两窗按模型开销计算的净额，无 maker/taker 拆分 | 2 |
| `valuation_only_unproven_two_leg_execution` | 资金费双域筛选，未证明双腿可执行 | 8 |
| `valuation_only_reference` | Binance 2023–2025 保本参考值 | 1 |

`carry_dual_domain` 中 7 个零费毛额为正的组（BTC/ETH/SOL/XRP 早期段及 ETH/SOL/XRP 时间段）全部标为估值，不得作为现金结论引用。

## 四、 请 Codex（`aleph`）执行的预注册校准实验

1. **只用选择期数据**：取既有 14 个月首做市窗（或 W01–W06）对照臂配置，仅追加 `diagnose_cross_market: true` 与 `calibration_telemetry: true`（已验证不改变原报告字段），BTC 与 ETH 分别回放。
2. 每个市场运行：
   `python3 scripts/extract_mm_calibration_inputs.py <reports...> --split-label selection_dev --max-pos-for-as <归一化最大仓位> --target-risk-budget-bps 6 --maker-fee-bps 0 --min-profit-bps 1 --quantile-p 0.5 --calibrate --output <market>_cal.json`
3. 冻结四臂（对照 / 仅 `quantile_floor_p` / 仅 `as_risk_budget_bps_at_max_pos` / 两者）后再评估。由于 W08 在 Sprint 9 代码发布前已开窗，建议从 **W09** 起按逐配置冻结凭据作为前向窗（来源为 `selection_dev`，非未触碰留出集）；冻结清单脚本目前由 Codex 持有编辑锁，由 Codex 决定登记方式。
4. 报告所有臂（含失败、缺口、未结清），不得只报最优臂。

## 五、 本地验证门禁记录

| 门禁 | 结果 |
| --- | --- |
| `make fmt-check`、`make check` | 通过 |
| `cargo clippy --locked --all-targets -- -D warnings`（默认与 `--features research-replay`） | 0 告警 |
| `make test` | Rust `663+5+8` 通过（6 ignored）；Go 5 个测试包 `-race` 通过；scripts `189` 通过 |
| `cargo test --locked --features research-replay` | Rust `663+5+8` 通过（6 ignored） |
| `make test-python` | `42` 通过 |
| `make dashboard-build` | oxlint 0 告警/0 错误，vitest `19/19`，Vite 构建成功 |
| `make build`、`make trade MODE=check CONFIG=config.example.toml` | 通过（`Configuration OK`） |

> [!NOTE]
> 首轮全量测试中集成测试新增的 `maker_fills > 0` 断言失败：原通用夹具只有盘口没有成交，导致早期版本的对账断言实际为空检验。已改为带双向扫穿成交的专用夹具，并追加 `1s` 已标注成交数 `> 0` 断言后全量重跑通过。
