package main

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestArcusTradeSubscriptionsRequireMarketsAndDeduplicate(t *testing.T) {
	if _, err := arcusTradeSubscriptions("", "", true); err == nil {
		t.Fatal("expected error when -arcus-trades has no Arcus markets")
	}
	subs, err := arcusTradeSubscriptions("NVDA-USD,SNDK-USD", "NVDA-USD,BTC-USD", true)
	if err != nil {
		t.Fatal(err)
	}
	if len(subs) != 1 || subs[0].market != "" || strings.Join(subs[0].coins, ",") != "NVDA-USD,SNDK-USD,BTC-USD" || len(subs[0].requests) != 3 {
		t.Fatalf("unexpected deduplicated multiplexed subscription: %+v", subs)
	}
}

func TestArcusPublicTradesRedactParticipantAddressesAndOrderIDs(t *testing.T) {
	source := subscription{source: "arcus_trades", market: "NVDA-USD", coins: []string{"NVDA-USD"}}
	acknowledged := make(map[string]bool)

	// Unacknowledged channel_data must fail closed.
	rawTrade := []byte(`{"type":"channel_data","channel":"trades","id":"NVDA-USD","contents":[{"marketDisplayName":"NVDA-USD","marketId":28,"side":"BUY","price":"160.26","size":"0.0464516","tradeId":"32780688","timestamp":1794728086203046,"makerOrderId":"synthetic-maker-order","takerOrderId":"synthetic-taker-order","makerAddress":"synthetic-maker-address","takerAddress":"synthetic-taker-address","sequenceNumber":11976995605}]}`)
	if _, err := arcusTradePublication(source, rawTrade, acknowledged); err == nil {
		t.Fatal("accepted Arcus trade batch before subscription acknowledgment")
	}

	// Greeting frame is ignored.
	greeting, err := arcusTradePublication(source, []byte(`{"type":"connected","connection_id":"123"}`), acknowledged)
	if err != nil || greeting.Event != "" {
		t.Fatalf("unexpected greeting handling: %+v, err=%v", greeting, err)
	}

	// Subscription acknowledgment marks market online.
	ack, err := arcusTradePublication(source, []byte(`{"type":"subscribed","channel":"trades","id":"NVDA-USD","contents":{}}`), acknowledged)
	if err != nil {
		t.Fatal(err)
	}
	if ack.Event != "subscribed" || ack.Market != "NVDA-USD" || string(ack.Payload) != `{"channel":"trades","id":"NVDA-USD"}` {
		t.Fatalf("unexpected ack record: %+v", ack)
	}

	// Live trade batch preserves microsecond timestamp, decimals, sequenceNumber, and strips addresses/order IDs.
	msg, err := arcusTradePublication(source, rawTrade, acknowledged)
	if err != nil {
		t.Fatal(err)
	}
	want := `{"channel":"trades","id":"NVDA-USD","contents":[{"marketDisplayName":"NVDA-USD","marketId":28,"side":"BUY","price":"160.26","size":"0.0464516","tradeId":"32780688","timestamp":1794728086203046,"sequenceNumber":11976995605}]}`
	if msg.Event != "message" || msg.Market != "NVDA-USD" || string(msg.Payload) != want {
		t.Fatalf("unexpected sanitized trade payload:\ngot:  %s\nwant: %s", string(msg.Payload), want)
	}
	if strings.Contains(string(msg.Payload), "Address") || strings.Contains(string(msg.Payload), "OrderId") {
		t.Fatal("participant addresses or order IDs leaked into public capture")
	}
}

func TestArcusTradeWebSocketEndToEndCaptureLifecycle(t *testing.T) {
	subs, err := arcusTradeSubscriptions("NVDA-USD", "", true)
	if err != nil || len(subs) != 1 {
		t.Fatalf("arcusTradeSubscriptions failed: %v", err)
	}
	source := subs[0]
	stop := errors.New("done")

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
		defer cancel()
		_, reqBytes, err := conn.Read(ctx)
		if err != nil {
			t.Error(err)
			return
		}
		var req arcusTradeSubscribe
		if json.Unmarshal(reqBytes, &req) != nil || req.Type != "subscribe" || req.Channel != "trades" || req.ID != "NVDA-USD" {
			t.Errorf("unexpected subscribe request: %s", string(reqBytes))
			return
		}
		frames := []string{
			`{"type":"connected","connection_id":"999"}`,
			`{"type":"subscribed","channel":"trades","id":"NVDA-USD","contents":{}}`,
			`{"type":"channel_data","channel":"trades","id":"NVDA-USD","contents":[{"marketDisplayName":"NVDA-USD","marketId":28,"side":"SELL","price":"160.10","size":"1.25","tradeId":"77","timestamp":1794728086999000}]}`,
		}
		for _, f := range frames {
			if err := conn.Write(ctx, websocket.MessageText, []byte(f)); err != nil {
				t.Error(err)
				return
			}
		}
	}))
	defer server.Close()

	source.url = "ws" + strings.TrimPrefix(server.URL, "http")
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()

	var events []string
	err = connect(ctx, source, func(r record) error {
		events = append(events, r.Event+":"+r.Market)
		if r.Event == "message" {
			return stop
		}
		return nil
	})
	if !errors.Is(err, stop) {
		t.Fatalf("expected stop after trade message, got %v (events=%v)", err, events)
	}
	wantEvents := []string{"connected:NVDA-USD", "subscribed:NVDA-USD", "message:NVDA-USD"}
	if strings.Join(events, ",") != strings.Join(wantEvents, ",") {
		t.Fatalf("unexpected lifecycle events: got %v, want %v", events, wantEvents)
	}
}
