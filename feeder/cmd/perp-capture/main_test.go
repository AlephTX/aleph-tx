package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

func decodeCaptureRows(t *testing.T, path string) []record {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	dec := json.NewDecoder(f)
	var rows []record
	for dec.More() {
		var row record
		if err := dec.Decode(&row); err != nil {
			t.Fatal(err)
		}
		rows = append(rows, row)
	}
	return rows
}

func startMockLighterServer(
	t *testing.T,
	onPrimary func(conn *websocket.Conn),
	onProbe func(ctx context.Context, conn *websocket.Conn, channel string, probeAttempt int64),
) (*httptest.Server, *atomic.Int64, *atomic.Int64) {
	t.Helper()
	var primaryConns atomic.Int64
	var probeConns atomic.Int64
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()

		_, firstRaw, err := conn.Read(r.Context())
		if err != nil {
			return
		}
		var firstSub lighterSubscription
		if err := json.Unmarshal(firstRaw, &firstSub); err != nil {
			t.Errorf("decode subscribe: %v", err)
			return
		}
		if strings.HasPrefix(firstSub.Channel, "order_book/") {
			primaryConns.Add(1)
			// Read the matching market_stats subscription sent by connect().
			if _, _, err := conn.Read(r.Context()); err != nil {
				return
			}
			mkt := strings.TrimPrefix(firstSub.Channel, "order_book/")
			now := time.Now().UnixMilli()
			if err := wsjson.Write(r.Context(), conn, map[string]any{
				"type":      "subscribed/order_book",
				"channel":   "order_book:" + mkt,
				"timestamp": now,
				"order_book": map[string]any{
					"nonce":           10,
					"begin_nonce":     0,
					"last_updated_at": now * 1000,
					"bids":            []map[string]string{{"price": "2000.0", "size": "1.0"}},
					"asks":            []map[string]string{{"price": "2000.1", "size": "1.0"}},
				},
			}); err != nil {
				return
			}
			if err := wsjson.Write(r.Context(), conn, map[string]any{
				"type":      "subscribed/market_stats",
				"channel":   "market_stats:" + mkt,
				"timestamp": now,
				"market_stats": map[string]any{
					"market_id":   0,
					"symbol":      "ETH",
					"index_price": "2000.05",
				},
			}); err != nil {
				return
			}
			if onPrimary != nil {
				onPrimary(conn)
			}
			for {
				if _, _, err := conn.Read(r.Context()); err != nil {
					return
				}
			}
		}

		attempt := probeConns.Add(1)
		if onProbe != nil {
			onProbe(r.Context(), conn, firstSub.Channel, attempt)
		}
		for {
			if _, _, err := conn.Read(r.Context()); err != nil {
				return
			}
		}
	}))
	return srv, &primaryConns, &probeConns
}

func TestLighterClockProbeFlagValidationAndSourceBuilding(t *testing.T) {
	if err := validateLighterClockOptions("0", "", 0, false, true); err == nil {
		t.Fatal("expected error for -lighter-clock-probe without -lighter-clock-refresh")
	}
	if err := validateLighterClockOptions("", "", 4*time.Minute, false, true); err == nil {
		t.Fatal("expected error for -lighter-clock-probe without Lighter markets")
	}
	if err := validateLighterClockOptions("0", "", 4*time.Minute, false, true); err != nil {
		t.Fatalf("unexpected error for core clock probe: %v", err)
	}
	if err := validateLighterClockOptions("", "1", 4*time.Minute, false, true); err != nil {
		t.Fatalf("unexpected error for robinhood clock probe: %v", err)
	}
	if err := validateLighterClockOptions("0", "1", 4*time.Minute, true, true); err != nil {
		t.Fatalf("unexpected error for staggered core+rh clock probe: %v", err)
	}

	// Legacy mode remains untouched when -lighter-clock-probe is false.
	legacy, err := subscriptions("0,1", "", "", "", "", "", "", 0, 4*time.Minute, false)
	if err != nil {
		t.Fatal(err)
	}
	rhLegacy, err := robinhoodSubscriptions("2,3", false, 4*time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	legacy = append(legacy, rhLegacy...)
	if len(legacy) != 2 || legacy[0].clockRefresh != 4*time.Minute || legacy[1].clockRefresh != 4*time.Minute {
		t.Fatalf("unexpected legacy subscriptions: %+v", legacy)
	}

	// Probe mode clears primary clockRefresh and appends one market_stats/<first_id> probe per domain.
	withProbes, err := enableLighterClockProbes(legacy, 4*time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	if len(withProbes) != 4 {
		t.Fatalf("expected 4 subscriptions (2 primary + 2 probes), got %d", len(withProbes))
	}
	start := time.Now()
	staggerLighterClockRefresh(withProbes, start)
	for i := range 2 {
		if withProbes[i].clockProbe || withProbes[i].clockRefresh != 0 || !withProbes[i].firstClockRefresh.IsZero() {
			t.Fatalf("primary subscription %d should not disconnect on clock refresh: %+v", i, withProbes[i])
		}
	}
	coreProbe := withProbes[2]
	rhProbe := withProbes[3]
	if !coreProbe.clockProbe || coreProbe.source != "lighter" || coreProbe.clockRefresh != 4*time.Minute {
		t.Fatalf("unexpected core probe: %+v", coreProbe)
	}
	if !coreProbe.firstClockRefresh.Equal(start.Add(4 * time.Minute)) {
		t.Fatalf("unexpected core staggered firstClockRefresh: %v", coreProbe.firstClockRefresh)
	}
	if sub := coreProbe.requests[0].(lighterSubscription); sub.Channel != "market_stats/0" {
		t.Fatalf("expected first core market_stats/0, got %+v", sub)
	}
	if !rhProbe.clockProbe || rhProbe.source != "lighter_robinhood" || rhProbe.clockRefresh != 4*time.Minute {
		t.Fatalf("unexpected rh probe: %+v", rhProbe)
	}
	if !rhProbe.firstClockRefresh.Equal(start.Add(2 * time.Minute)) {
		t.Fatalf("unexpected rh staggered firstClockRefresh: %v", rhProbe.firstClockRefresh)
	}
	if sub := rhProbe.requests[0].(lighterSubscription); sub.Channel != "market_stats/2" {
		t.Fatalf("expected first rh market_stats/2, got %+v", sub)
	}
}

func TestLighterClockProbeEmitsWithoutDisconnectingPrimaryStream(t *testing.T) {
	srv, primaryConns, probeConns := startMockLighterServer(
		t,
		nil,
		func(ctx context.Context, conn *websocket.Conn, channel string, attempt int64) {
			if attempt == 1 {
				if err := wsjson.Write(ctx, conn, map[string]string{"type": "ping"}); err != nil {
					return
				}
				if _, _, err := conn.Read(ctx); err != nil {
					return
				}
			}
			mkt := strings.TrimPrefix(channel, "market_stats/")
			_ = wsjson.Write(ctx, conn, map[string]any{
				"type":      "subscribed/market_stats",
				"channel":   "market_stats:" + mkt,
				"timestamp": time.Now().UnixMilli(),
				"market_stats": map[string]any{
					"market_id":   0,
					"symbol":      "ETH",
					"index_price": "2000.05",
				},
			})
		},
	)
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	sources, err := subscriptions("0,1", "", "", "", "", "", "", 0, 4*time.Minute, false)
	if err != nil {
		t.Fatal(err)
	}
	sources[0].url = wsURL
	sources, err = enableLighterClockProbes(sources, 80*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}

	outPath := filepath.Join(t.TempDir(), "probe.ndjson")
	out, err := openCaptureOutput(outPath, false)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 320*time.Millisecond)
	defer cancel()
	if err := captureWithTiming(ctx, sources, out, true); err != nil {
		t.Fatalf("captureWithTiming failed: %v", err)
	}
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}

	if got := primaryConns.Load(); got != 1 {
		t.Fatalf("expected exactly 1 primary connection (no reconnects), got %d", got)
	}
	if got := probeConns.Load(); got < 2 {
		t.Fatalf("expected >= 2 probe connections, got %d", got)
	}

	rows := decodeCaptureRows(t, outPath)
	var probes, primaryMsgs int
	var prevSeq uint64
	for _, row := range rows {
		if row.Timing == nil {
			t.Fatalf("missing capture_timing on row %+v", row)
		}
		if row.Timing.Sequence != prevSeq+1 {
			t.Fatalf("sequence gap: got %d, want %d", row.Timing.Sequence, prevSeq+1)
		}
		prevSeq = row.Timing.Sequence
		switch row.Event {
		case "clock_refresh", "disconnect":
			t.Fatalf("unexpected %s event in clock-probe mode: %+v", row.Event, row)
		case "clock_probe":
			probes++
			if row.Source != "lighter" || row.RequestSentAt <= 0 || row.ReceivedAt < row.RequestSentAt || row.AvailableAt < row.ReceivedAt {
				t.Fatalf("invalid clock_probe timing/source: %+v", row)
			}
			var payload map[string]any
			if err := json.Unmarshal(row.Payload, &payload); err != nil {
				t.Fatalf("decode clock_probe payload: %v", err)
			}
			if payload["type"] != "subscribed/market_stats" || payload["channel"] != "market_stats:0" {
				t.Fatalf("unexpected clock_probe payload: %v", payload)
			}
		case "message":
			primaryMsgs++
		}
	}
	if probes < 2 || primaryMsgs < 2 {
		t.Fatalf("expected >=2 probes and >=2 primary messages, got probes=%d primaryMsgs=%d", probes, primaryMsgs)
	}
}

func TestLighterClockProbeRecoversGracefullyFromProbeFailure(t *testing.T) {
	srv, primaryConns, probeConns := startMockLighterServer(
		t,
		nil,
		func(ctx context.Context, conn *websocket.Conn, channel string, attempt int64) {
			if attempt == 1 {
				_ = wsjson.Write(ctx, conn, map[string]string{"type": "error"})
				return
			}
			mkt := strings.TrimPrefix(channel, "market_stats/")
			_ = wsjson.Write(ctx, conn, map[string]any{
				"type":      "subscribed/market_stats",
				"channel":   "market_stats:" + mkt,
				"timestamp": time.Now().UnixMilli(),
			})
		},
	)
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	sources, err := subscriptions("0", "", "", "", "", "", "", 0, 4*time.Minute, false)
	if err != nil {
		t.Fatal(err)
	}
	sources[0].url = wsURL
	sources, err = enableLighterClockProbes(sources, 80*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}

	outPath := filepath.Join(t.TempDir(), "probe_recover.ndjson")
	out, err := openCaptureOutput(outPath, false)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 320*time.Millisecond)
	defer cancel()
	if err := captureWithTiming(ctx, sources, out, false); err != nil {
		t.Fatalf("captureWithTiming failed: %v", err)
	}
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}

	if got := primaryConns.Load(); got != 1 {
		t.Fatalf("primary stream disrupted by probe failure: primaryConns=%d", got)
	}
	if got := probeConns.Load(); got < 2 {
		t.Fatalf("expected probe retry after first failure, got %d attempts", got)
	}

	var probes int
	for _, row := range decodeCaptureRows(t, outPath) {
		if row.Event == "disconnect" || row.Event == "clock_refresh" {
			t.Fatalf("unexpected %s event after probe failure: %+v", row.Event, row)
		}
		if row.Event == "clock_probe" {
			probes++
			if row.RequestSentAt <= 0 {
				t.Fatalf("missing RequestSentAt on recovered clock_probe: %+v", row)
			}
		}
	}
	if probes < 1 {
		t.Fatalf("expected >=1 successful clock_probe after retry, got %d", probes)
	}
}

func TestLighterClockProbeStaggeredCoreAndRobinhood(t *testing.T) {
	srv, primaryConns, _ := startMockLighterServer(
		t,
		nil,
		func(ctx context.Context, conn *websocket.Conn, channel string, _ int64) {
			mkt := strings.TrimPrefix(channel, "market_stats/")
			_ = wsjson.Write(ctx, conn, map[string]any{
				"type":      "subscribed/market_stats",
				"channel":   "market_stats:" + mkt,
				"timestamp": time.Now().UnixMilli(),
			})
		},
	)
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	sources, err := subscriptions("0", "", "", "", "", "", "", 0, 4*time.Minute, false)
	if err != nil {
		t.Fatal(err)
	}
	rhSources, err := robinhoodSubscriptions("1", false, 4*time.Minute)
	if err != nil {
		t.Fatal(err)
	}
	sources = append(sources, rhSources...)
	for i := range sources {
		sources[i].url = wsURL
	}
	const period = 240 * time.Millisecond
	sources, err = enableLighterClockProbes(sources, period)
	if err != nil {
		t.Fatal(err)
	}
	staggerLighterClockRefresh(sources, time.Now())

	outPath := filepath.Join(t.TempDir(), "staggered.ndjson")
	out, err := openCaptureOutput(outPath, false)
	if err != nil {
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 400*time.Millisecond)
	defer cancel()
	if err := captureWithTiming(ctx, sources, out, false); err != nil {
		t.Fatalf("captureWithTiming failed: %v", err)
	}
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}

	if got := primaryConns.Load(); got != 2 {
		t.Fatalf("expected 2 continuous primary connections (core + rh), got %d", got)
	}

	var firstCoreAt, firstRHAt int64
	for _, row := range decodeCaptureRows(t, outPath) {
		if row.Event == "disconnect" || row.Event == "clock_refresh" {
			t.Fatalf("unexpected %s event: %+v", row.Event, row)
		}
		if row.Event != "clock_probe" {
			continue
		}
		switch row.Source {
		case "lighter":
			if firstCoreAt == 0 {
				firstCoreAt = row.ReceivedAt
			}
			if !strings.Contains(string(row.Payload), `"market_stats:0"`) {
				t.Fatalf("core probe subscribed to wrong channel: %s", string(row.Payload))
			}
		case "lighter_robinhood":
			if firstRHAt == 0 {
				firstRHAt = row.ReceivedAt
			}
			if !strings.Contains(string(row.Payload), `"market_stats:1"`) {
				t.Fatalf("rh probe subscribed to wrong channel: %s", string(row.Payload))
			}
		default:
			t.Fatalf("unexpected clock_probe source: %s", row.Source)
		}
	}
	if firstCoreAt == 0 || firstRHAt == 0 {
		t.Fatalf("missing clock_probe for core=%d or rh=%d", firstCoreAt, firstRHAt)
	}
	if firstRHAt > firstCoreAt {
		t.Fatalf("expected staggered robinhood probe (%d) before core probe (%d)", firstRHAt, firstCoreAt)
	}
}
