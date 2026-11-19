# Sprint 13：做市撤单隔离方向安全门控、对冲双盲接收时钟分离与 Codex 186 份独立账本深度复核（2026-11-19）

**状态**：代码修复、单元测试与同行复核已完成本地实现；三赛道**继续维持 `HOLD`**（零实盘下单、撤单、部署或远端状态更改）。本轮直接响应 Codex (`Astra 6.2`) `2026-11-19 11:24 / 11:34 CST` 的 186 份 Sprint 12 独立回测、双时钟观测、`W08` 停算时钟补丁及两项优先复核请求。

---

## 一、 `S13-1`：做市撤单隔离 30 秒 Markout 恶化根因与方向安全门控

### 1.1 对 Codex `fill_quality/summary.json` 与逐钱包差异的定位

Codex 在 114 份独立回放中发现：`50s` 期限下开启 Sprint 12 `isolate_cooldown_cancel_churn = true` 使月度 14 窗被动成交 `178 -> 195`，1 秒成交后中价从 `-1.525` 改善至 `-1.110 bp`，但 **30 秒成交后中价从 `-1.027` 恶化至 `-2.200 bp`**，压力净额由 `-$0.2984` 降至 `-$3.2326`：

- **非等待撤单成交分化**：对比 `t50_iso0` 与 `t50_iso1`：
  - 等待撤单（`cancel_pending`）成交由 `128` 笔降至 `121` 笔，30 秒 markout 反从 `-$2.715` 改善至 `-$2.262`（`+$0.453`）；
  - **非等待撤单成交**由 `50` 笔增至 `74` 笔（`+24` 笔），其 30 秒 markout 从 **`+$0.703` 骤降至 `-$1.541`（`-$2.244`）**。
- **单钱包集中暴露**：对 `mm/summary.json` 14 窗逐钱包核对，`2026-07-01_A_BTC` 单窗由 `+$2.2675` 跌至 `-$0.7714`（**`-$3.0389`**，占全部 14 窗回撤 `-$2.9342` 的 `103.6%`，其余 13 窗合计 `+$0.1047`），且该窗撤单数反增 `+347`、吃单强平 `+1` 笔。
- **代码根因**：Sprint 12 的 `isolate_cooldown_cancel_churn` 在两处未区分价格漂移方向：
  1. 冷却侧残档（`bid_in_cooldown` / `ask_in_cooldown`）抑制非安全 `NormalPriceRequote` 时，把**向挂单方向恶化**（Bid `old_px > new_px`、Ask `old_px < new_px`）但尚未触及 `adverse_retreat_bps` 悬崖的次级档位也按住了；
  2. 对侧隔离（`opp_would_defer`）及冷却期虚拟挂单计数在价格反抽时同样可能延后对侧**逆向漂移**撤单，使陈旧激进挂单以“非等待撤单”状态直接被趋势扫单成交，随后在 50 秒库存期触发吃单止损并退出平稳模式（进而推高撤单数）。

### 1.2 代码修复：`cancels_are_strictly_passive` 方向安全门控

在 [execution.rs](../src/strategy/inventory_neutral_mm/execution.rs) 新增纯函数 `cancels_are_strictly_passive`，并在 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) 对照当前顶档目标价（`plan.target.bid_price` / `plan.target.ask_price`）计算 `bid_cancels_passive` / `ask_cancels_passive`：

1. **买单仅允许延后“不激进于当前目标买价”的撤单**：仅当全部待撤买单满足 `order.price <= target_bid_price + FLOAT_EPSILON`（旧买单位于目标买价或更深位置）时返回 `true`；一旦目标买价下移至旧买单下方（`old_px > target_bid_price`，逆向漂移）立即返回 `false` 放行撤单。
2. **卖单镜像对称**：仅当全部待撤卖单满足 `order.price >= target_ask_price - FLOAT_EPSILON`（旧卖单位于目标卖价或更高位置）时返回 `true`；目标卖价上移越过旧卖单（`old_px < target_ask_price`）绝不延迟。
3. **交易所编号、生命周期与空列表严格 fail-closed**（同步吸收 Codex `11:49 CST` 交叉碰撞复例）：严格按 `order.order_index == Some(*cancel_id)`、同侧且 `order.lifecycle != OrderLifecycle::PendingCancel`（与 `resolve_cancel_client_order_ids` 一致）解析待撤订单；`to_cancel.is_empty()`、`target_px <= 0`、仅匹配 `client_order_id` 或处于 `PendingCancel` 均严格返回 `false`。
4. **三处统一接入**：同时约束冷却侧残档抑制、对侧隔离（保持关闭 `isolate_cooldown_cancel_churn` 时的旧 `staged_reentry_only` 路径逐字节不变）及冷却期微刷新虚拟档数门控。单元测试 `passive_cancel_guard_never_defers_adverse_price_drift` 完整覆盖方向边界、编号独立、交叉碰撞（`client=77/idx=10` 激进 vs `client=10/idx=88` 被动）及 `PendingCancel` 生命周期校验。

---

## 二、 `S13-2`：对冲双盲宽限接收时钟分离（回应 Codex `blind_clock` 实测）

Codex 在 `blind_clock/summary.json` 中精准定位到 `grace200` 在 `W01`/`W05` ETH 上于第 `0 ms` 即触发强平的原因：
- `W01` 首帧双空时，接收龄仅 `105 ms`，但交易所源时间龄已达 `180 / 206 ms`（最大 `280 ms`），实际双盲仅 `74–101 ms`；
- `W05` 首帧双空时，接收龄仅 `100 ms`，但交易所源时间龄已达 `346 / 171 ms`（最大 `426 ms`），实际双盲仅 `80–149 ms`。
- 旧实现把 `now - last_source_ms[i] > grace_ms` 混入 `max_hedged_both_blind_ms`，导致平静盘口的既有源时间龄直接耗尽 `200 ms` 宽限。

**修复**：在 [maker_first.rs](../src/strategy/relative_value/paper/maker_first.rs) `hedged_single_leg_margin_or_drift_breached` 中严格分离两个时钟：
- `max_hedged_both_blind_ms` 仅度量**接收时钟上的同步双盲持续时长**：`now - max(last_usable_at_ms[0], last_usable_at_ms[1]) <= grace_ms`（仍硬性要求 `self.net().is_zero()` 且双腿 `last_source_ms[i] > 0`）；
- `paired_stop` 原有的 `max_hedged_book_stale_ms` 检查继续对双腿独立施加**源时间龄 + 接收龄绝对上限**，杜绝老旧源盘口滞留。
- 扩充 `max_hedged_both_blind_ms_survives_brief_dual_refresh_and_still_stops_on_outage`，覆盖 `W05` 静默源龄 `346 ms > grace 200 ms` 且接收龄 `180 ms <= 200 ms` 存活、`250 ms` 接收超限止损与 `5,200 ms > 5,000 ms` 源龄超限止损全部路径。

---

## 三、 `S13-3`：对 Codex `strategy_halted_at_ms` 补丁、`W08` 停算与 `W01–W06` 24 钱包漂移拆解的同行复核

### 3.1 `strategy_halted_at_ms` 补丁与 `W08` 第 67 分钟停算复核

- 核对 [recorded.rs](../src/strategy/inventory_neutral_mm/replay/recorded.rs) 与 [PERP_RELATIVE_VALUE.md](../docs/PERP_RELATIVE_VALUE.md)：`strategy_halted_at_ms = result.is_err().then(now_ms)` 在 `strategy.run`/`continuous::run` 返回错误瞬间捕获模拟时钟（早于 `feed` 排空磁带的 `active_until_ms`），且 `skip_serializing_if = "Option::is_none"` 保证正常报告逐字节不变，**复核通过**。
- 解释 `W08` 四个停算钱包（`turnB_500_{d10,d25,d50}` `41.499s`、`turnB_5000_d50` `44.270s`）：核对 [continuous.rs](../src/strategy/inventory_neutral_mm/replay/recorded/continuous.rs) `L79-83` 与 [inventory_neutral_mm.rs](../src/strategy/inventory_neutral_mm.rs) `L1790-1875`，进入 `research data pause` 时持仓非零且盘口过期，`clean_exchange_state` 执行撤单同步 + 账户确认 + `fetch_cleanup_mid_price`（`20 × 250 ms = 5s` 轮询）及重试循环，最终因无新鲜盘口平残仓而安全 fail-closed 停算，属预期风险保护行为。

### 3.2 东京 `W01–W06` 全部 24 个独立套利钱包入场后漂移拆解（保留 0 成交与未结算窗）

核对 `sprint12_peer_research_20261119/arb/{summary.json,review.json}` 与 [costs.rs](../src/strategy/relative_value/paper/maker_first/costs.rs#L200-L205) 恒等式 `drift_cost = Q(h_0 - h_1) + Q[(m_0 - h_0) - (m_2 - h_3)]`：

- **全量 24 窗分布**（无遗漏）：`19` 窗已结清且 `0` 往返（BTC `11` + ETH `8`，应力现金 `$0.0000`）；`3` 窗阻塞未结算（`w03 ETH r=0`、`w05 BTC r=1`、`w05 ETH r=1`，均 `0` 往返，受 `funding/terminal/cash` 阻塞保留未知）；`2` 窗活跃已结清 ETH 往返：

| 钱包 | 方案 | 配对持有 | 入场执行 (`entry_maker+hedge`) | 平仓执行 (`close/forced`) | 价格漂移 (`drift`) | 费用+模型税+吃单压力 | 压力净额 |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `w01 ETH r=1` | `strict` / `grace200` | `35.5s` | `+$0.01100` | `-$0.00797` | `-$0.12677` (`-12.68 bp`) | `-$0.03998` | `-$0.16373` |
| `w01 ETH r=1` | `grace500` | `152.0s` | `+$0.01100` | `-$0.00110` | `-$0.12870` (`-12.87 bp`) | `-$0.02996` | `-$0.14876` |
| `w05 ETH r=0` | `strict` / `grace200` | `15.4s` | `+$0.01019` | `-$0.01104` | `-$0.05915` (`-5.91 bp`) | `-$0.03993` | `-$0.09992` |
| `w05 ETH r=0` | `grace500` | `46.9s` | `+$0.01019` | `-$0.02884` | `-$0.04726` (`-4.73 bp`) | `-$0.02989` | `-$0.09581` |

- **结构性结论**：
  1. `W01` 在 `35.5s` 时 `drift` 已达 `-$0.12677`，延长持有 `116.5s` 至 `152.0s` 仅微增 `-$0.00193`（**98.5% 的漂移损失发生在前 35.5s——即入场未对冲滞后与初期基差塌陷**）；
  2. `W05` 延长持有 `31.5s` 反而回补 `+$0.01189` 漂移，但被动平仓执行（`-$0.02884`）比强平（`-$0.01104`）多付出 `-$0.01780`，净改善仅 `+$0.00411`；
  3. 两笔合计被动平仓节省 `+$0.01908`，而入场后漂移高达 `-$0.17596`（为退出改善的 **`9.2×`**、入场价差捕获 `+$0.02119` 的 **`8.3×`**），证实消除双盲强平只是卫生前置条件，套利赛道的决定性瓶颈仍是入场瞬间的逆向选择与跨所基差非均值回归。

---

## 四、 本地验证门禁记录（2026-11-19 11:55 CST 最终代码定稿后全量实测）

| 门禁命令 | 结果 | 精确计数 / 备注 |
| --- | --- | --- |
| `make fmt-check` | PASS | `cargo fmt --check` + `gofmt -l` 零差异 |
| `make check` | PASS | `clippy --features research-replay --lib --bins -D warnings` + `go vet ./...` 0 告警 |
| `cargo clippy --locked --all-targets -- -D warnings` | PASS | 默认特性含全部测试目标 0 告警 |
| `cargo clippy --locked --features research-replay --all-targets -- -D warnings` | PASS | `research-replay` 特性含全部测试目标 0 告警 |
| `make test` | PASS | Rust 默认 `672+5+8` passed（`0 failed`，`6 ignored`）；Python `scripts/` `199` tests（`4.958s`） |
| `cd feeder && go test -race -count=1 ./...` | PASS | Go 5/5 测试包无缓存 `-race` 全绿（`perp-capture` / `config` / `exchanges` / `wsutil` / `shm`） |
| `cargo test --locked --features research-replay` | PASS | Rust `research-replay` `672+5+8` passed（`0 failed`，`6 ignored`） |
| `make test-python` | PASS | `deploy/oasis` `42` tests（`0.166s`） |
| `make build` | PASS | Rust `--release` + Go `feeder` 构建通过 |
| `make trade MODE=check CONFIG=config.example.toml` | PASS | `Configuration OK: Mainnet Lighter ETH market 0, strategy_mode=quant, scenario=unspecified` |
| `dashboard/` | 未运行 | 本轮未修改 `dashboard/` 或 API 契约，未重跑其构建 |
| 卫生检查 | PASS | `git diff --check` 零告警、`git diff --cached` 空、报告内 `8` 条相对链接全部有效、Sprint 13 文件零私人分享链接 |
