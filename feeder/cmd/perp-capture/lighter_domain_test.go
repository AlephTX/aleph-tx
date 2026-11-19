package main

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
)

func TestRobinhoodDomainHasIndependentSources(t *testing.T) {
	sources, err := robinhoodSubscriptions("1,0", true, 4*time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	if len(sources) != 2 {
		t.Fatalf("sources=%d", len(sources))
	}
	if sources[0].source != "lighter_robinhood" || sources[0].url != "wss://api.rh.lighter.xyz/stream" || sources[0].clockRefresh != 4*time.Minute {
		t.Fatal("wrong independent domain")
	}
	if sources[1].source != "lighter_robinhood_trades" || len(sources[1].requests) != 2 {
		t.Fatal("missing public trade channels")
	}
	if _, err := robinhoodSubscriptions("1,1", false, 0); err == nil {
		t.Fatal("duplicate market accepted")
	}
	core, err := subscriptions("1", "", "", "", "", "", "", 0, 0, false)
	if err != nil || core[0].source != "lighter" || core[0].url != "wss://mainnet.zklighter.elliot.ai/stream" {
		t.Fatal("core compatibility changed")
	}
}

func TestStaggerLighterBookRefreshAfterReconnect(t *testing.T) {
	start := time.Now()
	const period = 4 * time.Minute
	sources := []subscription{
		{source: "lighter", clockRefresh: period},
		{source: "lighter_robinhood", clockRefresh: period},
		{source: "lighter_trades"},
		{source: "lighter_robinhood_trades"},
		{source: "hyperliquid"},
	}
	if got := (subscription{source: "lighter", firstClockRefresh: start}).nextClockRefresh(start.Add(time.Second)); !got.IsZero() {
		t.Fatalf("non-positive clockRefresh must not panic or return a deadline, got %v", got)
	}
	if got := sources[0].nextClockRefresh(start.Add(time.Minute)); !got.Equal(start.Add(5 * time.Minute)) {
		t.Fatal("default refresh must be relative to each connection")
	}
	staggerLighterClockRefresh(sources, start)
	for _, s := range sources[2:] {
		if !s.firstClockRefresh.IsZero() {
			t.Fatalf("scheduled refresh on %s", s.source)
		}
	}
	for _, tc := range []struct{ elapsed, core, rh time.Duration }{
		{0, 4 * time.Minute, 2 * time.Minute},
		{2*time.Minute - time.Nanosecond, 4 * time.Minute, 2 * time.Minute},
		{2 * time.Minute, 4 * time.Minute, 6 * time.Minute},
		{4*time.Minute + time.Nanosecond, 8 * time.Minute, 6 * time.Minute},
		{15 * time.Minute, 16 * time.Minute, 18 * time.Minute},
	} {
		for i, want := range []time.Duration{tc.core, tc.rh} {
			got := sources[i].nextClockRefresh(start.Add(tc.elapsed)).Sub(start)
			if got != want || got <= tc.elapsed || got-tc.elapsed > period {
				t.Fatalf("%s at %v: got %v, want %v", sources[i].source, tc.elapsed, got, want)
			}
		}
	}
}

func TestRunSourceUsesScheduledClockDeadline(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		if err := conn.Write(r.Context(), websocket.MessageText, []byte(`{"type":"subscribed/market_stats","channel":"market_stats:0"}`)); err == nil {
			conn.Read(r.Context())
		}
	}))
	defer server.Close()

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	source := subscription{source: "lighter_robinhood", url: "ws" + strings.TrimPrefix(server.URL, "http"),
		clockRefresh: time.Hour, firstClockRefresh: time.Now().Add(time.Second)}
	var received bool
	err := runSource(ctx, source, func(r record) error {
		received = r.Source == source.source && r.Event == "message"
		return nil
	})
	if !received || !errors.Is(err, errClockRefresh) || ctx.Err() != nil {
		t.Fatalf("received=%v scheduled refresh=%v parent error=%v", received, err, ctx.Err())
	}
}
