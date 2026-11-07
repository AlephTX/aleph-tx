package config

import "testing"

func TestValidateLighterPositionScope(t *testing.T) {
	c := Config{Lighter: ExchangeSection{FeederEnabled: true, FeederWSURL: "wss://example.test", FeederSymbols: map[string]string{"ETH": "0", "BTC": "1"}}}
	if err := c.Validate(); err == nil {
		t.Fatal("multiple markets would overwrite one position slot")
	}
	delete(c.Lighter.FeederSymbols, "BTC")
	if err := c.Validate(); err != nil {
		t.Fatal(err)
	}
	c.Lighter.FeederWSURL = ""
	if err := c.Validate(); err == nil {
		t.Fatal("missing WebSocket URL accepted")
	}
}

func TestSymbolTableDefaultsAndValidation(t *testing.T) {
	c := Config{}
	if got := c.SymbolTable(); got["BTC"] != 1001 || got["ETH"] != 1002 || len(got) != 2 {
		t.Fatalf("default symbols changed: %v", got)
	}
	c.Symbols = map[string]uint16{"SOL": 1003}
	if err := c.Validate(); err != nil {
		t.Fatal(err)
	}
	for _, bad := range []map[string]uint16{
		{"SOL": 0},
		{"SOL": MaxSymbolID},
		{"SOL": 1003, "DOGE": 1003},
	} {
		c.Symbols = bad
		if err := c.Validate(); err == nil {
			t.Fatalf("accepted invalid symbols %v", bad)
		}
	}
}

func TestEnabledFeederSymbolsMustBeRegistered(t *testing.T) {
	c := Config{
		Symbols:     map[string]uint16{"ETH": 1002},
		Hyperliquid: ExchangeSection{FeederEnabled: true, FeederWSURL: "wss://example.test", FeederSymbols: map[string]string{"SOL": "SOL"}},
	}
	if err := c.Validate(); err == nil {
		t.Fatal("unregistered ticker accepted")
	}
	c.Symbols["SOL"] = 1003
	if err := c.Validate(); err != nil {
		t.Fatal(err)
	}
}

func TestRepositoryConfigsLoad(t *testing.T) {
	for _, path := range []string{"../../config.example.toml", "../../deploy/oasis/feeder.toml"} {
		c, err := Load(path)
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		if got := c.SymbolTable(); got["ETH"] != 1002 || got["BTC"] != 1001 {
			t.Fatalf("%s: symbols = %v", path, got)
		}
	}
}
