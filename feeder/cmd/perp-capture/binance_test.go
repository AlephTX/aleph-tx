package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"nhooyr.io/websocket"
	"strings"
	"testing"
	"time"
)

const syntheticBBO = `{"e":"bookTicker","s":"ETHUSDT","u":9007199254740993,"E":1800000000002,"T":1800000000001,"b":"99.99","a":"100.01","B":"2","A":"3","st":1,"unexpected":"synthetic"}`

func TestBinanceIdentityExactFieldsAndMissingTimes(t *testing.T) {
	sources, err := binanceSubscriptions("ETHUSDT,BTCUSDT")
	if err != nil || len(sources) != 2 || sources[0].url != "wss://fstream.binance.com/public/ws/ethusdt@bookTicker" {
		t.Fatal("invalid subscription")
	}
	for _, invalid := range []string{"ethusdt", "BTCUSDT/BTCUSDT", "BTCUSD", "ETHUSDT,ETHUSDT"} {
		if _, err := binanceSubscriptions(invalid); err == nil {
			t.Fatal("invalid symbol accepted")
		}
	}
	raw, err := publicBinanceBBO([]byte(syntheticBBO), "ETHUSDT")
	if err != nil || !bytes.Contains(raw, []byte(`9007199254740993`)) || bytes.Contains(raw, []byte("unexpected")) {
		t.Fatal("lost exact public identity or retained extra fields")
	}
	var fields map[string]json.RawMessage
	json.Unmarshal(raw, &fields)
	delete(fields, "T")
	delete(fields, "E")
	delete(fields, "u")
	raw, _ = json.Marshal(fields)
	clean, err := publicBinanceBBO(raw, "ETHUSDT")
	if err != nil || bytes.Contains(clean, []byte(`"T"`)) {
		t.Fatal("invented source timestamp")
	}
	for _, invalid := range []string{strings.Replace(syntheticBBO, `"st":1`, `"st":2`, 1), strings.Replace(syntheticBBO, `"99.99"`, `"101"`, 1), strings.Replace(syntheticBBO, `"ETHUSDT"`, `"BTCUSDT"`, 1)} {
		if _, err := publicBinanceBBO([]byte(invalid), "ETHUSDT"); err == nil {
			t.Fatal("invalid BBO accepted")
		}
	}
}

func TestPreciseWebSocketCaptureAndFooter(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer conn.CloseNow()
		if err := conn.Write(r.Context(), websocket.MessageText, []byte(syntheticBBO)); err != nil {
			return
		}
		_, _, _ = conn.Read(r.Context())
	}))
	defer server.Close()
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()
	out := &failingCaptureFile{}
	source := subscription{source: "binance", market: "ETHUSDT", url: "ws" + strings.TrimPrefix(server.URL, "http")}
	if err := captureWithTiming(ctx, []subscription{source}, out, true); err != nil {
		t.Fatal(err)
	}
	scanner := bufio.NewScanner(bytes.NewReader(out.Bytes()))
	var previous uint64
	var id string
	var messages int
	var last record
	for scanner.Scan() {
		var row record
		if err := json.Unmarshal(scanner.Bytes(), &row); err != nil {
			t.Fatal(err)
		}
		stamp := row.Timing
		if stamp == nil || stamp.Sequence != previous+1 || stamp.AvailableWallNS < stamp.ReceiptWallNS || stamp.AvailableMonoNS < stamp.ReceiptMonoNS || row.ReceivedAt != stamp.ReceiptWallNS/1e6 || row.AvailableAt != stamp.AvailableWallNS/1e6 {
			t.Fatal("invalid clock evidence")
		}
		if id != "" && id != stamp.RunID {
			t.Fatal("changed process clock identity")
		}
		if row.Source == "binance" && stamp.ConnectionEpoch != 1 {
			t.Fatal("missing connection epoch")
		}
		id = stamp.RunID
		previous = stamp.Sequence
		last = row
		if row.Event == "message" {
			messages++
		}
	}
	if messages != 1 || last.Event != "end" || last.Timing.ConnectionEpoch != 0 {
		t.Fatal("missing message or footer")
	}
}

func TestPreciseClockRejectsBackdatingAndClockSteps(t *testing.T) {
	c, err := newCaptureClock()
	if err != nil {
		t.Fatal(err)
	}
	received := c.start.Add(time.Millisecond)
	row := record{received: received}
	if c.stamp(&row, received.Add(-time.Nanosecond)) == nil {
		t.Fatal("available before received")
	}
	if err := c.stamp(&row, received.Add(time.Millisecond)); err != nil {
		t.Fatal(err)
	}
	if c.stamp(&row, received) == nil {
		t.Fatal("availability regressed")
	}
	// Reconstructed wall time has no monotonic component and cannot represent
	// a wall clock step in this synthetic case; direct consistency is checked.
	row.received = time.Time{}
	if c.stamp(&row, time.Now()) == nil {
		t.Fatal("missing socket receipt accepted")
	}
}
