package main

import (
	"compress/gzip"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

func TestRollingCaptureRotatesWithoutDroppingConnectionOrSequence(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		for i := 1; i <= 12; i++ {
			nowMs := time.Now().UnixMilli()
			err := wsjson.Write(r.Context(), c, map[string]any{
				"e":  "bookTicker",
				"u":  1000 + i,
				"s":  "ETHUSDT",
				"b":  "2450.10",
				"B":  "10.0",
				"a":  "2450.20",
				"A":  "12.0",
				"T":  nowMs,
				"E":  nowMs,
				"st": 1,
			})
			if err != nil {
				return
			}
			time.Sleep(25 * time.Millisecond)
		}
		<-r.Context().Done()
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	sources := []subscription{{
		source: "binance",
		market: "ETHUSDT",
		url:    wsURL,
	}}

	tmp := t.TempDir()
	basePath := filepath.Join(tmp, "flywheel.ndjson.gz")
	rolling, err := openRollingCaptureOutput(basePath, true)
	if err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 280*time.Millisecond)
	defer cancel()
	if err := captureWithRotation(ctx, sources, nil, rolling, true, 90*time.Millisecond); err != nil {
		t.Fatalf("captureWithRotation failed: %v", err)
	}

	if len(rolling.sealedPaths) < 2 {
		t.Fatalf("expected >= 2 rotated segments, got %v", rolling.sealedPaths)
	}
	matches, _ := filepath.Glob(filepath.Join(tmp, "*.partial"))
	if len(matches) != 0 {
		t.Fatalf("leftover .partial files after clean completion: %v", matches)
	}

	var runID string
	var prevSeq uint64
	var msgCount int
	var sawTerminalEnd bool
	for idx, segPath := range rolling.sealedPaths {
		f, err := os.Open(segPath)
		if err != nil {
			t.Fatal(err)
		}
		gz, err := gzip.NewReader(f)
		if err != nil {
			f.Close()
			t.Fatal(err)
		}
		dec := json.NewDecoder(gz)
		firstInSeg := true
		for dec.More() {
			var row record
			if err := dec.Decode(&row); err != nil {
				t.Fatalf("decode %s: %v", segPath, err)
			}
			if row.Timing == nil {
				t.Fatalf("missing capture_timing in %s", segPath)
			}
			if runID == "" {
				runID = row.Timing.RunID
			} else if row.Timing.RunID != runID {
				t.Fatalf("run_id changed across rotation: %q != %q", row.Timing.RunID, runID)
			}
			if row.Timing.Sequence != prevSeq+1 {
				t.Fatalf("sequence gap at %s: got %d, want %d", segPath, row.Timing.Sequence, prevSeq+1)
			}
			prevSeq = row.Timing.Sequence
			if firstInSeg {
				if row.Source != "collector" || row.Event != "segment_start" {
					t.Fatalf("segment %d did not start with collector/segment_start: %+v", idx, row)
				}
				firstInSeg = false
			}
			if row.Event == "disconnect" {
				t.Fatalf("unexpected disconnect during in-process rotation: %+v", row)
			}
			if row.Source == "binance" && row.Event == "message" {
				msgCount++
			}
			if row.Source == "collector" && row.Event == "end" {
				sawTerminalEnd = true
			}
		}
		_ = gz.Close()
		_ = f.Close()
	}
	if msgCount < 5 || !sawTerminalEnd {
		t.Fatalf("unexpected message count=%d sawTerminalEnd=%v", msgCount, sawTerminalEnd)
	}
}
