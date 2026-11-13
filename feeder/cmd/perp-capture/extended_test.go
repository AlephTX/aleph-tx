package main

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
)

func TestExtendedRejectsIndicativeMarketAndBookGap(t *testing.T) {
	metadata := func(flag string) string {
		return `{"status":"OK","data":[{"name":"BTC-USD","active":true,"status":"ACTIVE","type":"PERPETUAL","isRfq":` + flag + `}]}`
	}
	if _, err := extendedMarkets(strings.NewReader(metadata("true")), []string{"BTC-USD"}, false); err == nil {
		t.Fatal("RFQ metadata accepted as a CLOB")
	}
	feeds, err := extendedMarkets(strings.NewReader(metadata("false")), []string{"BTC-USD"}, false)
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

func TestExtendedIndexSourceIsOptIn(t *testing.T) {
	const metadata = `{"status":"OK","data":[{"name":"BTC-USD","active":true,"status":"ACTIVE","type":"PERPETUAL","isRfq":false},{"name":"ETH-USD","active":true,"status":"ACTIVE","type":"PERPETUAL","isRfq":false}]}`
	feeds, err := extendedMarkets(strings.NewReader(metadata), []string{"BTC-USD", "ETH-USD"}, true)
	if err != nil || len(feeds) != 6 {
		t.Fatalf("CLOB and index discovery failed: %v", err)
	}
	for i, market := range []string{"BTC-USD", "ETH-USD"} {
		want := subscription{source: "extended_index", market: market,
			url: "wss://api.starknet.extended.exchange/stream.extended.exchange/v1/prices/index/" + market}
		got := feeds[3*i+2]
		if got.source != want.source || got.market != want.market || got.url != want.url || len(got.requests) != 0 {
			t.Fatalf("incorrect index source for %s", market)
		}
	}
}

func TestExtendedIndexCapturePreservesExactPublicationAndRejectsSequenceGap(t *testing.T) {
	const first = `{"type":"IP","data":{"m":"BTC-USD","p":"65206.200000001","ts":1700000000000},"ts":1700000000001,"seq":9,"sourceEventId":null}`
	const gap = `{"type":"IP","data":{"m":"BTC-USD","p":"65206.3","ts":1700000000010},"ts":1700000000011,"seq":11,"sourceEventId":null}`
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		for _, message := range []string{first, gap} {
			if err := conn.Write(r.Context(), websocket.MessageText, []byte(message)); err != nil {
				t.Error(err)
				return
			}
		}
		_, _, _ = conn.Read(r.Context())
	}))
	defer server.Close()
	source := subscription{source: "extended_index", market: "BTC-USD", url: "ws" + strings.TrimPrefix(server.URL, "http")}
	for range 2 {
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		var messages []record
		err := connect(ctx, source, func(row record) error {
			messages = append(messages, row)
			return nil
		})
		cancel()
		if err == nil || err.Error() != "Extended sequence discontinuity" || len(messages) != 1 {
			t.Fatalf("expected one publication before sequence rejection, got %d with %v", len(messages), err)
		}
		row := messages[0]
		if row.Source != source.source || row.Market != source.market || row.Event != "message" ||
			row.ReceivedAt == 0 || string(row.Payload) != first {
			t.Fatal("index publication lost its identity, receipt time or exact wire values")
		}
	}
	for _, changed := range [][2]string{{`"BTC-USD"`, `"ETH-USD"`}, {`"IP"`, `"MP"`},
		{`"65206.200000001"`, `"NaN"`}, {`"65206.200000001"`, `"0"`}, {`1700000000000`, `1700000000002`}} {
		if _, err := extendedPublication([]byte(strings.Replace(first, changed[0], changed[1], 1)), source, 0); err == nil {
			t.Fatal("invalid index publication accepted")
		}
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
