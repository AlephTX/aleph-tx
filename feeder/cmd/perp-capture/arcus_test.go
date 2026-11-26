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

func TestArcusBookSubscriptionsIncludeL2AndBBO(t *testing.T) {
	sources, err := subscriptions("", "", "", "", "", "BTC-USD,ETH-USD", "", 0, 0, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(sources) != 1 || sources[0].source != "arcus" {
		t.Fatalf("unexpected sources: %+v", sources)
	}
	var got []string
	for _, request := range sources[0].requests {
		b, err := json.Marshal(request)
		if err != nil {
			t.Fatal(err)
		}
		got = append(got, string(b))
	}
	want := []string{
		`{"channel":"l2Orderbook","id":"BTC-USD","nLevels":20,"type":"subscribe"}`,
		`{"channel":"bbo","id":"BTC-USD","type":"subscribe"}`,
		`{"channel":"l2Orderbook","id":"ETH-USD","nLevels":20,"type":"subscribe"}`,
		`{"channel":"bbo","id":"ETH-USD","type":"subscribe"}`,
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Fatalf("unexpected Arcus requests:\ngot:  %v\nwant: %v", got, want)
	}
}

func TestArcusWebSocketCapturesBBOAndL2Publications(t *testing.T) {
	sources, err := subscriptions("", "", "", "", "", "BTC-USD", "", 0, 0, false)
	if err != nil || len(sources) != 1 {
		t.Fatalf("subscriptions failed: %v", err)
	}
	source := sources[0]
	l2 := `{"type":"channel_data","channel":"l2Orderbook","id":"BTC-USD","contents":{"timestamp":1795691150000000,"globalSequenceId":16893834600,"bids":[["60415.0","1"]],"asks":[["60415.5","1"]]}}`
	bbo := `{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"60415.1","size":"0.08408347"},"bestAsk":{"price":"60415.2","size":"0.01058685"},"timestamp":1795691150080124,"lastSequenceId":980274961,"globalSequenceId":16893834613}}`
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
		defer cancel()
		var channels []string
		for range 2 {
			_, reqBytes, err := conn.Read(ctx)
			var req struct{ Type, Channel, ID string }
			if err != nil || json.Unmarshal(reqBytes, &req) != nil || req.Type != "subscribe" || req.ID != "BTC-USD" {
				t.Errorf("unexpected subscribe request: %s", reqBytes)
				return
			}
			channels = append(channels, req.Channel)
		}
		if strings.Join(channels, ",") != "l2Orderbook,bbo" {
			t.Errorf("unexpected Arcus channels: %v", channels)
			return
		}
		frames := []string{
			`{"type":"connected"}`,
			`{"type":"channel_data","channel":"bbo","id":"ETH-USD","contents":{}}`,
			`{"type":"channel_data","channel":"trades","id":"BTC-USD","contents":[]}`,
			l2,
			bbo,
		}
		for _, f := range frames {
			if err := conn.Write(ctx, websocket.MessageText, []byte(f)); err != nil {
				t.Error(err)
				return
			}
		}
		<-ctx.Done()
	}))
	defer server.Close()
	source.url = "ws" + strings.TrimPrefix(server.URL, "http")
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	stop := errors.New("done")
	var payloads []string
	err = connect(ctx, source, func(r record) error {
		if r.Source != "arcus" || r.Event != "message" || r.ReceivedAt <= 0 {
			t.Errorf("unexpected record: %+v", r)
		}
		payloads = append(payloads, string(r.Payload))
		if len(payloads) == 2 {
			return stop
		}
		return nil
	})
	if !errors.Is(err, stop) {
		t.Fatalf("expected stop after two publications, got %v (payloads=%v)", err, payloads)
	}
	if len(payloads) != 2 || payloads[0] != l2 || payloads[1] != bbo {
		t.Fatalf("raw Arcus publications not preserved:\n%v", payloads)
	}
}
