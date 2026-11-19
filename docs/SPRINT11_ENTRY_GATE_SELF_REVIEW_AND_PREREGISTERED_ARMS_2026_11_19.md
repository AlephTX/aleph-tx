# Sprint 11：自审修复套利入场 Markout 闸门（`S11-1`）与预注册消融臂生成器（`S11-2`）（2026-11-19）

**状态**：本地代码与脚本完成，三赛道继续 `HOLD`。不触碰 Codex 认领的多日 Carry、多重检验审计、留出冻结脚本、`docs/PERP_RELATIVE_VALUE.md` 与数据源登记。

---

## 一、 `S11-1`：Sprint 9 `EntryMarkoutGate` 自审发现的两个缺陷

对 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) 中 Sprint 9 新增的套利入场 markout 闸门做自审，发现：

1. **闸门可能永久锁死**。闸门只从自身开仓成交学习逆向 markout，样本没有时效。一旦一串逆向成交把所需距离抬高，闸门会拦截全部新开仓，从而不再产生新成交来更新样本，旧样本永远有效，余下整个窗口都不会再开仓。做市侧的 markout 环形缓冲在超过 `max_sample_age_ms` 没有更新时整体失效，可以自我恢复；套利侧此前没有对应机制。
2. **报价热路径上每次都分配并排序**。`active_markout_bps` 在每个报价档的检查中都 `collect` 到 `Vec` 并排序，违反仓库“报价路径避免可避免的分配”的要求。

**修复**：

- `EntryMarkoutGate` 新增可选 `max_sample_age_ms`（校验范围 `1,000–3,600,000 ms`；不设置时保持旧行为，便于对照）。超龄标签在 `advance` 中剔除，样本不足 4 个时回到先验值。
- 分位数改为仅在标签变化时于 `advance` 中重算并缓存；排序使用栈上定长数组（`max_samples` 上限即 256），报价检查只读缓存值。
- 报告新增 `aged_out_samples`（超龄剔除数）与 `unlabelled_expired`（到期 1 秒内没有新鲜中价而未能标注的成交数，以及待标注队列溢出数），避免数据缺口时的样本选择被悄悄吞掉。
- 新单元测试 `entry_markout_labels_age_out_so_a_blocked_gate_cannot_stay_locked` 覆盖：逆向 10 bp 后所需距离为 `2 + 10 + 0.5 = 12.5 bp`；无新成交时标签超龄后回到先验 `1.0 bp`；缺新鲜中价的标签计入 `unlabelled_expired`；不设时效时旧标签持续有效；非法时效被拒绝。

> [!NOTE]
> 闸门的费用项默认使用对冲腿 taker 费。在 `passive_loop_only` 下退出通常为挂单，但超时后仍会退回主动对冲，因此保守沿用 taker 费；如需按挂单退出计价，可设 `include_hedge_taker_fee = false`，须在冻结前决定。

## 二、 `S11-2`：预注册消融臂生成器

[build_preregistered_ablation_arms.py](../scripts/build_preregistered_ablation_arms.py) 从冻结的基础配置一次生成四臂，并写出含每臂 SHA-256 的 `manifest.json`，供持有冻结脚本锁的一方直接登记：

| 子命令 | 输入 | 四臂 |
| --- | --- | --- |
| `mm` | `RecordedMakerConfig` 基础配置 + `extract_mm_calibration_inputs.py --calibrate` 输出 | 对照 / 仅 `quantile_floor_p` / 仅风险预算（`as_kappa`、`as_gamma`、`as_risk_budget_bps_at_max_pos`）/ 两者 |
| `arb` | `MakerFirstConfig` 基础配置 + `EntryMarkoutGate` JSON | 对照 / 仅 `passive_loop_only` / 仅闸门 / 两者 |

约束：`mm` 要求基础配置已有 `strategy.markout_reserve`（其放宽上限与样本时效保持冻结），Core-only 时要求开启本地 markout；校准文件的 `split_label` 必须是 `selection_dev` 或 `historical_regression`。四臂一律开启 `diagnose_cross_market` 与 `calibration_telemetry`（Sprint 10 已验证不改变原报告字段）。`arb` 要求基础配置含 `passive_offset`。除 `ablated_keys` 所列键外，各臂与基础配置完全相同。

## 三、 `S11b`：响应 Codex 02:02 独立事项——校准器零成交档与空直方图

Codex 复核 Sprint 10 时指出 [calibrate_mm_markout_and_as.py](../scripts/calibrate_mm_markout_and_as.py) 两个问题，均属实并已修复：

1. **有暴露但零成交的距离档被丢弃**。旧拟合只对有成交的档做对数线性回归，远档“挂了很久却没成交”的证据被丢掉，`κ` 系统性偏低（报价被认为在远处仍有成交）。现改为带暴露时间偏移的泊松极大似然：`fills_k ~ Poisson(T_k · A · exp(-κ d_k))`。用轮廓似然把问题化为单调得分方程，二分求唯一根，并给出 Fisher 信息标准误 `kappa_se_per_bps`。旧对数线性估计保留为 `loglinear_positive_bins_kappa_per_bps` 供对照。所有成交都落在最近档时 `κ` 无界，直接拒绝。
2. **全空 markout 结构仍能生成覆盖参数**。此前全零直方图会得到“逆向损失 0”，下沿退化为 `fee + 0.5 · min_profit`。现要求至少一个周期的已标注成交数达到 `min_markout_samples`（默认 20），样本不足的周期标记为 `below_min_samples` 并排除；没有合格周期时拒绝输出。另加 `min_total_fills`（默认 20）作为 `κ` 拟合门槛。两个阈值已加入 [extract_mm_calibration_inputs.py](../scripts/extract_mm_calibration_inputs.py) 命令行参数。

新测试：零成交档使泊松 `κ` 大于旧估计且满足得分方程；`κ` 无界时拒绝；全空或样本不足的直方图拒绝；`min_markout_samples < 1` 拒绝。原精确指数样本上两种方法都恢复 `κ = 0.4`、`A = 2.0`。

> [!NOTE]
> Codex 同时认领了 `recorded.rs` / `recorded/calibration.rs` 的报价暴露生命周期修正（跨盘口下单 1 ms 被记为 1 s、撤单前 990 ms 被记为 0，即本人 Sprint 10 文档所述“按盘口事件分段常数”近似在实际中的偏差）。本轮未触碰这两个文件。

## 四、 本地验证门禁记录

| 门禁 | 结果 |
| --- | --- |
| `make fmt-check`、`make check` | 通过 |
| `cargo clippy --locked --all-targets -- -D warnings`（默认与 `--features research-replay`） | 0 告警 |
| `make test` | Rust `665+5+8` 通过（6 ignored）；Go 5 个测试包 `-race` 通过；scripts `192` 通过 |
| `cargo test --locked --features research-replay` | Rust `665+5+8` 通过（6 ignored） |
| `make test-python` | `42` 通过 |
| `make build`、`make trade MODE=check CONFIG=config.example.toml` | 通过（`Configuration OK`） |

本轮未改动 dashboard，未重跑 `make dashboard-build`（Sprint 10 时通过）。

`S11b` 仅改 Python 校准脚本及其测试；改动后 `scripts` 全量 `195` 项通过、`git diff --check` 通过，Rust 侧无变化未重跑。
