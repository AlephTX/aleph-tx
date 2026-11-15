---
description: Active Go WebSocket adapters - Lighter Core & Robinhood (public, private V2, account stats), Arcus, Hyperliquid/Entropy/TradeXYZ, and Binance
alwaysApply: true
---

# feeder/exchanges/

> Real-time WebSocket adapters writing lock-free BBO, top-5 depth, 128-byte V2 private events, and account stats into `/dev/shm`. Legacy v3 adapters (`backpack.go`, `edgex.go`, `01.go`) are archived in `../../legacy/v3_archive/feeder/exchanges/`.

## Key Files

| File | Description |
|------|-------------|
| `base.go` | `Exchange` interface and `RunConnectionLoop()` reconnect supervisor |
| `common.go` | Shared exchange/symbol ID constants (`NUM_EXCHANGES = 12` ABI) and symbol registry |
| `lighter.go` | Lighter Core (`2`) & Robinhood Chain (`8`) full-depth incremental orderbook maintainer (`NewLighterWithID`, `offset`/`nonce` continuity check, crossed-book guard, BBO + top-5 depth SHM publication) |
| `lighter_auth.go` | Lighter Poseidon2 + Schnorr WebSocket auth token generator |
| `lighter_private.go` | Lighter private order/trade stream -> 128-byte `EventRingBufferV2` (`FeeSource` attribution, bounded LRU dedup) |
| `lighter_account_stats.go` | Lighter account stats WS + REST polling -> 128-byte `AccountStatsWriter` |
| `arcus.go` | Arcus (`7`) `l2Orderbook` WebSocket adapter (`nLevels: 20`, monotonic µs clock & future-clock guard, BBO + top-5 depth SHM publication) |
| `hyperliquid.go` | Multiplexed Hyperliquid (`NewHyperliquidGroup`) sharing one WebSocket across Core (`1`), Entropy `io:*` (`9`), and TradeXYZ `xyz:*` (`10`) |
| `binance.go` | Binance USD-M Futures (`6`) `bookTicker` combined WebSocket adapter |
| `multivenue_test.go` | Race-checked tests for Arcus, multiplexed Hyperliquid/Entropy/TradeXYZ, and Lighter Robinhood slot isolation |

## Exchange Slot IDs (`ShmBboMessage` / `ShmDepthSnapshot` 12-Slot ABI)

| ID | Slot | Status |
|----|------|--------|
| `1` | Hyperliquid (Core) | Active |
| `2` | Lighter (Core) | Active |
| `3` | EdgeX | Reserved ABI slot (`legacy/v3_archive`) |
| `4` | 01 | Reserved ABI slot (`legacy/v3_archive`) |
| `5` | Backpack | Reserved ABI slot (`legacy/v3_archive`) |
| `6` | Binance USD-M | Active |
| `7` | Arcus | Active |
| `8` | Lighter Robinhood | Active |
| `9` | Entropy (`io:*`) | Active |
| `10` | TradeXYZ (`xyz:*`) | Active |
| `11` | Extended | Reserved active slot |
