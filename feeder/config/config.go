package config

import (
	"fmt"
	"os"
	"strconv"
	"strings"

	"github.com/pelletier/go-toml/v2"
)

// Config represents the unified configuration structure
// Feeder reads only the feeder-specific fields from each exchange section
type Config struct {
	// Symbols maps tickers (BTC, ETH, NVDA, ...) to global SHM symbol IDs.
	// When empty, DefaultSymbols applies. Rust reads the same table.
	Symbols          map[string]uint16 `toml:"symbols"`
	Lighter          ExchangeSection   `toml:"lighter"`
	LighterRobinhood ExchangeSection   `toml:"lighter_robinhood"`
	EdgeX            ExchangeSection   `toml:"edgex"`
	Backpack         ExchangeSection   `toml:"backpack"`
	Hyperliquid      ExchangeSection   `toml:"hyperliquid"`
	Entropy          ExchangeSection   `toml:"entropy"`
	TradeXYZ         ExchangeSection   `toml:"tradexyz"`
	Arcus            ExchangeSection   `toml:"arcus"`
	Binance          ExchangeSection   `toml:"binance"`
}

// ExchangeSection contains both feeder and strategy config
// Feeder only uses the feeder_* fields
type ExchangeSection struct {
	// Feeder settings
	FeederEnabled       bool              `toml:"feeder_enabled"`
	FeederWSURL         string            `toml:"feeder_ws_url"`
	FeederRESTURL       string            `toml:"feeder_rest_url"`
	FeederSymbols       map[string]string `toml:"feeder_symbols"`
	FeederPublicSymbols map[string]string `toml:"feeder_public_symbols"`

	// Strategy settings (ignored by feeder)
	ExchangeID uint16 `toml:"exchange_id"`
	// ... other fields ignored by feeder
}

// ExchangeConfig is the normalized feeder configuration per venue domain.
type ExchangeConfig struct {
	Enabled       bool              `toml:"enabled"`
	Testnet       bool              `toml:"testnet"`
	WSURL         string            `toml:"ws_url"`
	RESTURL       string            `toml:"rest_url"`
	Symbols       map[string]string `toml:"symbols"`
	PublicSymbols map[string]string `toml:"public_symbols"`
}

// AllPublicSymbols merges primary Symbols with optional PublicSymbols for
// public orderbook subscription while keeping private streams bound to Symbols.
func (e ExchangeConfig) AllPublicSymbols() map[string]string {
	merged := make(map[string]string, len(e.Symbols)+len(e.PublicSymbols))
	for k, v := range e.PublicSymbols {
		merged[k] = v
	}
	for k, v := range e.Symbols {
		merged[k] = v
	}
	return merged
}

// ToExchangeMap converts the unified config to the active feeder map.
// Legacy [edgex] and [backpack] sections remain accepted in TOML for backward
// compatibility while their adapters reside in legacy/v3_archive.
func (c *Config) ToExchangeMap() map[string]ExchangeConfig {
	return map[string]ExchangeConfig{
		"lighter": {
			Enabled:       c.Lighter.FeederEnabled,
			WSURL:         c.Lighter.FeederWSURL,
			RESTURL:       c.Lighter.FeederRESTURL,
			Symbols:       c.Lighter.FeederSymbols,
			PublicSymbols: c.Lighter.FeederPublicSymbols,
		},
		"lighter_robinhood": {
			Enabled:       c.LighterRobinhood.FeederEnabled,
			WSURL:         c.LighterRobinhood.FeederWSURL,
			RESTURL:       c.LighterRobinhood.FeederRESTURL,
			Symbols:       c.LighterRobinhood.FeederSymbols,
			PublicSymbols: c.LighterRobinhood.FeederPublicSymbols,
		},
		"hyperliquid": {
			Enabled: c.Hyperliquid.FeederEnabled,
			WSURL:   c.Hyperliquid.FeederWSURL,
			RESTURL: c.Hyperliquid.FeederRESTURL,
			Symbols: c.Hyperliquid.FeederSymbols,
		},
		"entropy": {
			Enabled: c.Entropy.FeederEnabled,
			WSURL:   c.Entropy.FeederWSURL,
			RESTURL: c.Entropy.FeederRESTURL,
			Symbols: c.Entropy.FeederSymbols,
		},
		"tradexyz": {
			Enabled: c.TradeXYZ.FeederEnabled,
			WSURL:   c.TradeXYZ.FeederWSURL,
			RESTURL: c.TradeXYZ.FeederRESTURL,
			Symbols: c.TradeXYZ.FeederSymbols,
		},
		"arcus": {
			Enabled: c.Arcus.FeederEnabled,
			WSURL:   c.Arcus.FeederWSURL,
			RESTURL: c.Arcus.FeederRESTURL,
			Symbols: c.Arcus.FeederSymbols,
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
	return LoadWithMode(path, false)
}

func LoadWithMode(path string, publicOnly bool) (*Config, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}

	var c Config
	if err := toml.Unmarshal(b, &c); err != nil {
		return nil, err
	}

	if err := c.ValidateWithMode(publicOnly); err != nil {
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
// single-position account SHM ABI when private account tracking is active.
func (c *Config) Validate() error {
	return c.ValidateWithMode(false)
}

// ValidateWithMode validates symbol tables and per-venue mappings. When
// publicOnly is true, [lighter] is not bound to the single-market private
// account-stats SHM limit and may subscribe multiple public markets directly.
func (c *Config) ValidateWithMode(publicOnly bool) error {
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
	exchanges := c.ToExchangeMap()
	for name, exchange := range exchanges {
		if !exchange.Enabled {
			continue
		}
		allSyms := exchange.AllPublicSymbols()
		if exchange.WSURL == "" || len(allSyms) == 0 {
			return fmt.Errorf("%s feeder requires a WebSocket URL and symbol mapping", name)
		}
		for ticker, remote := range allSyms {
			if _, ok := symbols[ticker]; !ok {
				return fmt.Errorf("%s feeder maps %q, which is not in [symbols]", name, ticker)
			}
			if strings.TrimSpace(remote) == "" {
				return fmt.Errorf("%s feeder maps %q to an empty remote market", name, ticker)
			}
			switch name {
			case "lighter", "lighter_robinhood":
				idx, err := strconv.Atoi(remote)
				if err != nil || idx < 0 {
					return fmt.Errorf("%s feeder market for %q must be a non-negative integer, got %q", name, ticker, remote)
				}
			case "entropy":
				if !strings.HasPrefix(remote, "io:") || len(remote) <= 3 {
					return fmt.Errorf("entropy feeder market for %q must have io: prefix, got %q", ticker, remote)
				}
			case "tradexyz":
				if !strings.HasPrefix(remote, "xyz:") || len(remote) <= 4 {
					return fmt.Errorf("tradexyz feeder market for %q must have xyz: prefix, got %q", ticker, remote)
				}
			case "hyperliquid":
				if strings.Contains(remote, ":") && !strings.HasPrefix(remote, "io:") && !strings.HasPrefix(remote, "xyz:") {
					return fmt.Errorf("hyperliquid feeder market %q has unsupported HIP-3 prefix", remote)
				}
			}
		}
		for ticker, primary := range exchange.Symbols {
			if pub, dup := exchange.PublicSymbols[ticker]; dup && pub != primary {
				return fmt.Errorf("%s feeder maps %q to conflicting markets %q and %q", name, ticker, primary, pub)
			}
		}
	}
	if c.Lighter.FeederEnabled && !publicOnly && len(c.Lighter.FeederSymbols) != 1 {
		return fmt.Errorf("lighter feeder requires exactly one market: account position SHM is single-market")
	}
	return nil
}
