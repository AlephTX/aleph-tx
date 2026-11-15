---
description: Active exchange clients and research adapters (Lighter Core/Robinhood, Hyperliquid, Arcus, BULK, Variational)
alwaysApply: true
---

# src/exchanges/

> Modular venue clients for supervised execution (`lighter`, `hyperliquid`) and public-data capture/metadata (`arcus`, `bulk`, `variational`). Legacy v3 adapters (`backpack`, `edgex`) are preserved in `../../legacy/v3_archive/src/exchanges/`.

## Directory Layout

```
src/exchanges/
  mod.rs          # pub mod arcus, bulk, hyperliquid, lighter, variational
  lighter/
    mod.rs        # LighterNetwork (Mainnet 304, Testnet 300, RobinhoodMainnet 466324), market validation
    ffi.rs        # CGO signer bindings + SigningLanePool (round-robin multi-API-key lanes)
    risk.rs       # Isolated/cross liquidation price, cross-market MMR, post-fill safety guards
    trading.rs    # LighterTrading client, dual-state (pending/confirmed) nonce manager, batch/cancel APIs
  hyperliquid/    # Native Hyperliquid + HIP-3 DEX (km / xyz) EIP-712 signing, IOC hedge client, clearinghouse queries
  arcus/          # Arcus public orderbook, market trades, and contract metadata client
  bulk/           # BULK public orderbook, trades, funding, and ticker metadata client
  variational/    # Variational public quote/metadata client
```

## Key Execution Capabilities

### Lighter (`lighter/`)
- **Domains**: `Mainnet` (`chain_id = 304`, USDC), `Testnet` (`chain_id = 300`), and `RobinhoodMainnet` (`chain_id = 466324`, USDG, supervised `quant` mode + public research).
- **Multi-Key Signing Pool**: `SigningLanePool` binds `1..N` `(api_key_index, private_key)` lanes (`LIGHTER_EXTRA_LANES` or `LIGHTER_API_KEY_INDICES` + `LIGHTER_PRIVATE_KEYS`) with independent per-lane nonce state to multiply rate-limit headroom without cross-key nonce races.
- **Dual-State Nonce Recovery**: Separates `confirmed_nonce` from `pending_nonce`, rolls back locally on transport failure before server ingestion, and resyncs via `/api/v1/nextNonce` on `21104` / `21711`.
- **Liquidation & Margin Math (`risk.rs`)**: Exact isolated/cross liquidation price, bps-to-liquidation buffer, and cross-market maintenance margin requirement (`portfolio_cross_mmr_usd`).

### Hyperliquid (`hyperliquid/`)
- **Native IOC Hedging**: EIP-712 signed reduce-only and hedging IOC orders paired with `src/live_hedge/` durable write-ahead journaling and clearinghouse position reconciliation.
- **HIP-3 Perp DEX Support**: Public capture and metadata resolution across native Hyperliquid, Entropy (`km`), and TradeXYZ (`xyz`).
