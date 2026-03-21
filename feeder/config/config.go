package config

import (
	"os"

	"github.com/pelletier/go-toml/v2"
)

// Config represents the unified configuration structure
// Feeder reads only the feeder-specific fields from each exchange section
type Config struct {
	Lighter     ExchangeSection `toml:"lighter"`
	EdgeX       ExchangeSection `toml:"edgex"`
	Backpack    ExchangeSection `toml:"backpack"`
	Hyperliquid ExchangeSection `toml:"hyperliquid"`
	Binance     ExchangeSection `toml:"binance"`
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

	return &c, nil
}

