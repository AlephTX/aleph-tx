# Binance 同机参考与独立 A/B 回放

本轮先验证 **Lighter Robinhood ETH 的本地报价与方向风险过滤**，Core ETH 是对照域。五个主平台仍为 Arcus、Entropy、TradeXYZ、BULK、Lighter RH。采集范围不等于可回测范围，也不等于奖励资格。

## 实验身份和三种场景

| 场景 | 目标与本轮推进 | 边界 |
| --- | --- | --- |
| 量化盈利 | 已实现研究专用 Binance 方向风险过滤，A/B 完整现金、成交量、markout、撤单与库存独立比较；RH 优先，Core 对照 | 本轮新过滤只接入 maker replay；没有推广到实盘或其他交易所执行模型 |
| 对冲套利 | 同机记录 io/xyz 的 NVDA、SNDK，以及 Binance 对应永续；先区分共同波动和场所偏离 | 合约单位、股数、时段、资金费、结算币与退出成本未确认前，不把价格相近视为可套利；尚无新的套利收益结果 |
| 激励产出 | 风险过滤作为降低不利入场成本的候选；继续按损失预算、每百万**已确认合格**换手成本、实际积分/成本评价 | 尚未把新过滤接入双腿 turnover 撮合器；奖励资格未知时保留未知，不借用 quant 的账本或资金 |

RH 多头与 Core 空头可以组成激励策略自身的两腿，由同一个激励账本支付全部资本、费用、资金费和损失；这不违反场景隔离。每周奖励方向必须重新核实。Arcus 股票现货抵押加合约仍保留低频、半自动定位。

## 时间归档

`perp-capture -binance ETHUSDT,NVDAUSDT,SNDKUSDT` 为每个 Binance 符号建立独立公开 USD-M BBO 连接，并自动对**同进程所有源**开启精确时间归档。`-precise-times` 也可单独启用。默认旧采集格式和实盘 SHM 不变。

每条记录新增 `capture_timing`：随机进程 `run_id`、单连接重连 `connection_epoch`、全局落盘顺序 `sequence`、`receipt_wall_ns` / `receipt_monotonic_ns`、`available_wall_ns` / `available_monotonic_ns`。接收时刻在 WebSocket 读取返回或 HTTP body 读取后取得；可用时刻是队列消费并交给归档编码器的边界，**不代表 fsync 已完成或实盘策略已经收到**。单调时钟是进程起点的相对值，不能跨进程对齐。

Binance 原生 `E`、`T`、`u`、符号与精确字符串 BBO 保留；缺失不填造。`u` 在原始流采样前先检查，只要求前进，不要求相邻差为 1。错误市场、反向 CM 类型、无效价格会触发重连；离线消费者拒绝回退/重复 ID，清空相应历史。断流、重连和源字段缺失均不能继承旧许可。

精确编译通过 `compile_archive(..., precise_times=True)` 校验整个归档的同进程身份、连续序号、时钟及 epoch；所有可用时刻**向上取整到毫秒**。接收时间和交易所时间保持原样，不能将晚到消息移回源时间。

回放参考流按 25 ms 已结束桶保留最后可用 BBO，在桶结束后发布；原可用时间保留为 `observed_ms` 供年龄检查，完整原始流仍在归档。失效事件立即传递并丢弃未发布样本。同毫秒按 trade-feed、trade、local-book、Binance 排序；两个实验臂使用完全相同输入。此顺序是明确的毫秒级保守约定，不是还原亚毫秒撮合。

交易所时钟尚未校准。Binance 源时间与本机接收时间差采用 −50 至 2000 ms 的输入诊断界限，不能当成实测网络延迟。Lighter 回放继续使用原 block timestamp 作为成交源时间；新保留的 execution metadata 供独立审计，不静默替换旧撮合语义。

## A/B 冻结参数

每个域分别使用 500/1000 本地报价币资本，订单名义额 50/100；RH 记 USDG，Core 记 USDC。共八个独立进程/账本，不汇总跨币种现金。

| 项 | A | B |
| --- | --- | --- |
| 报价 | 相同本地策略，12 bps 准入 | 与 A 相同 |
| Binance | 记录且报告可用性 | 额外方向风险过滤 |
| 信号 | 同样计算 | 1000 ms 已可用价格变化，阈值 1 bps，最大年龄 500 ms |
| 下跌 / 上涨 | 不过滤 | 分别撤去新增多头 bid / 新增空头 ask |
| 未就绪、陈旧、断流 | 不过滤 | 阻止新增风险，保留可能的减仓方向 |

过滤只有在账户仓位和成交确认仓位都表明该侧不能减仓时，才将目标数量设为零；随后沿用原有撤单、延迟、挂单风险占用和成交账本。它不是立即取消已经生效的订单。历史配置中的 `core_only` 在这里表示“只用目标域本地定价”，RH 输入也使用这个研究开关。

新窗口使用 RH Standard Maker/Cancel/Taker 附加延迟 200/200/300 ms；Core 为 0/300/300 ms。另加独立 250 ms transport 假设，因此 RH 新单为 450 ms，RH 取消为 450 ms，Core 取消为 550 ms。这些不是账户实测。网络、响应、成交 clock guard、真实执行、滚动限流/volume quota 尚未校准，输出固定标记 `execution_qualified=false`。

当前官方账户表的 RH Premium 最低量档为 1.2/3.5 bps，Core Premium 零质押档为 0.4/2.8 bps，不能混用。本轮不变更账户等级。依据为 [11 月 14 日执行审计](EXECUTION_CLOCK_REVIEW_2026_11_14.md)及其[官方账户类型来源](https://docs.lighter.xyz/developers/reference/account-types.md)；当前重新拉取页面失败，不能声称又独立验证了新的生效日期。

后续公开费率审计已成功归档 [RH overview](https://docs.lighter.xyz/developers/robinhood-chain/overview.md)，确认该页列出的 Standard Maker/Cancel/Taker 附加延迟为 200/200/300 ms，Premium 最低量档为 1.2/3.5 bps。这是新的公开来源快照，不是账户实测，也没有改写上述冻结配置；早先访问失败的记录继续保留。

0.25 bps 额外开销继续单列为未校准模型假设，输出 0/0.1/0.25/0.5 bps 固定现金敏感性。每小时资金费边界平仓、末端 drain、日内损失 guard 均沿用现有连续驱动，不拆成短段重置资本。

## 编译和运行

`examples/eth_core_rh_books.json` 保留 USDC/USDG 原币种，开启 `independent_books=true`、`paper_entries=false`。这只允许编译独立盘口；`RelativeValue` 的跨币种配对检查没有放宽，普通 pair replay 也拒绝带独立盘口标记的 header。冻结入口检查原生 market ID、tick、lot、最小数量及结算币与公开元数据一致。

```bash
python3 scripts/replay_binance_ab.py freeze \
  --engine /research/bin/maker_replay --compiler /research/bin/cross_venue \
  --collector /research/bin/perp-capture --base config.example.toml \
  --capture-profile examples/eth_core_rh_books.json \
  --core-metadata /research/core-market-metadata.json \
  --rh-metadata /research/rh-market-metadata.json \
  --start-ms FUTURE_UTC_EPOCH_MS --output /research/new-frozen-ab

python3 scripts/archive_public_capture.py ingest \
  /research/public.ndjson.gz /research/new-archive
python3 scripts/replay_binance_ab.py run /research/new-frozen-ab /research/new-archive
```

`FUTURE_UTC_EPOCH_MS` 必须替换成冻结至少两分钟后的整数时间戳。warm-up 固定在开始前 60 秒，决策连续十小时，末端 drain 15 秒。所有配置、引擎和 Python 依赖 hash 在开始前冻结，运行前后复核。新输出目录/文件均排他创建，失败证据不覆盖；输入不足不能重排窗口补足十小时。

`capture_binance_window.py` 读取专用目录的 `capture-plan.json`：校验保护实验 hash、时间边界、采集器 hash，等待预定时刻后仅启动自己的公开采集进程。运行时维护至少 14 GiB 磁盘余量及独立文件预算，超限只停止本任务并记录 incomplete；终止标记/时间覆盖核对后封存，保留 `.partial` 证据的硬链接。`campaign-events.ndjson` 为 Git 外追加清单。

原始 Binance 流体量较大。Oasis 只保留采集和必要工具；封存后的分片归档、编译及八臂回放优先在有余量的独立本机工作区完成。任何未完成回放都明确保留 pending，不预填收益。

## Claude 调研的取舍

- 接纳平台优先级、激励成本预算、RH/Core 等级分别建模、Hedge Boost 自有账本、实收时间归档等方向。
- 官方明确排除 Standard-to-Standard 奖励；具体成交的资格映射仍未验证。新公开费率审计可以提供档位表匹配线索，但不据此确认奖励，也不能把缺失费率的旧磁带补成零费率。没有代替用户联系平台。
- Binance 公开 `exchangeInfo` 本轮确认 NVDAUSDT、SNDKUSDT 为 `TRADIFI_PERPETUAL`、状态 TRADING；对应原始 BBO 已收到。但尚未证明其股数、时段、指数和 io/xyz/Arcus 一致，不能据此运行股票套利。Pre-IPO 配对继续待核实。
- Arrakis 的其他时段/网络测得的领先关系不迁移为 Oasis 的 100/700 ms 优势，35–50 ms 建模延迟也不等于实测。历史 aggTrades 不能补造 Oasis 过去的接收时间。
- BULK AURA、Arcus 第三方积分公式、未知空投、某钱包群体与未来反女巫规则不进入确定收益。无官方确认不等于已证明不存在。
- C/D 组的基差修正定价、15 秒波动门、Premium 反事实等均保留后续独立实验身份，避免在本轮同时改变信号、价格和执行条件。

Binance 单币实时 BBO 路由依据[官方 USD-M 公共流](https://developers.binance.com/en/docs/catalog/core-trading-derivatives-trading-usd-s-m-futures/api/ws-streams/public)；合约状态以本轮保存的公开 `exchangeInfo` 快照为准。
