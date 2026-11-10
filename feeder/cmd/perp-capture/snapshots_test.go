package main

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
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
