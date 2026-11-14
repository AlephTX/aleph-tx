package main

import (
	"fmt"
	"testing"
)

func TestLighterPublicTradeExecutionMetadataExactAndRedacted(t *testing.T) {
	// Synthetic integers above float64's exact range exercise lossless capture.
	raw := []byte(`{"type":"update/trade","channel":"trade:0","account_index":41,"trades":[{"trade_id":9007199254740997,"market_id":0,"timestamp":1700000000000,"price":"100.00","size":"0.01","is_maker_ask":false,"transaction_time":9007199254740993,"block_height":18446744073709551615,"sequence_index":9007199254740995,"ask_account_id":42,"bid_account_id":43,"ask_id":11,"bid_id":12,"maker_fee":9,"taker_position_size_before":"10","ask_account_pnl":"2","tx_hash":"synthetic-private-field"}]}`)
	got, err := publicLighterTrades(raw)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"type":"update/trade","channel":"trade:0","trades":[{"trade_id":9007199254740997,"market_id":0,"timestamp":1700000000000,"price":"100.00","size":"0.01","is_maker_ask":false,"transaction_time":9007199254740993,"block_height":18446744073709551615,"sequence_index":9007199254740995}]}`
	if string(got) != want {
		t.Fatal("public clock fields changed precision, block timestamp changed, or private fields escaped")
	}
}

func TestLighterPublicTradeExecutionMetadataMissingNullAndZero(t *testing.T) {
	base := `{"trade_id":1,"market_id":0,"timestamp":1700000000000,"price":"100","size":"1","is_maker_ask":true`
	envelope := func(trade string) []byte {
		return []byte(`{"type":"update/trade","channel":"trade:0","trades":[` + trade + `}]}`)
	}
	for _, tc := range []struct {
		name, supplied, retained string
	}{
		{"missing", "", ""},
		{"null", `,"transaction_time":null,"block_height":null,"sequence_index":null`, ""},
		{"zero", `,"transaction_time":0,"block_height":0,"sequence_index":0`,
			`,"transaction_time":0,"block_height":0,"sequence_index":0`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			got, err := publicLighterTrades(envelope(base + tc.supplied))
			if err != nil {
				t.Fatal(err)
			}
			if string(got) != string(envelope(base+tc.retained)) {
				t.Fatal("missing metadata was inferred or an unavailable wire zero was changed")
			}
		})
	}
}

func TestLighterPublicTradeExecutionMetadataRejectsInvalidIntegers(t *testing.T) {
	for _, field := range []string{"transaction_time", "block_height", "sequence_index"} {
		for _, value := range []string{"-1", "1.5", "1e3", `"10"`, "true", "{}", "[]", "18446744073709551616"} {
			t.Run(field+"/"+value, func(t *testing.T) {
				raw := []byte(fmt.Sprintf(`{"type":"update/trade","channel":"trade:0","trades":[{"trade_id":1,"market_id":0,"timestamp":1700000000000,"price":"100","size":"1","is_maker_ask":true,%q:%s}]}`, field, value))
				if got, err := publicLighterTrades(raw); err == nil || got != nil {
					t.Fatal("invalid integer metadata was accepted or exported")
				}
			})
		}
	}
}
