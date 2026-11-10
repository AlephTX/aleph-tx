# AlephTX 快速上手

先验证配置，再用内置场景检查策略、两腿成交和恢复流程。以下命令在仓库根目录执行，使用合成行情和本机测试服务。

## 1. 验证环境与配置

安装 Rust 1.95.0、rustfmt 和 Clippy。Go 版本以模块文件为准。
[构建与 CI](../README.md#validate-locally) · [Go 模块](../feeder/go.mod)

```sh
make trade MODE=check CONFIG=config.example.toml
make fmt-check
make check
make test
make build
```

示例配置输出 `strategy_mode=quant`。它选择 Lighter 单场所做市。启用
`inventory_hedge` 时，配置还需要同标的的 Hyperliquid 行情和库存对冲限额。
检查模式会校验网络、交易对映射和模式约束，无需加载账户凭据。
[配置规范](CONFIGURATION.md)

## 2. 运行离线回放

```sh
target/release/cross_venue hedge-replay examples/hedge_replay.json
target/release/cross_venue perp-replay examples/crypto_funding_paper.json
target/release/cross_venue perp-replay examples/equity_basis_paper.json
```

第一组场景覆盖部分成交、确认丢失和对冲对账。后两组分别验证资金费率持有和
股票永续基差回归。核对入场和拒绝次数、开平仓两腿的成交、手续费、滑点和残余敞口。
[纸面执行报告](PERP_RELATIVE_VALUE.md#read-the-execution-report)

真实行情研究使用公共盘口归档。对同一录制文件编译不同路线时，保留各自的市场参数、
交易时段、独立参考价和资金费率证据。样本外测试需提前固定训练截止和策略参数。
[公开行情录制](PERP_RECORDING.md)

## 3. 准备由监督进程管理的交易会话

在 Linux x86-64 上使用专用账户和本地配置。测试网和主网的启动模式均可发单和撤单。
按运行手册核对账户权限、市场精度、库存限额、停止流程和异常恢复，再启动相应会话。
如有活动恢复日志，先对账并确认所涉场所的挂单和仓位。
[交易与恢复手册](LIVE_TRADING.md)
