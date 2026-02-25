package config

import (
	"os"

	"github.com/pelletier/go-toml/v2"
)

type Config struct {
	Exchanges map[string]ExchangeConfig `toml:"exchanges"`
}

type ExchangeConfig struct {
	Enabled bool              `toml:"enabled"`
	Testnet bool              `toml:"testnet"`
	WSURL   string            `toml:"ws_url"`
	RESTURL string            `toml:"rest_url"`
	// Symbols maps standard local symbol (e.g. "BTC") to exchange-specific ID (e.g. "BTC_USDC_PERP")
	Symbols map[string]string `toml:"symbols"`
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
