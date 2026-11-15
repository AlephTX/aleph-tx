---
description: Active Go WebSocket adapters - Lighter (public, private V2, account stats), Hyperliquid, and Binance
alwaysApply: true
---

# feeder/exchanges/

> Real-time WebSocket adapters writing lock-free BBO, top-5 depth, 128-byte V2 private events, and account stats into `/dev/shm`. Legacy v3 adapters (`backpack.go`, `edgex.go`, `01.go`) are archived in `../../legacy/v3_archive/feeder/exchanges/`.

## Key Files

| File | Description |
|------|-------------|
| `base.go` | `Exchange` interface and `RunConnectionLoop()` reconnect supervisor |
| `common.go` | Shared exchange/symbol ID constants (`NUM_EXCHANGES = 7` ABI preserved) and symbol registry |
| `lighter.go` | Lighter full-depth incremental orderbook maintainer (`offset` continuity check, crossed-book guard, BBO + top-5 depth SHM publication) |
| `lighter_auth.go` | Lighter Poseidon2 + Schnorr WebSocket auth token generator |
| `lighter_private.go` | Lighter private order/trade stream -> 128-byte `EventRingBufferV2` (`FeeSource` attribution, bounded LRU dedup) |
| `lighter_account_stats.go` | Lighter account stats WS + REST polling -> 128-byte `AccountStatsWriter` |
| `hyperliquid.go` | Hyperliquid `l2Book` WebSocket adapter |
| `binance.go` | Binance USD-M Futures `bookTicker` combined WebSocket adapter |

## Exchange Slot IDs (`ShmBboMessage` 7-Slot ABI)

| ID | Slot | Status |
|----|------|--------|
| `1` | Hyperliquid | Active |
| `2` | Lighter | Active |
| `3` | EdgeX | Reserved ABI slot (`legacy/v3_archive`) |
| `4` | 01 | Reserved ABI slot (`legacy/v3_archive`) |
| `5` | Backpack | Reserved ABI slot (`legacy/v3_archive`) |
| `6` | Binance | Active |
