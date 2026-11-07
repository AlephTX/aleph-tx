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
		snapshot, err := decodeLighterAccount(strings.NewReader(tc.body), 42)
		var size float64
		if err == nil {
			size, err = snapshot.signedPosition(0, SymbolETHPERP)
		}
		if (err == nil) != tc.ok || size != tc.size {
			t.Errorf("body=%s size=%v error=%v", tc.body, size, err)
		}
	}
}

func TestRESTEquityUsesAssetValueAndRequiresAvailableBalance(t *testing.T) {
	snapshot, err := decodeLighterAccount(strings.NewReader(`{"code":200,"accounts":[{"index":42,"collateral":"99","total_asset_value":"100","available_balance":"75","positions":[{"market_id":4095,"symbol":"ETH","position":"0.05","sign":-1,"position_value":"-125"}]}]}`), 42)
	if err != nil {
		t.Fatal(err)
	}
	stats, err := snapshot.equityStats()
	if err != nil || stats != (lighterEquityStats{99, 100, 75, 1.25, 0.25}) {
		t.Fatalf("unexpected equity %+v: %v", stats, err)
	}
	snapshot.AvailableBalance = ""
	if _, err := snapshot.equityStats(); err == nil {
		t.Fatal("accepted missing available balance")
	}
}
