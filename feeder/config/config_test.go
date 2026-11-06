package config

import "testing"

func TestValidateLighterPositionScope(t *testing.T) {
	c := Config{Lighter: ExchangeSection{FeederEnabled: true, FeederWSURL: "wss://example.test", FeederSymbols: map[string]string{"ETH-PERP": "0", "BTC-PERP": "1"}}}
	if err := c.Validate(); err == nil {
		t.Fatal("multiple markets would overwrite one position slot")
	}
	delete(c.Lighter.FeederSymbols, "BTC-PERP")
	if err := c.Validate(); err != nil {
		t.Fatal(err)
	}
	c.Lighter.FeederWSURL = ""
	if err := c.Validate(); err == nil {
		t.Fatal("missing WebSocket URL accepted")
	}
}
