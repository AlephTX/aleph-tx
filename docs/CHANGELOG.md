# Refactor History

> Historical record of major architectural changes. Not auto-loaded.

## v3.3.0 - Unified Multi-Exchange Makefile (2025.01.XX)

**Objective**: Standardize Makefile commands across all exchanges with consistent `make <exchange>-up STRATEGY=<name>` pattern.

**Changes**:
- Unified command format: `lighter-up`, `backpack-up`, `edgex-up`
- Strategy selection via `STRATEGY=` parameter (default: `inventory_neutral_mm`)
- Per-exchange feeder + strategy PID tracking
- Graceful shutdown with 10-15s timeout before force kill
- Unified `make status` showing all exchanges

**Migration**:
```bash
# Old (v3.2.0)
make live-up              # Lighter inventory_neutral_mm
make adaptive-up          # Lighter adaptive_mm
make backpack-up          # Backpack (hardcoded strategy)

# New (v3.3.0)
make lighter-up                          # Default: inventory_neutral_mm
make lighter-up STRATEGY=adaptive_mm     # Adaptive MM
make backpack-up STRATEGY=simple_mm      # Backpack with strategy selection
```

## v3.2.0 - Exchange Decoupling Refactor (2025.01.XX)

**Objective**: Modularize exchange-specific code into `src/exchanges/` with config-driven hot-swappable architecture.

**Phase 1: Directory Restructure**
- Created `src/exchanges/{lighter,backpack,edgex}/` modules
- Moved `lighter_ffi.rs` → `exchanges/lighter/ffi.rs`
- Moved `lighter_trading.rs` → `exchanges/lighter/trading.rs`
- Deleted legacy `lighter_orders.rs` (unused)
- Added re-exports in `src/lib.rs` for backward compatibility

**Phase 2: Exchange Trait Implementation**
- `BackpackGateway` - Full Exchange trait implementation
- `EdgeXGateway` - Stub implementation (requires StarkNet L2 signing)
- Both wrap existing REST clients

**Phase 3: Examples & Documentation**
- Created `examples/backpack_mm.rs` demonstrating BackpackGateway usage
- Updated all CLAUDE.md files to reflect new structure
- Added `@CLAUDECODE/tasks/exchange-decoupling-refactor/ARCHITECTURE_ANALYSIS.md`

**Commit**: `751c621 refactor(exchanges): Modularize exchange integrations`
- 28 files changed, 1009 insertions(+), 622 deletions(-)
- Net reduction: 192 lines
