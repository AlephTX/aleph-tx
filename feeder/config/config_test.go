package config

import "testing"

func TestValidateLighterPositionScope(t *testing.T) {
	c := Config{Lighter: ExchangeSection{FeederEnabled: true, FeederWSURL: "wss://example.test", FeederSymbols: map[string]string{"ETH": "0", "BTC": "1"}}}
	if err := c.Validate(); err == nil {
		t.Fatal("multiple markets would overwrite one position slot")
	}
	if err := c.ValidateWithMode(true); err != nil {
		t.Fatalf("public-only mode should allow multi-market Lighter BBOs: %v", err)
	}
	delete(c.Lighter.FeederSymbols, "BTC")
	c.Lighter.FeederPublicSymbols = map[string]string{"BTC": "1"}
	if err := c.Validate(); err != nil {
		t.Fatalf("single private market + extra feeder_public_symbols rejected: %v", err)
	}
	c.Lighter.FeederWSURL = ""
	if err := c.Validate(); err == nil {
		t.Fatal("missing WebSocket URL accepted")
	}
}

func TestValidateMultiVenueDomainsAndHIP3Prefixes(t *testing.T) {
	c := Config{
		Symbols: map[string]uint16{"BTC": 1001, "ETH": 1002, "NVDA": 1003, "SNDK": 1004},
		LighterRobinhood: ExchangeSection{
			FeederEnabled: true,
			FeederWSURL:   "wss://api.rh.lighter.xyz/stream",
			FeederSymbols: map[string]string{"ETH": "0", "BTC": "1"},
		},
		Arcus: ExchangeSection{
			FeederEnabled: true,
			FeederWSURL:   "wss://api.arcus.xyz/v1/ws",
			FeederSymbols: map[string]string{"NVDA": "NVDA-USD", "ETH": "ETH-USD"},
		},
		Entropy: ExchangeSection{
			FeederEnabled: true,
			FeederWSURL:   "wss://api.hyperliquid.xyz/ws",
			FeederSymbols: map[string]string{"NVDA": "io:NVDA", "SNDK": "io:SNDK"},
		},
		TradeXYZ: ExchangeSection{
			FeederEnabled: true,
			FeederWSURL:   "wss://api.hyperliquid.xyz/ws",
			FeederSymbols: map[string]string{"NVDA": "xyz:NVDA", "SNDK": "xyz:SNDK"},
		},
	}
	if err := c.Validate(); err != nil {
		t.Fatalf("valid multi-venue config rejected: %v", err)
	}

	badEntropy := c
	badEntropy.Entropy.FeederSymbols = map[string]string{"NVDA": "xyz:NVDA"}
	if err := badEntropy.Validate(); err == nil {
		t.Fatal("entropy accepted non-io: prefix")
	}

	badTradeXYZ := c
	badTradeXYZ.TradeXYZ.FeederSymbols = map[string]string{"NVDA": "NVDA"}
	if err := badTradeXYZ.Validate(); err == nil {
		t.Fatal("tradexyz accepted unprefixed symbol")
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
	for _, path := range []string{
		"../../config.example.toml",
		"../../deploy/oasis/feeder.toml",
		"../../profiles/scenario1_quant_eth.example.toml",
		"../../profiles/scenario2_hedge_btc.example.toml",
		"../../profiles/scenario3_turnover_separate_l1.example.toml",
	} {
		c, err := Load(path)
		if err != nil {
			t.Fatalf("%s: %v", path, err)
		}
		if got := c.SymbolTable(); got["ETH"] != 1002 || got["BTC"] != 1001 {
			t.Fatalf("%s: symbols = %v", path, got)
		}
	}
}
