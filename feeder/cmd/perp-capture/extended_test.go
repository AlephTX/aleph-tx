package main

import (
	"strings"
	"testing"
)

func TestExtendedRejectsIndicativeMarketAndBookGap(t *testing.T) {
	metadata := func(flag string) string {
		return `{"status":"OK","data":[{"name":"BTC-USD","active":true,"status":"ACTIVE","type":"PERPETUAL","isRfq":` + flag + `}]}`
	}
	if _, err := extendedMarkets(strings.NewReader(metadata("true")), []string{"BTC-USD"}); err == nil {
		t.Fatal("RFQ metadata accepted as a CLOB")
	}
	feeds, err := extendedMarkets(strings.NewReader(metadata("false")), []string{"BTC-USD"})
	if err != nil || len(feeds) != 2 || feeds[0].market != "BTC-USD" {
		t.Fatalf("CLOB discovery failed: %v", err)
	}
	if seq, err := extendedPublication([]byte(`{"ts":1700000000000,"type":"SNAPSHOT","seq":1,"data":{"m":"BTC-USD"}}`), feeds[0], 0); err != nil || seq != 1 {
		t.Fatalf("snapshot rejected: %v", err)
	}
	if _, err := extendedPublication([]byte(`{"ts":1700000000001,"type":"DELTA","seq":3,"data":{"m":"BTC-USD"}}`), feeds[0], 1); err == nil {
		t.Fatal("depth sequence gap accepted")
	}
}

func TestLighterPublicTradeCaptureKeepsOnlyReplayFields(t *testing.T) {
	raw := []byte(`{"type":"update/trade","channel":"trade:0","trades":[{"trade_id":123,"market_id":0,"timestamp":1700000000000,"price":"67500.0","size":"0.01","is_maker_ask":false,"ask_account_id":42,"ask_id":11,"maker_fee":9}]}`)
	filtered, err := publicLighterTrades(raw)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"type":"update/trade","channel":"trade:0","trades":[{"trade_id":123,"market_id":0,"timestamp":1700000000000,"price":"67500.0","size":"0.01","is_maker_ask":false}]}`
	if string(filtered) != want {
		t.Fatal("unexpected public trade archive fields or values")
	}
}
