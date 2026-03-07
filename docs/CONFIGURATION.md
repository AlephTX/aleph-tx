# Configuration Guide

AlephTX uses a unified configuration system with clear separation between sensitive credentials and trading parameters.

## Configuration Files

### 1. `config.toml` - Unified Configuration (Root Directory)

Single configuration file that controls **both** feeder (market data) and strategy (trading) for all exchanges.

**Structure**: Organized by exchange, each section contains:
- `feeder_*` fields: Control market data ingestion (Go feeder)
- Other fields: Control trading strategy (Rust)

**Example**:
```toml
[edgex]
# Feeder settings (Go)
feeder_enabled = true
feeder_ws_url = "wss://quote.edgex.exchange/api/v1/public/ws"
feeder_symbols = { ETH = "10000002", BTC = "10000001" }

# Strategy settings (Rust)
exchange_id = 3
risk_fraction = 0.10
min_spread_bps = 8.0
contract_id = 1
price_decimals = 2
size_decimals = 4
```

### 2. `.env.*` Files - Sensitive Credentials (Root Directory)

**Only** store private keys and API credentials. Never commit these files.

**Files**:
- `.env.lighter` - Lighter DEX credentials
- `.env.edgex` - EdgeX credentials
- `.env.backpack` - Backpack credentials

**Example** (`.env.edgex`):
```bash
EDGEX_ACCOUNT_ID=573736952784748604
EDGEX_STARK_PRIVATE_KEY=023421824d933e7e9ed0159ec5902b183eee87fd1ea2dd32807a2d69e247ef57
```

## Exchange Configuration

### Lighter DEX

```toml
[lighter]
# Feeder
feeder_enabled = true
feeder_ws_url = "wss://mainnet.zklighter.elliot.ai/stream"
feeder_symbols = { ETH = "0" }

# Strategy
exchange_id = 2
symbol_id = 1002
market_id = 0
base_order_size = 0.05
max_position = 0.1
risk_fraction = 0.20
min_spread_bps = 6.0
```

**Credentials** (`.env.lighter`):
```bash
API_KEY_PRIVATE_KEY=<your_ed25519_private_key>
LIGHTER_ACCOUNT_INDEX=<your_account_index>
LIGHTER_API_KEY_INDEX=<your_api_key_index>
```

### EdgeX

```toml
[edgex]
# Feeder
feeder_enabled = true
feeder_ws_url = "wss://quote.edgex.exchange/api/v1/public/ws"
feeder_symbols = { ETH = "10000002", BTC = "10000001" }

# Strategy
exchange_id = 3
contract_id = 1
synthetic_asset_id = "0x4554482d3130000000000000000000"
collateral_asset_id = "0x555344432d36000000000000000000"
price_decimals = 2
size_decimals = 4
fee_rate = 0.0005
risk_fraction = 0.10
min_spread_bps = 8.0
```

**Credentials** (`.env.edgex`):
```bash
EDGEX_ACCOUNT_ID=<your_account_id>
EDGEX_STARK_PRIVATE_KEY=<your_stark_private_key>
```

### Backpack

```toml
[backpack]
# Feeder
feeder_enabled = false
feeder_ws_url = "wss://ws.backpack.exchange"
feeder_symbols = { ETH = "ETH_USDC_PERP", BTC = "BTC_USDC_PERP" }

# Strategy
exchange_id = 4
risk_fraction = 0.20
min_spread_bps = 6.0
```

**Credentials** (`.env.backpack`):
```bash
BACKPACK_PUBLIC_KEY=<your_base64_public_key>
BACKPACK_SECRET_KEY=<your_base64_secret_key>
```

## Configuration Loading

### Feeder (Go)

The feeder reads `config.toml` from the root directory:

```bash
# Default: reads config.toml from current directory
./feeder/feeder-app config.toml

# Or specify path via environment variable
ALEPH_FEEDER_CONFIG=custom.toml ./feeder/feeder-app
```

The feeder only uses `feeder_*` fields and ignores strategy-specific fields.

### Strategy (Rust)

Rust strategies read `config.toml` from the root directory:

```rust
let config = AppConfig::load_default(); // Reads config.toml
let edgex_config = config.edgex;        // Access exchange section
```

Credentials are loaded from `.env.*` files via environment variables.

## Switching Exchanges

To switch from one exchange to another:

1. **Update `config.toml`**: Set `feeder_enabled = true` for target exchange, `false` for others
2. **Ensure `.env.*` file exists**: Create/update credentials file for target exchange
3. **Use appropriate Makefile target**:
   ```bash
   make lighter-up    # Lighter DEX
   make edgex-up      # EdgeX
   make backpack-up   # Backpack
   ```

## Native Libraries

### Lighter DEX Only

Lighter requires a native Ed25519 signing library located at:
```
src/native/lighter-signer-linux-amd64.so
```

The Makefile automatically sets `LD_LIBRARY_PATH` for Lighter strategies:
```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

### Other Exchanges

- **EdgeX**: Pure Rust `starknet-crypto` crate (no native library)
- **Backpack**: Pure Rust crypto (no native library)
- **Hyperliquid**: Pure Rust crypto (no native library)

## Migration from Old Config

If you have old configuration files:

**Old structure**:
```
feeder/config.toml          # Feeder config
feeder/config.edgex.toml    # EdgeX feeder config
config.toml                 # Strategy config
lib/lighter-signer-*.so     # Native library
```

**New structure**:
```
config.toml                 # Unified config (feeder + strategy)
.env.lighter                # Lighter credentials
.env.edgex                  # EdgeX credentials
src/native/*.so             # Native libraries (Lighter only)
```

## Best Practices

1. **Never commit `.env.*` files** - Add to `.gitignore`
2. **Use unified `config.toml`** - Single source of truth for all non-sensitive config
3. **Enable one exchange at a time** - Set `feeder_enabled = true` for only one exchange
4. **Test configuration changes** - Use `make <exchange>-down && make <exchange>-up` to restart
5. **Keep credentials separate** - Never put private keys in `config.toml`

## Troubleshooting

### Feeder can't find config.toml
```
Error: failed to load config config.toml: no such file or directory
```
**Solution**: Ensure you're running from the root directory, or specify full path:
```bash
./feeder/feeder-app config.toml
```

### Strategy can't find native library (Lighter only)
```
Error: error while loading shared libraries: liblighter-signer-linux-amd64.so
```
**Solution**: Ensure `LD_LIBRARY_PATH` is set (Makefile handles this automatically):
```bash
export LD_LIBRARY_PATH=$(pwd)/src/native:$LD_LIBRARY_PATH
```

### Wrong exchange enabled in feeder
```
Log: "lighter: enabled=true" but you want EdgeX
```
**Solution**: Update `config.toml`:
```toml
[lighter]
feeder_enabled = false  # Disable Lighter

[edgex]
feeder_enabled = true   # Enable EdgeX
```
