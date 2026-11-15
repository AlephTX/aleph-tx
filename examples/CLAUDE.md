---
description: Offline replay fixtures, capture compile profiles, and SHM inspection examples
alwaysApply: true
---

# examples/

> Synthetic replay fixtures (`*.json`), multi-venue book compilation profiles, and local SHM inspection utilities. Historical EdgeX StarkEx Pedersen debug examples are archived under `../legacy/v3_archive/examples/`.

## Key Files

| File | Description |
|------|-------------|
| `hedge_replay.json` | Deterministic lost-ack and partial-fill recovery fixture for `cross_venue hedge-replay` |
| `crypto_funding_paper.json` | Two-leg crypto funding carry fixture for `cross_venue perp-replay` |
| `equity_basis_paper.json` | Equity perpetual session-aware basis reversion fixture for `cross_venue perp-replay` |
| `eth_core_rh_books.json` | Independent-domain Lighter Core (USDC) + Robinhood (USDG) book compilation profile |
| `*_compile.json` | Venue pair compilation templates (Entropy, Arcus, TradeXYZ, BULK, Hyperliquid) |
| `test_account_stats.rs` | Read-only `/dev/shm/aleph-account-stats` diagnostic example |

## Offline Verification

```bash
target/release/cross_venue hedge-replay examples/hedge_replay.json
target/release/cross_venue perp-replay examples/crypto_funding_paper.json
target/release/cross_venue perp-replay examples/equity_basis_paper.json
```
