// Package exchanges defines exchange IDs and symbol mappings.
package exchanges

// Exchange IDs — must match Rust arbitrage.rs constants.
const (
	ExchangeHyperliquid uint8 = 1
	ExchangeLighter     uint8 = 2
	ExchangeEdgeX       uint8 = 3
	Exchange01          uint8 = 4
	ExchangeBackpack    uint8 = 5
)

// Symbol IDs — global normalized IDs.
const (
	SymbolBTCPERP uint16 = 1001
	SymbolETHPERP uint16 = 1002
)

// CoinToSymbolID maps Hyperliquid coin names to our symbol IDs.
var CoinToSymbolID = map[string]uint16{
	"BTC": SymbolBTCPERP,
	"ETH": SymbolETHPERP,
}
