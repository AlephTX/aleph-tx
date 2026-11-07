package exchanges

import (
	"strings"
	"testing"
)

func TestRESTPositionRequiresExplicitAccountAndMarketData(t *testing.T) {
	for _, tc := range []struct {
		body string
		size float64
		ok   bool
	}{
		{`{"code":200,"accounts":[{"index":42,"positions":[]}]}`, 0, true},
		{`{"code":200,"accounts":[{"index":42,"positions":[{"market_id":0,"symbol":"ETH","position":"0.25","sign":-1}]}]}`, -0.25, true},
		{`{"code":200,"accounts":[{"index":43,"positions":[]}]}`, 0, false},
		{`{"code":200,"accounts":[{"index":42,"positions":null}]}`, 0, false},
		{`{"code":200,"accounts":[{"index":42,"positions":[{"symbol":"ETH","position":"1","sign":1}]}]}`, 0, false},
		{`{"code":200,"accounts":[{"index":42,"positions":[{"market_id":0,"symbol":"BTC","position":"1","sign":1}]}]}`, 0, false},
	} {
		size, err := parseLighterPosition(strings.NewReader(tc.body), 42, 0, SymbolETHPERP)
		if (err == nil) != tc.ok || size != tc.size {
			t.Errorf("body=%s size=%v error=%v", tc.body, size, err)
		}
	}
}
