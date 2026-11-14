# 执行时间与现金账本审计

日期：2026-11-14。基线：`06995343f8f41c6702a75c12c1d1914244c1bdf1`。

本次在独立检出中做离线修复。没有连接 Oasis 或用户 Mac 工作区，没有读取其未提交文档，没有提交真实订单、取消订单、切换账户等级或部署。当前补丁不改变策略参数、撮合、风险预留或资金费模型，不覆盖原始采集与旧回测结果。主平台和下一轮研究设计见 [研究设计](PRIORITY_VENUE_RESEARCH_DESIGN_2026_11_14.md)。

## 1. 已确认的问题与修复

### 1.1 “撤单等待期成交”原指标是消息接收状态

`recorded.rs` 原有 `cancel_pending_fills` 在处理公共成交消息时检查队列的 `cancel_requested`。它没有记录首次请求时间，因此“源时间在撤单前、消息晚到”的成交也可能进入该指标。旧计数不等于真实撮合发生在撤单等待期；这不直接证明旧现金损益错误，也不能直接推翻用户报告的 15/18 笔成交。

本次在 `diagnose_cross_market` 下增加 `cross_market_attribution.cancel_timing`，保留原字段，并在 Python 汇总明确其接收时刻口径。新诊断记录首次撤单请求、最早模型生效期限和模拟取消时间，按配置 guard 的源时间区间分为七类：

| 类别 | 含义 |
| --- | --- |
| `no_cancel_request_observed` | 未观察到该订单的撤单请求 |
| `before_cancel_request` | 整个源时间区间在请求前 |
| `request_boundary_overlap` | 与请求边界重叠 |
| `during_cancel_window` | 整个区间在请求与模型生效之间 |
| `effective_boundary_overlap` | 与生效边界重叠 |
| `both_boundaries_overlap` | 同时跨越两个边界 |
| `after_cancel_effective` | 整个区间在模型生效之后 |

区间为 `[source_ms - guard, min(source_ms + guard, receipt_ms)]`，精确落在边界的记录仍属不确定。边界是模型中的撤单生效，绝非交易所真实确认。每组记录笔数、成交额、费用和 1/5/30 秒 markout；markout 继续以模拟收到成交为锚，并记录 pending/expired 覆盖。分母使用各组实际可计算 markout 的成交额，零覆盖保留未知。

另记录成交接收滞后、源/接收队列年龄、请求至接收、生效至接收的带符号统计，以及晚接收计数。晚接收与上述源时间分组相互独立，不把二者重复相加成笔数。

诊断始终声明 `source_clock_execution_verified=false`、`guard_calibrated=false`。目前旧源时间并没有因为新增诊断就变成逐笔真实执行时间。只有取得字段覆盖和时钟校准证据后，才可研究更强的执行归因。

### 1.2 公共采集丢弃了可用的执行时间字段

Lighter 当前官方文档区分：Trade `timestamp` 为区块开启毫秒，`transaction_time` 为 sequencer 执行微秒。区块内交易共享区块时间；直接把它当逐笔撮合时间可能使百毫秒量级领先研究偏移。跨机器时钟的差还包含未知偏移，不能称为测得的网络延迟。[T1]

Go 公共成交流白名单原本未保留 `transaction_time`、`block_height`、`sequence_index`。现在用可选原生整数无损保存，不改单位，不从其他字段填造，不增加账户/订单/签名/私有仓位字段。`sequence_index` 只在上游实际提供时保留，未假定每条 Trade 都有。缺失/null 不输出，时间戳零值仍表示不能用于执行时间判断。

Rust 归档承载层使用 JSON 值，新增可选字段兼容旧记录；没有改变共享内存 ABI。该改动尚未部署，原采集文件不会凭空拥有新字段。

### 1.3 新增密封归档的只读时钟审计

`scripts/audit_lighter_trade_clock.py` 消费 `archive_public_capture` 的密封归档，按明确的 Core/RH 域、原生 market ID 和可用时间窗口统计：

- 订阅确认、断流、快照、重复/冲突成交和未确认流上的记录；
- 执行/区块/序号字段的存在、缺失、零值和非法值；
- 执行减区块、接收减执行、接收减区块、可用减接收的整数微秒分布；
- 执行早于区块、执行晚于本机接收、区块至执行差超出所设 guard 的计数。

输出仅为聚合诊断，包含 manifest、所消费分片、审计脚本及 reader 的 hash。读取结束重新验证来源没有变化；不改原始文件或历史 `source_ms`，不补造缺失执行时间。即使字段覆盖完整，`execution_qualified` 和 `stream_quality_verified` 仍为 false。

报告采用同目录临时文件完整写入后原子发布，已有输出拒绝覆盖，序列化失败不留半份最终报告。

以下是命令模板；路径须替换为真实密封归档，market ID 必须按该域独立解析，输出不能已经存在：

```bash
python3 scripts/audit_lighter_trade_clock.py /absolute/sealed_archive lighter_robinhood 1 /absolute/new_clock_audit.json --clock-guard-ms 100
```

可选 `--start-ms` 与 `--end-ms` 必须同时提供；窗口按实际可用时间 `[start, end)`。`100` 只是比较用 guard，不是已校准误差。本次只运行合成测试，没有用户 Oasis 归档，因此没有新的真实分组比例或真实时钟分布。

### 1.4 成本曲线混用了精确现金证明与旧汇总

`replay_turnover_cost_curve.py` 原本检查精确账本的扣额外开销前证明，但后续净额可能来自旧 feedback 字段。合成反例中精确账本为 -0.4、旧汇总为 +0.4，足以使成本判定方向错误。

修复后：

- 只要出现精确证明字段就要求整套证明完整，不回退 legacy 掩盖残缺；
- 同时核对每周期和全局扣开销前/后的精确和、额外开销、反馈字段；已有浮点 1 ULP 容忍未放宽；
- 净现金、净成本以及相关成本指标的现金分子统一取精确扣开销后总额；
- 费用、价格损失和额外开销从精确周期分量独立核对，不能拿旧汇总补齐；缺分量时总净额仍可独立成立，但分解保留未知；
- `cash_components_reconciled` 与 `legacy_headline_consistent` 分别说明精确分解与旧标题的一致性；
- 只有完全没有精确证明字段的旧报告才用 legacy 路径，并核对扣开销前后关系。

执行/价格 markout 仍为非加总诊断。断流、资金费边界、终止未清仓等原来的失效条件保留。该修复提高成本报告可靠性，没有将亏损预算改成必须盈利，也没有重新认证旧 RH 成本数字。

## 2. 当前 Lighter 执行规则核查

官方文档快照读取于 2026-11-14 06:25:18 UTC。除明确写出期间的规则外，没有通用历史生效日期。因此下表只能用于下一轮配置核查，不能反写冻结的历史延迟。

| 域/等级 | 普通 perp Maker/Taker（bps） | Maker / Cancel / Taker 附加延迟（ms） |
| --- | --- | --- |
| Core Standard | 0 / 0 | 0 / 300 / 300 |
| Core Plus | 0.5 / 0.5 | 0 / 300 / 300 |
| Core Premium，零质押档 | 0.4 / 2.8 | 0 / 0 / 140 |
| RH Standard | 0 / 0 | 200 / 200 / 300 |
| RH Plus | 1.5 / 1.5 | 200 / 200 / 300 |
| RH Premium，最低量档 | 1.2 / 3.5 | 0 / 0 / 200 |

上表不包括网络、排队、SDK、重试和真实确认延迟；其他市场/等级需单独读取。当前 RH Standard cancel 文档为 200 ms，不能悄悄替换旧实验的 300 ms 假设再声称仍是同一实验。[T2]

Standard 的读请求与交易共享滚动分钟限制；Plus/Premium 另有交易请求预算和 Volume Quota。HTTP/WS 不是两个独立交易预算；批次请求和其中的操作/配额不同。研究里的 `maker_cancel_requests` 计数是首次请求取消的不同订单，不是网络 POST 数，不能拿累计数除十小时证明没有触发限流。[T3]

Volume Quota 当前涉及 Create、Modify、Grouped、CancelAll，单个 Cancel 类型不扣此配额但仍受请求限制。官方定价为每个付费 quota 0.0005 USDC/USDG，适用等级、免费调用、成交补充和批处理语义均须按规则核算。其潜在成本足以影响不足一美元的筛选利润，实际需求未知时不能填零或直接按订单数估算。下一轮需记录 rolling 60s 请求、动作、查询与重试，而不是只统计 quote refresh。[T4]

## 3. 验证结果

环境为 Linux 独立工作区，Rust 1.95.0、Go 1.25.0。没有复用用户报告为本轮执行证据。

| 检查 | 基线 `0699534` | 本次最终代码 |
| --- | --- | --- |
| Rust，库与 binaries | 551 通过，7 忽略 | 558 通过，7 忽略 |
| Python research tests | 85 通过 | 102 通过 |
| Go `-race ./...` | 通过 | 通过 |
| 格式与静态检查 | 未单列基线 | `make fmt-check check` 通过 |
| 构建 | 未单列基线 | `make build` 通过；启用 `research-replay` 的 maker_replay/cross_venue release 构建通过 |

新增用例覆盖延迟消息跨撤单边界、两边界重叠、重复取消、批次生效时钟、markout 覆盖、诊断开关不改变完整现金/风险结果；Go 大整数/空值/隐私白名单兼容；精确与旧现金总额冲突、分解不完整；归档缺字段、乱序、快照/重复和报告不覆盖。

测试证明代码边界和回归行为，不证明策略盈利、真实执行延迟、奖励资格或 Oasis 数据完备。下一步要在保留原证据的条件下，对封存后的真实归档另建诊断结果。

## 4. Mac 工作区接入复核

2026-11-14 将 Downloads 交付应用到 `0699534`。ZIP 内的补丁、研究设计和审计说明与单独下载版本一致；应用后 13 个文件逐一匹配交付清单的 SHA-256。此后仅补充研究定位、历史指标解释和本地验证说明，代码保持交付版本。

本机使用 Rust 1.95.0、Go 1.25.0，平台为 macOS arm64。`make fmt-check`、`make check`、`make test` 通过：Rust **556 通过、6 忽略**，Python research **102 通过**，Go `-race ./...` 通过。上述 Linux 558/7 是交付方独立检出的记录，本轮没有在 Linux 重跑，不混算为本机结果。当前环境的 Go 默认版本不同，检查显式选择 `GOTOOLCHAIN=go1.25.0`。

`make build` 和 `cargo build --locked --release --features research-replay --bin maker_replay --bin cross_venue` 均通过。新增审计 CLI 的帮助入口、文档本地链接和 `git diff --check` 通过。15 个本轮文件完成内容审查及敏感模式扫描，未发现敏感项；原有两份其他工作文档不在本轮变更清单内。

同时复核官方时间戳和账户类型文档，确认毫秒区块时间与微秒执行时间的区别，以及当前 RH Standard Maker/Cancel 的 200 ms 附加延迟。历史配置未更新。README 增加研究入口，进展文档修正旧撤单指标的解释；Arcus 现货抵押组合保留低频、半自动定位。

本次没有连接 Oasis、重启采集或运行真实归档回测，没有新的 Binance A/B 结果。原有 `docs/PERP_RELATIVE_VALUE.md` 和 `docs/STRATEGY_RESEARCH_2026_11_13.md` 的内容 hash 与接入前一致。测试日志留在 Git 外；代码尚未 commit 或 push。

## 5. 来源

- [T1 Lighter timestamp semantics](https://docs.lighter.xyz/developers/reference/timestamps.md)
- [T2 Lighter account types](https://docs.lighter.xyz/developers/reference/account-types.md)
- [T3 Lighter rate limits](https://docs.lighter.xyz/developers/reference/rate-limits.md)
- [T4 Lighter volume quota](https://docs.lighter.xyz/developers/reference/volume-quota.md)

上述原文快照的 SHA-256：

| 来源 | SHA-256 |
| --- | --- |
| T1 | `7953935bda23a938eebaf9e741ec47c490bb6cf5d04e74431195c36f40923a0c` |
| T4 | `c542f5e7385e72b23924100ad0d050bc526343fc316de7fc78b8dd7b23f224fc` |
