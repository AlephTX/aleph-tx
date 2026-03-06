---
description: Protobuf/gRPC service definitions for AlephCore trading API
alwaysApply: true
---

# proto/

> gRPC service and message definitions for cross-system communication.

## Key Files

| File | Description |
|------|-------------|
| aleph.proto | AlephCore service definition - market data, trading, state, control RPCs |

## AlephCore Service RPCs

| RPC | Description |
|-----|-------------|
| `SubscribeOrderbook` | Stream orderbook updates |
| `SubscribeTicker` | Stream ticker updates |
| `PlaceOrder` | Submit order |
| `CancelOrder` | Cancel order |
| `GetOpenOrders` | List open orders |
| `GetPositions` | Current positions |
| `GetBalance` | Account balance |
| `HealthCheck` | System health |
| `Pause` / `Resume` | Trading control |

## Gotchas

- `go_package` set to `github.com/AlephTX/aleph-tx/proto`.
- Currently a specification - not yet compiled into generated code in the main build path.
