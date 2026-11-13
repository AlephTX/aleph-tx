package main

import (
	"context"
	"encoding/json"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
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
