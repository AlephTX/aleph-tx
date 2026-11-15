---
description: Active trading and research strategies - Lighter inventory-neutral MM and causal two-leg relative-value replay
alwaysApply: true
---

# src/strategy/

> Active supervised quoting (`inventory_neutral_mm`) and causal cross-venue paper replay (`relative_value/`, `cross_venue`, `matched_settlement`). Legacy v3 single-venue strategies (`backpack_mm`, `edgex_mm`, `lighter_adaptive_mm`, `arbitrage`) are archived in `../../legacy/v3_archive/src/strategy/`.

## Key Modules

| Path | Description |
|------|-------------|
| `mod.rs` | Common `Strategy` trait (`on_bbo_update`, `on_idle`, `on_shutdown`) |
| `inventory_neutral_mm.rs` | Production Lighter market maker: external fair-value anchor (Hyperliquid + Binance median with causal EWMA basis), A-S spread + quote hysteresis, directional markout/order-flow/cross-market reserves, bilateral `OrderTracker` accounting, and persistent daily loss guard |
| `relative_value/` | Causal two-leg paper execution engine (`pair_hedge`, maker-first queue simulation, `turnover` cost-budget & posture controls, exact cash/fee reconciliation, and cancel-window attribution) |
| `cross_venue.rs` | Cross-venue spread, carry, and instrument equivalence evaluation |
| `matched_settlement.rs` | Collateral and settlement-domain compatibility checks (preventing unapproved cross-currency pairing such as USDG vs USDC) |

## Key Design Invariants

- **Direct Execution (No Boomerang)**: `InventoryNeutralMM` signs and dispatches orders directly from Rust while consuming 64-byte BBO, depth, and 128-byte V2 private events from shared memory.
- **Three-Scenario Non-Subsidy Contract**: Quant profit, hedge arbitrage, and incentive turnover use separate configurations, state, capital/loss budgets, and ledgers (`[deployment]` STP & subaccount isolation in `src/config.rs`).
- **Causal Replay Fidelity**: `relative_value` advances state strictly by local receipt/availability time (`available_at_ms`), never backdating delayed feeds or valuing unconfirmed venue points above zero.
