---
description: Example programs - inventory_neutral_mm, adaptive_mm, backpack_mm demos
alwaysApply: true
---

# examples/

> Production-ready example programs that serve as entry points for `make` targets.

## Key Files

| File | Description |
|------|-------------|
| inventory_neutral_mm.rs | Inventory-Neutral MM (Lighter DEX) - production HFT strategy (used by `make live-up`) |
| adaptive_mm.rs | Adaptive MM (Lighter DEX) - fee-aware market maker (used by `make adaptive-up`) |
| backpack_mm.rs | Backpack MM - Exchange trait demo with BackpackGateway (used by `make backpack-up`) |
| test_account_stats.rs | Simple account stats SHM reader demo |

## Architecture

```mermaid
graph TD
    subgraph "Example: inventory_neutral_mm.rs"
        INIT[Init Shadow Ledger] --> EC[Spawn Event Consumer]
        INIT --> SHM[Open SHM Matrix + Account Stats]
        SHM --> INMM[InventoryNeutralMM.run]
        EC -->|Background Reconciliation| INMM
        CTRLC[Ctrl+C] -->|Watch Channel| INMM
    end

    subgraph "Example: backpack_mm.rs"
        BP_INIT[Load Backpack Credentials] --> BP_CLIENT[BackpackClient::new]
        BP_CLIENT --> BP_GW[BackpackGateway::new]
        BP_GW --> BP_SHM[Open SHM Matrix]
        BP_SHM --> BP_LOOP[Simple MM Loop]
        BP_CTRLC[Ctrl+C] -->|Watch Channel| BP_LOOP
    end
```

## Gotchas

- These are the actual binaries started by Makefile targets.
- All require the Go feeder to be running first (Makefile handles this).
- Environment variables loaded from `.env.lighter`, `.env.backpack`, etc.
- Graceful shutdown: Ctrl+C triggers watch channel, strategy cancels all orders before exit.
- `backpack_mm.rs` is a demo of the Exchange trait abstraction - order execution is commented out by default.

## Usage

```bash
# Lighter DEX (production)
make live-up        # Inventory-Neutral MM
make adaptive-up    # Adaptive MM

# Backpack (Exchange trait demo)
make backpack-up    # Simple MM with BackpackGateway
```
