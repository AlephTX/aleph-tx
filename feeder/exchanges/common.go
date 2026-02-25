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


// SymbolNameToID maps standard local ticker names to our global symbol IDs.
var SymbolNameToID = map[string]uint16{
	"BTC": SymbolBTCPERP,
	"ETH": SymbolETHPERP,
}

// BuildReverseSymbolMap creates a map from the exchange's specific symbol string directly to our internal global symbol ID.
func BuildReverseSymbolMap(symbols map[string]string) map[string]uint16 {
	m := make(map[string]uint16)
	for localSym, exchSym := range symbols {
		if id, ok := SymbolNameToID[localSym]; ok {
			m[exchSym] = id
		}
	}
	return m
}
