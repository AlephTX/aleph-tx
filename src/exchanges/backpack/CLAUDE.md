---
description: Backpack exchange REST API client - Ed25519 auth, order management, position tracking
alwaysApply: true
---

# src/exchanges/backpack/

> Backpack exchange REST API client with Ed25519 signature authentication.

## Key Files

| File | Description |
|------|-------------|
| client.rs | `BackpackClient` - REST client with Ed25519 signing, order/position/balance methods |
| model.rs | Data structures: `BackpackOrderRequest`, `BackpackPosition`, `BackpackFill`, `BackpackBalance` |

## API Methods

| Method | Endpoint | Description |
|--------|----------|-------------|
| `place_order()` | POST /api/v1/order | Create limit/market order |
| `cancel_order()` | DELETE /api/v1/order | Cancel single order |
| `cancel_all_orders()` | DELETE /api/v1/orders | Cancel all open orders |
| `get_open_positions()` | GET /api/v1/positions | Fetch current positions |
| `get_order_history()` | GET /api/v1/orders | Trade history |
| `get_fills()` | GET /api/v1/fills | Fill history |
| `get_balances()` | GET /api/v1/balances | Account balances |

## Auth Headers

`X-API-Key`, `X-Timestamp`, `X-Window`, `X-Signature` (Ed25519 over sorted params).
