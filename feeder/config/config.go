package config

import (
	"fmt"
	"os"

	"github.com/pelletier/go-toml/v2"
)

// Config represents the unified configuration structure
// Feeder reads only the feeder-specific fields from each exchange section
type Config struct {
	// Symbols maps tickers (BTC, ETH, ...) to global SHM symbol IDs. When
	// empty, DefaultSymbols applies. Rust reads the same table.
	Symbols     map[string]uint16 `toml:"symbols"`
	Lighter     ExchangeSection   `toml:"lighter"`
	EdgeX       ExchangeSection   `toml:"edgex"`
	Backpack    ExchangeSection   `toml:"backpack"`
	Hyperliquid ExchangeSection   `toml:"hyperliquid"`
	Binance     ExchangeSection   `toml:"binance"`
}

// ExchangeSection contains both feeder and strategy config
// Feeder only uses the feeder_* fields
type ExchangeSection struct {
	// Feeder settings
	FeederEnabled bool              `toml:"feeder_enabled"`
	FeederWSURL   string            `toml:"feeder_ws_url"`
	FeederRESTURL string            `toml:"feeder_rest_url"`
	FeederSymbols map[string]string `toml:"feeder_symbols"`

	// Strategy settings (ignored by feeder)
	ExchangeID uint16 `toml:"exchange_id"`
	// ... other fields ignored by feeder
}

// ExchangeConfig is the legacy format for backward compatibility
type ExchangeConfig struct {
	Enabled bool              `toml:"enabled"`
	Testnet bool              `toml:"testnet"`
	WSURL   string            `toml:"ws_url"`
	RESTURL string            `toml:"rest_url"`
	Symbols map[string]string `toml:"symbols"`
}

// ToExchangeMap converts the new unified config to the legacy map format
// This allows existing feeder code to work without changes
func (c *Config) ToExchangeMap() map[string]ExchangeConfig {
	return map[string]ExchangeConfig{
		"lighter": {
			Enabled: c.Lighter.FeederEnabled,
			WSURL:   c.Lighter.FeederWSURL,
			RESTURL: c.Lighter.FeederRESTURL,
			Symbols: c.Lighter.FeederSymbols,
		},
		"edgex": {
			Enabled: c.EdgeX.FeederEnabled,
			WSURL:   c.EdgeX.FeederWSURL,
			RESTURL: c.EdgeX.FeederRESTURL,
			Symbols: c.EdgeX.FeederSymbols,
		},
		"backpack": {
			Enabled: c.Backpack.FeederEnabled,
			WSURL:   c.Backpack.FeederWSURL,
			RESTURL: c.Backpack.FeederRESTURL,
			Symbols: c.Backpack.FeederSymbols,
		},
		"hyperliquid": {
			Enabled: c.Hyperliquid.FeederEnabled,
			WSURL:   c.Hyperliquid.FeederWSURL,
			RESTURL: c.Hyperliquid.FeederRESTURL,
			Symbols: c.Hyperliquid.FeederSymbols,
		},
		"binance": {
			Enabled: c.Binance.FeederEnabled,
			WSURL:   c.Binance.FeederWSURL,
			RESTURL: c.Binance.FeederRESTURL,
			Symbols: c.Binance.FeederSymbols,
		},
	}
}

func Load(path string) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}

	var c Config
	if err := toml.Unmarshal(b, &c); err != nil {
		return nil, err
	}

	if err := c.Validate(); err != nil {
		return nil, err
	}
	return &c, nil
}

// MaxSymbolID is the exclusive upper bound imposed by the SHM matrix layout.
const MaxSymbolID = 2048

// DefaultSymbols preserves the historical BTC/ETH IDs.
func DefaultSymbols() map[string]uint16 {
	return map[string]uint16{"BTC": 1001, "ETH": 1002}
}

// SymbolTable returns the configured symbols, or the defaults when unset.
func (c *Config) SymbolTable() map[string]uint16 {
	if len(c.Symbols) == 0 {
		return DefaultSymbols()
	}
	return c.Symbols
}

// Validate rejects configurations that cannot be represented by the current
// single-position account SHM ABI.
func (c *Config) Validate() error {
	seen := make(map[uint16]string, len(c.Symbols))
	for name, id := range c.Symbols {
		if name == "" || id == 0 || id >= MaxSymbolID {
			return fmt.Errorf("symbol %q: id %d must be in 1..%d", name, id, MaxSymbolID-1)
		}
		if other, dup := seen[id]; dup {
			return fmt.Errorf("symbols %q and %q share id %d", other, name, id)
		}
		seen[id] = name
	}
	symbols := c.SymbolTable()
	for name, exchange := range c.ToExchangeMap() {
		if !exchange.Enabled {
			continue
		}
		for ticker := range exchange.Symbols {
			if _, ok := symbols[ticker]; !ok {
				return fmt.Errorf("%s feeder maps %q, which is not in [symbols]", name, ticker)
			}
		}
	}
	if c.Lighter.FeederEnabled && len(c.Lighter.FeederSymbols) != 1 {
		return fmt.Errorf("lighter feeder requires exactly one market: account position SHM is single-market")
	}
	for name, exchange := range c.ToExchangeMap() {
		if exchange.Enabled && (exchange.WSURL == "" || len(exchange.Symbols) == 0) {
			return fmt.Errorf("%s feeder requires a WebSocket URL and symbol mapping", name)
		}
	}
	return nil
}
