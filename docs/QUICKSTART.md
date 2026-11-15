# AlephTX 快速上手指南 (Quickstart)

先在本地无凭据验证配置与共享内存槽位约束，再用确定性合成场景检验策略报价、双腿对冲与崩溃恢复，最后接入 24×7 公开盘口归档与三层量化数据湖。以下命令均在仓库根目录执行，默认使用离线夹具与本地测试服务，**不会发送任何真实订单**。

---

## 1. 环境校验与全栈编译验证

请使用 **Rust 1.95.0**（含 `rustfmt` 与 `Clippy`）以及 **Go 1.25.0** 工具链。
[构建与 CI 说明](../README.md#validate-locally) · [Go 模块定义](../feeder/go.mod) · [配置参考手册](CONFIGURATION.md)

```sh
# 1. 静态校验配置 Profile（解析网络、市场精度、12 槽位 SHM 绑定与风控护栏，不加载私钥）
make trade MODE=check CONFIG=config.example.toml

# 2. 代码格式、静态检查、全套单元/集成测试与 Release 编译
make fmt-check
make check
make test
make test-python
make dashboard-build
make build
```

- `config.example.toml` 默认输出 `strategy_mode=quant`（Lighter 单场所做市，支持 `Mainnet` `chain_id=304` 与 `RobinhoodMainnet` `chain_id=466324`、多通道 API Key 隔离与外部参考交易所中位数公允价 / 单边越界撤单护盾）。
- 启用 `strategy_mode = "inventory_hedge"` 时，配置同时校验同标的原生 Hyperliquid 行情与对冲限额（仅限 `USDC` 结算的 Lighter Core，禁止与 `USDG` 结算的 Robinhood Chain 隐式混兑）。

---

## 2. 运行确定性因果纸面回放 (Causal Paper Replay)

```sh
target/release/cross_venue hedge-replay examples/hedge_replay.json
target/release/cross_venue perp-replay examples/crypto_funding_paper.json
target/release/cross_venue perp-replay examples/equity_basis_paper.json
```

- **`hedge-replay`**：验证部分成交、确认丢失（Lost Ack）、双场所状态对账与写前日志（WAL）恢复屏障。
- **`perp-replay`**：分别验证加密永续资金费率利差持有（Funding Carry）与美股 RWA 永续跨所基差均值回归（Basis Reversion），输出入场/拒绝原因、双腿成交明细、手续费、滑点与期末残余敞口。
  [纸面执行报告解读](PERP_RELATIVE_VALUE.md#read-the-execution-report)

---

## 3. 公开市场录制、数据湖归一化与事后归因

生产采集在 `oasis-tokyo` 上通过三路物理隔离 systemd 守护进程（`aleph-perp-capture-{crypto,rwa,bulk}.service` + [perp_rolling_runner.py](../scripts/perp_rolling_runner.py)）按 **`10h15m` (`36,900s`) 窗口 7×24h 永续滚动运行**，也可在本地运行单次或进程内零断流切片录制：

```sh
# 1. 本地短时多源录制示例（支持 -segment-duration 进程内无断连切分）
feeder/perp-capture -duration 5m -gzip \
  -binance BTCUSDT,ETHUSDT -coinbase BTC-USD,ETH-USD \
  -lighter 0,1 -lighter-trades -lighter-robinhood 0,1 -lighter-robinhood-trades \
  -arcus-ws NVDA-USD,BTC-USD,ETH-USD -arcus-trades \
  -hyperliquid BTC,ETH,io:NVDA,xyz:NVDA -hl-trades \
  -output recordings/sample.ndjson.gz

# 2. 封存 Bronze 清单并归一化为 Silver Parquet + DuckDB 视图
python3 scripts/quant_lakehouse.py seal-bronze recordings/sample.ndjson.gz \
  --campaign-id sample-run --collector-region tokyo --split-role diagnostic_only
python3 scripts/quant_lakehouse.py normalize recordings/sample.ndjson.gz \
  --output-dir recordings/silver_staging --campaign-id sample-run --collector-region tokyo --precise-times
```

- **三赛道独立验证契约**：量化盈利（`01_quant_profit`）、对冲套利（`02_hedge_arbitrage`）与激励产出（`03_incentive_turnover`）使用各自独立的配置、资金预算与账本，以连续 **10 小时** 为最小验证单元。
  [公开行情录制规范](PERP_RECORDING.md) · [三层量化数据湖与归因指南](QUANT_DATA_LAKEHOUSE.md) · [东京阶段路线图](TOKYO_RESEARCH_AND_EXECUTION_TODO.md)

---

## 4. 准备受监督的实盘交易会话 (Supervised Live Session)

在 `x86_64` Linux 主机（`oasis-tokyo`）上使用最小权限账户（仅具备下单/撤单权限的 Agent Wallet / API Key，严禁提币权限）与受保护的本地配置：

1. 启动前按运行手册核对账户权限、市场 Tick/Lot 精度、单边与净敞口限额、UTC 日损基线以及停机清理流程。
2. 如存在未结清的恢复日志（Active Journal），必须先运行只读审计并完成双场所挂单与仓位对账，严禁强行清除日志重启。
   [实盘交易与恢复运行手册](LIVE_TRADING.md) · [Oasis 监控与控制台指南](../deploy/oasis/README.md)
