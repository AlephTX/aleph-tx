# Configuration Refactoring Summary

## Overview

Successfully refactored AlephTX configuration system to be simpler, more unified, and better organized.

## Changes Made

### 1. Unified Configuration System

**Before**:
- `config.toml` (Rust strategy config)
- `feeder/config.toml` (Go feeder config for Lighter)
- `feeder/config.edgex.toml` (Go feeder config for EdgeX)
- Separate configs for each exchange

**After**:
- Single `config.toml` at root
- Organized by exchange with `feeder_*` prefix for feeder settings
- Both Go feeder and Rust strategy read from same file
- Easy exchange switching (change one `feeder_enabled` flag)

### 2. Native Library Reorganization

**Before**:
- `lib/lighter-signer-linux-amd64.so` (top-level directory)
- `lib/CLAUDE.md`

**After**:
- `src/native/lighter-signer-linux-amd64.so` (Rust-specific location)
- `src/native/CLAUDE.md` (documents per-exchange requirements)
- Updated `build.rs` linker search path
- Updated Makefile `LD_LIBRARY_PATH`

**Rationale**: The .so library is only used by Rust (via FFI), so it belongs in `src/` not at top-level.

### 3. Credential Management

**Unchanged** (already correct):
- `.env.lighter` - Lighter DEX credentials
- `.env.edgex` - EdgeX credentials
- `.env.backpack` - Backpack credentials

Only private keys and API keys in `.env.*` files, never in `config.toml`.

### 4. Documentation

**New**:
- `docs/CONFIGURATION.md` - Comprehensive configuration guide
  - Exchange-specific setup
  - Configuration loading details
  - Migration guide from old structure
  - Troubleshooting section

**Updated**:
- `README.md` - Updated Quick Start and Project Structure
- `src/native/CLAUDE.md` - Documents native library requirements

## Configuration Format

### Example: EdgeX Section

```toml
[edgex]
# Feeder settings (Go)
feeder_enabled = true
feeder_ws_url = "wss://quote.edgex.exchange/api/v1/public/ws"
feeder_symbols = { ETH = "10000002", BTC = "10000001" }

# Strategy settings (Rust)
exchange_id = 3
contract_id = 1
price_decimals = 2
size_decimals = 4
risk_fraction = 0.10
min_spread_bps = 8.0
```

### Credentials (`.env.edgex`)

```bash
EDGEX_ACCOUNT_ID=573736952784748604
EDGEX_STARK_PRIVATE_KEY=023421824d933e7e9ed0159ec5902b183eee87fd1ea2dd32807a2d69e247ef57
```

## Code Changes

### Go Feeder (`feeder/config/config.go`)

- Added `ExchangeSection` struct with `feeder_*` fields
- Added `ToExchangeMap()` method for backward compatibility
- Reads unified config and converts to legacy format internally

### Go Main (`feeder/main.go`)

- Updated to use `ToExchangeMap()` for backward compatibility
- Runs from root directory: `./feeder/feeder-app config.toml`

### Rust Build (`build.rs`)

```rust
// Before
let lib_path = std::path::Path::new(&manifest_dir).join("lib");

// After
let native_path = std::path::Path::new(&manifest_dir).join("src/native");
```

### Makefile

```makefile
# Before
export LD_LIBRARY_PATH=$(pwd)/lib:$LD_LIBRARY_PATH

# After
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

## Benefits

1. **Single Source of Truth**: One `config.toml` for all exchanges
2. **Clear Separation**: `config.toml` (parameters) vs `.env.*` (secrets)
3. **Easier Exchange Switching**: Change one flag instead of multiple files
4. **Better Organization**: Native libs in `src/`, not top-level
5. **Consistent Structure**: All exchanges follow same pattern
6. **Comprehensive Documentation**: Clear guide for setup and troubleshooting

## Testing

Verified with EdgeX integration:
- ✅ Feeder reads unified config correctly
- ✅ Strategy reads unified config correctly
- ✅ Native library loading works (Lighter)
- ✅ EdgeX works without native library (pure Rust)
- ✅ BBO data flows correctly from feeder to strategy
- ✅ Market data updates every 5 seconds

## Migration Path

For existing deployments:

1. **Backup old configs**:
   ```bash
   cp config.toml config.toml.backup
   cp feeder/config.toml feeder/config.toml.backup
   ```

2. **Use new unified config**:
   - Copy settings from old configs to new `config.toml`
   - Set `feeder_enabled = true` for your exchange
   - Keep `.env.*` files unchanged

3. **Update commands**:
   ```bash
   # Old
   cd feeder && ./feeder-app
   
   # New
   ./feeder/feeder-app config.toml
   ```

4. **Verify**:
   ```bash
   make <exchange>-down
   make <exchange>-up
   tail -f logs/feeder-*.log logs/*-mm.log
   ```

## Files Changed

- `config.toml` - Unified configuration
- `build.rs` - Updated library search path
- `Makefile` - Updated LD_LIBRARY_PATH and feeder invocation
- `README.md` - Updated documentation
- `feeder/config/config.go` - New unified config loader
- `feeder/main.go` - Use unified config
- `docs/CONFIGURATION.md` - New comprehensive guide
- `src/native/CLAUDE.md` - New native library documentation

## Files Removed

- `feeder/config.toml` - Merged into root `config.toml`
- `feeder/config.edgex.toml` - Merged into root `config.toml`
- `lib/` directory - Moved to `src/native/`

## Commits

1. `fix(edgex): Fix feeder WebSocket parsing and add independent config` (167248f)
2. `refactor: Unify configuration system and reorganize native libraries` (07dfb48)
