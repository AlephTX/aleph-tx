package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestHyperliquidTradeReconnectRequiresAcknowledgmentAndRedacts(t *testing.T) {
	const ack = `{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"trades","coin":"io:NVDA"}}}`
	const trade = `{"channel":"trades","data":[{"coin":"io:NVDA","side":"A","px":"201.10","sz":"0.015","time":1800000000000,"tid":1125899906842623,"users":["fixture-a","fixture-b"],"hash":"fixture-c"}]}`
	sources, err := hyperliquidExtraSources("io:NVDA", true, 0)
	if err != nil {
		t.Fatal(err)
	}
	source := sources[0]
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), time.Second)
		defer cancel()
		if _, _, err := conn.Read(ctx); err != nil {
			t.Error(err)
			return
		}
		for _, message := range []string{ack, trade} {
			if err := conn.Write(ctx, websocket.MessageText, []byte(message)); err != nil {
				t.Error(err)
				return
			}
		}
	}))
	defer server.Close()
	source.url = "ws" + strings.TrimPrefix(server.URL, "http")
	for range 2 {
		var events []string
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		err := connect(ctx, source, func(row record) error {
			events = append(events, row.Event)
			if row.Event == "message" && (strings.Contains(string(row.Payload), "fixture-") ||
				!strings.Contains(string(row.Payload), `"tid":1125899906842623`) || row.Market != "io:NVDA") {
				t.Error("trade identity or public field filtering failed")
			}
			return nil
		})
		cancel()
		if err == nil || strings.Join(events, ",") != "connected,subscribed,message" {
			t.Fatalf("expected acknowledged connection, publication and close: %v %v", events, err)
		}
	}
	if _, err := hyperliquidPublication(source, "trades", []byte(trade), map[string]bool{}); err == nil {
		t.Fatal("accepted trades before the new connection's acknowledgment")
	}
}

func TestHyperliquidFundingCapturePreservesQueryAndPublicationTimes(t *testing.T) {
	const body = `[{"coin":"ETH","fundingRate":"-0.00001","premium":"0.00002","time":1800000000076}]`
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var q hyperliquidFundingQuery
		if r.Method != http.MethodPost || json.NewDecoder(r.Body).Decode(&q) != nil ||
			q.Type != "fundingHistory" || q.Coin != "ETH" || q.EndTime-q.StartTime != fundingLookbackMS {
			t.Error("incorrect funding query")
		}
		_, _ = io.WriteString(w, body)
	}))
	defer server.Close()
	source := subscription{source: "hyperliquid_funding", market: "ETH", url: server.URL}
	const sent = 1800000001000
	request, err := snapshotRequest(context.Background(), source, "ETH", sent)
	if err != nil {
		t.Fatal(err)
	}
	response, err := server.Client().Do(request)
	if err != nil {
		t.Fatal(err)
	}
	defer response.Body.Close()
	raw, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatal(err)
	}
	err = emitHyperliquidFunding(source, "ETH", sent, sent+123, raw, func(row record) error {
		if row.RequestSentAt != sent || row.ReceivedAt != sent+123 || row.Market != "ETH" ||
			!strings.Contains(string(row.Payload), body) {
			t.Error("funding response lost its request bracket or exact values")
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

func TestHyperliquidBookSubscriptionsIncludeBBOAndFastFlag(t *testing.T) {
	sources, err := subscriptions("", "BTC,xyz:NVDA", "", "", "", "", "", 0, 0, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(sources) != 1 || sources[0].source != "hyperliquid" {
		t.Fatalf("expected 1 hyperliquid source, got %+v", sources)
	}
	if len(sources[0].requests) != 6 {
		t.Fatalf("expected 6 requests (3 channels x 2 coins), got %d", len(sources[0].requests))
	}
	want := []map[string]any{
		{"method": "subscribe", "subscription": map[string]any{"type": "l2Book", "coin": "BTC", "fast": true}},
		{"method": "subscribe", "subscription": map[string]any{"type": "bbo", "coin": "BTC"}},
		{"method": "subscribe", "subscription": map[string]any{"type": "activeAssetCtx", "coin": "BTC"}},
		{"method": "subscribe", "subscription": map[string]any{"type": "l2Book", "coin": "xyz:NVDA", "fast": true}},
		{"method": "subscribe", "subscription": map[string]any{"type": "bbo", "coin": "xyz:NVDA"}},
		{"method": "subscribe", "subscription": map[string]any{"type": "activeAssetCtx", "coin": "xyz:NVDA"}},
	}
	for i, req := range sources[0].requests {
		raw, _ := json.Marshal(req)
		wantRaw, _ := json.Marshal(want[i])
		var gotObj, wantObj any
		_ = json.Unmarshal(raw, &gotObj)
		_ = json.Unmarshal(wantRaw, &wantObj)
		if !reflect.DeepEqual(gotObj, wantObj) {
			t.Fatalf("request %d mismatch: got %s want %s", i, raw, wantRaw)
		}
	}
}

func TestHyperliquidBBOPublicationAndEndToEndCapture(t *testing.T) {
	const ackBBO = `{"channel":"subscriptionResponse","data":{"method":"subscribe","subscription":{"type":"bbo","coin":"xyz:NVDA"}}}`
	const msgBBO = `{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1800000000500,"bbo":[{"px":"150.08","sz":"261.5","n":1},{"px":"150.09","sz":"415.9","n":12}]}}`
	const badBBO = `{"channel":"bbo","data":{"coin":"xyz:OTHER","time":1800000000500,"bbo":[{"px":"10","sz":"1","n":1},{"px":"11","sz":"1","n":1}]}}`

	sources, err := subscriptions("", "xyz:NVDA", "", "", "", "", "", 0, 0, false)
	if err != nil {
		t.Fatal(err)
	}
	source := sources[0]
	acked := map[string]bool{}
	row, err := hyperliquidPublication(source, "subscriptionResponse", []byte(ackBBO), acked)
	if err != nil || row.Source != "hyperliquid" || row.Event != "subscribed" || row.Market != "xyz:NVDA" {
		t.Fatalf("unexpected bbo ack result: row=%+v err=%v", row, err)
	}
	row, err = hyperliquidPublication(source, "bbo", []byte(msgBBO), acked)
	if err != nil || row.Source != "hyperliquid" || row.Event != "message" || row.Market != "xyz:NVDA" || !strings.Contains(string(row.Payload), `"channel":"bbo"`) {
		t.Fatalf("unexpected bbo message result: row=%+v err=%v", row, err)
	}
	if _, err := hyperliquidPublication(source, "bbo", []byte(badBBO), acked); err == nil {
		t.Fatal("expected error for unsubscribed coin on bbo channel")
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Error(err)
			return
		}
		defer conn.CloseNow()
		ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
		defer cancel()
		for range 3 {
			if _, _, err := conn.Read(ctx); err != nil {
				t.Error(err)
				return
			}
		}
		const msgL2 = `{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1800000000600,"levels":[[{"px":"150.07","sz":"10"}],[{"px":"150.10","sz":"10"}]]}}`
		for _, m := range []string{ackBBO, msgBBO, msgL2} {
			if err := conn.Write(ctx, websocket.MessageText, []byte(m)); err != nil {
				t.Error(err)
				return
			}
		}
	}))
	defer server.Close()
	source.url = "ws" + strings.TrimPrefix(server.URL, "http")
	var events []string
	var payloads []string
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	_ = connect(ctx, source, func(r record) error {
		events = append(events, r.Event)
		if r.Event == "message" {
			payloads = append(payloads, string(r.Payload))
		}
		return nil
	})
	if len(payloads) != 2 || !strings.Contains(payloads[0], `"channel":"bbo"`) || !strings.Contains(payloads[1], `"channel":"l2Book"`) {
		t.Fatalf("expected bbo and l2Book messages, events=%v payloads=%v", events, payloads)
	}
}
