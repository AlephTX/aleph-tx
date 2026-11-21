package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
)

const backpackTestMarket = "ANT.US_USDC_PERP"

func backpackDepth(first, last int) string {
	return `{"data":{"E":1700000000000100,"T":1700000000000050,"U":` + itoa(first) + `,"a":[["84.85","1.00"]],"b":[],"e":"depth","s":"ANT.US_USDC_PERP","u":` + itoa(last) + `},"stream":"depth.ANT.US_USDC_PERP"}`
}

func itoa(v int) string { b, _ := json.Marshal(v); return string(b) }

func TestBackpackSubscriptionsValidateSymbols(t *testing.T) {
	feeds, err := backpackSubscriptions("ANT.US_USDC_PERP,BTC_USDC_PERP")
	if err != nil || len(feeds) != 2 || feeds[0].market != backpackTestMarket || feeds[0].source != "backpack" {
		t.Fatalf("subscriptions: %v", err)
	}
	want := `{"method":"SUBSCRIBE","params":["depth.ANT.US_USDC_PERP","trade.ANT.US_USDC_PERP","markPrice.ANT.US_USDC_PERP"]}`
	if got, _ := json.Marshal(feeds[0].requests[0]); string(got) != want {
		t.Fatalf("subscribe request %s", got)
	}
	for _, bad := range []string{"BTC_USDC", "btc_USDC_PERP", "BTC USDC_PERP", "ANT.US_USDC_PERP,ANT.US_USDC_PERP"} {
		if _, err := backpackSubscriptions(bad); err == nil {
			t.Fatalf("accepted %q", bad)
		}
	}
}

func TestBackpackDepthContinuityAndIdentity(t *testing.T) {
	source := subscription{source: "backpack", market: backpackTestMarket}
	raw, seq, err := backpackPublication([]byte(backpackDepth(10, 12)), source, 0)
	if err != nil || seq != 12 || string(raw) != backpackDepth(10, 12) {
		t.Fatalf("first delta: %v", err)
	}
	if _, seq, err = backpackPublication([]byte(backpackDepth(13, 13)), source, seq); err != nil || seq != 13 {
		t.Fatalf("contiguous delta: %v", err)
	}
	if _, _, err := backpackPublication([]byte(backpackDepth(15, 15)), source, 13); err == nil {
		t.Fatal("depth gap accepted")
	}
	other := strings.ReplaceAll(backpackDepth(14, 14), backpackTestMarket, "BTC_USDC_PERP")
	if _, _, err := backpackPublication([]byte(other), source, 13); err == nil {
		t.Fatal("foreign market accepted")
	}
	if _, _, err := backpackPublication([]byte(`{"id":null,"error":{"code":4014,"message":"Invalid symbol"}}`), source, 0); err == nil {
		t.Fatal("subscription error accepted")
	}
	noTimes := strings.Replace(backpackDepth(14, 14), `"E":1700000000000100,`, "", 1)
	if _, _, err := backpackPublication([]byte(noTimes), source, 13); err == nil {
		t.Fatal("missing event time accepted")
	}
}

func TestBackpackTradeDropsOrderIDsAndKeepsExactDecimals(t *testing.T) {
	source := subscription{source: "backpack", market: backpackTestMarket}
	raw := `{"data":{"E":1700000000000100,"T":1700000000000000,"a":"111","b":"222","e":"trade","m":true,"p":"84.4300","q":"21.06","s":"ANT.US_USDC_PERP","t":17957},"stream":"trade.ANT.US_USDC_PERP"}`
	got, seq, err := backpackPublication([]byte(raw), source, 7)
	if err != nil || seq != 7 {
		t.Fatalf("trade: %v", err)
	}
	want := `{"stream":"trade.ANT.US_USDC_PERP","data":{"E":1700000000000100,"T":1700000000000000,"e":"trade","m":true,"p":"84.4300","q":"21.06","s":"ANT.US_USDC_PERP","t":17957}}`
	if string(got) != want {
		t.Fatalf("trade archive %s", got)
	}
}

func TestBackpackConnectArchivesSnapshotBeforeDeltasAndStopsOnGap(t *testing.T) {
	snapshot := `{"asks":[["84.85","29.45"]],"bids":[["84.80","3.00"]],"lastUpdateId":"11","timestamp":1700000000000000}`
	rest := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Query().Get("symbol") != backpackTestMarket {
			http.Error(w, "bad symbol", http.StatusBadRequest)
			return
		}
		_, _ = w.Write([]byte(snapshot))
	}))
	defer rest.Close()
	ws := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		if _, _, err := conn.Read(r.Context()); err != nil {
			t.Error(err)
			return
		}
		for _, message := range []string{backpackDepth(10, 12), backpackDepth(13, 13), backpackDepth(20, 20)} {
			if err := conn.Write(r.Context(), websocket.MessageText, []byte(message)); err != nil {
				return
			}
		}
		_, _, _ = conn.Read(r.Context())
	}))
	defer ws.Close()
	feeds, err := backpackSubscriptions(backpackTestMarket)
	if err != nil {
		t.Fatal(err)
	}
	source := feeds[0]
	source.url = "ws" + strings.TrimPrefix(ws.URL, "http")
	source.snapshotURL = rest.URL
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	var rows []record
	err = connect(ctx, source, func(row record) error { rows = append(rows, row); return nil })
	if err == nil || !strings.Contains(err.Error(), "sequence gap") {
		t.Fatalf("expected gap error, got %v", err)
	}
	if len(rows) != 3 || rows[0].Source != "backpack_snapshot" || rows[1].Source != "backpack" || rows[2].Source != "backpack" {
		t.Fatalf("unexpected archive order: %d rows", len(rows))
	}
	var payload snapshotPayload
	if json.Unmarshal(rows[0].Payload, &payload) != nil || payload.Market != backpackTestMarket || string(payload.Data) != snapshot ||
		rows[0].RequestSentAt == 0 || rows[0].ReceivedAt < rows[0].RequestSentAt {
		t.Fatal("snapshot lost identity, exact body or request bracket")
	}
	if string(rows[1].Payload) != backpackDepth(10, 12) || rows[1].Market != backpackTestMarket {
		t.Fatal("delta lost exact wire values or identity")
	}
}
