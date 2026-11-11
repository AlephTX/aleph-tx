package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
)

func TestPublicRESTSnapshotsKeepRequestIdentityAndWireTime(t *testing.T) {
	stop := errors.New("received fixture")
	for _, tc := range []struct {
		source, endpoint, path, levels string
	}{
		{"arcus_snapshot", "/v1/l2OrderBook", "/v1/l2OrderBook/NVDA-USD", "nLevels"},
		{"bulk_snapshot", "/api/v1/l2book", "/api/v1/l2book", "nlevels"},
	} {
		t.Run(tc.source, func(t *testing.T) {
			const body = `{"timestamp":1794325133390293059}`
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				if r.Method != http.MethodGet || r.URL.Path != tc.path ||
					r.URL.Query().Get(tc.levels) != "20" {
					t.Errorf("unexpected request: %s %s", r.Method, r.URL)
				}
				if tc.source == "bulk_snapshot" &&
					(r.URL.Query().Get("type") != "l2book" || r.URL.Query().Get("coin") != "NVDA-USD") {
					t.Errorf("invalid BULK market query: %s", r.URL.RawQuery)
				}
				_, _ = io.WriteString(w, body)
			}))
			defer server.Close()
			source := subscription{
				source: tc.source, url: server.URL + tc.endpoint, interval: time.Second,
			}
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			defer cancel()
			err := pollSnapshotCoin(ctx, source, "NVDA-USD", server.Client(), func(r record) error {
				var payload snapshotPayload
				if err := json.Unmarshal(r.Payload, &payload); err != nil {
					t.Fatal(err)
				}
				if r.Source != tc.source || r.Event != "message" || r.ReceivedAt <= 0 ||
					payload.Market != "NVDA-USD" || string(payload.Data) != body {
					t.Errorf("snapshot did not preserve source, receipt time, or original payload")
				}
				return stop
			})
			if !errors.Is(err, stop) {
				t.Fatalf("snapshot was not delivered: %v", err)
			}
		})
	}
}

func TestPublicWebSocketSnapshotsArchiveOnlyRequestedFullBooks(t *testing.T) {
	sources, err := subscriptions("", "", "", "", "", "ETH-USD", "ETH-USD", 0, 0, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(sources) != 2 {
		t.Fatalf("expected two WebSocket sources, got %d", len(sources))
	}
	fixtures := map[string]struct{ request, control, publication string }{
		"arcus": {
			`{"type":"subscribe","channel":"l2Orderbook","id":"ETH-USD","nLevels":20}`,
			`{"type":"connected"}`,
			`{"type":"subscribed","channel":"l2Orderbook","id":"ETH-USD","contents":{"timestamp":1794325133390000,"bids":[["1999","1"]],"asks":[["2001","1"]]}}`,
		},
		"bulk": {
			`{"method":"subscribe","subscription":[{"type":"l2Snapshot","symbol":"ETH-USD","nlevels":20}]}`,
			`{"type":"subscriptionResponse","topics":["l2snapshot.ETH-USD.20"]}`,
			`{"type":"l2Snapshot","topic":"l2snapshot.ETH-USD.20","data":{"book":{"symbol":"ETH-USD","updateType":"snapshot","timestamp":1794325133390,"levels":[[{"px":1999,"sz":1}],[{"px":2001,"sz":1}]]}}}`,
		},
	}
	stop := errors.New("received fixture")
	for _, source := range sources {
		t.Run(source.source, func(t *testing.T) {
			fixture := fixtures[source.source]
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				conn, err := websocket.Accept(w, r, nil)
				if err != nil {
					t.Error(err)
					return
				}
				defer conn.CloseNow()
				ctx, cancel := context.WithTimeout(r.Context(), 2*time.Second)
				defer cancel()
				_, request, err := conn.Read(ctx)
				var got, want any
				if err != nil || json.Unmarshal(request, &got) != nil ||
					json.Unmarshal([]byte(fixture.request), &want) != nil || !reflect.DeepEqual(got, want) {
					t.Errorf("unexpected %s subscription", source.source)
					return
				}
				marketField := `"id"`
				if source.source == "bulk" {
					marketField = `"symbol"`
				}
				otherMarket := strings.ReplaceAll(fixture.publication,
					marketField+`:"ETH-USD"`, marketField+`:"BTC-USD"`)
				for _, message := range []string{fixture.control, otherMarket, fixture.publication} {
					if err := conn.Write(ctx, websocket.MessageText, []byte(message)); err != nil {
						t.Error(err)
						return
					}
				}
			}))
			defer server.Close()
			source.url = "ws" + strings.TrimPrefix(server.URL, "http")
			ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
			defer cancel()
			err := connect(ctx, source, func(row record) error {
				if row.Source != source.source || row.Event != "message" || row.ReceivedAt <= 0 ||
					row.RequestSentAt != 0 || string(row.Payload) != fixture.publication {
					t.Error("capture did not preserve the requested publication and receipt time")
				}
				return stop
			})
			if !errors.Is(err, stop) {
				t.Fatalf("requested publication not delivered: %v", err)
			}
		})
	}
}
