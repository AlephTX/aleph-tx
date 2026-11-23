package exchanges

import (
	"context"
	"fmt"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

func withTestSymbolRegistry(tb testing.TB, symbols map[string]uint16) {
	tb.Helper()
	prev := SymbolNameToID
	SetSymbolRegistry(symbols)
	tb.Cleanup(func() {
		SymbolNameToID = prev
	})
}

func buildArcusBenchmarkPayload(levels int, tsUs int64) []byte {
	var b strings.Builder
	fmt.Fprintf(&b, `{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":%d,"bids":[`, tsUs)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `["%.2f","%.2f"]`, 145.10-float64(i)*0.05, 10.0+float64(i))
	}
	b.WriteString(`],"asks":[`)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `["%.2f","%.2f"]`, 145.15+float64(i)*0.05, 12.0+float64(i))
	}
	b.WriteString(`]}}`)
	return []byte(b.String())
}

func TestArcusPooledDecoderNoStaleLeakageAndEdgeCases(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("arcus-pool-edge")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	withTestSymbolRegistry(t, map[string]uint16{"NVDA": 1003})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"NVDA": "NVDA-USD"},
	}, matrix, depth)

	lastPub := make(map[uint16]int64)
	now := time.UnixMilli(1_794_712_727_000)

	// 1. 20-level message populates all 5 SHM depth levels.
	f1 := buildArcusBenchmarkPayload(20, 1_794_712_726_000_000)
	if err := arc.handleMessage(f1, now, lastPub); err != nil {
		t.Fatal(err)
	}
	d1, ok := depth.ReadDepth(1003, ExchangeArcus)
	if !ok || d1.Bids[4].Price == 0 || d1.Asks[4].Price == 0 {
		t.Fatalf("expected 5 levels populated, got ok=%v %+v", ok, d1)
	}

	// 2. Subsequent 1-level message must zero out levels 2..5 (no pooled stale leakage).
	f2 := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA\u002dUSD","contents":{"timestamp":1794712726100000,"bids":[["145.12","0"],["145.11","3.5"]],"asks":[["145.16","4.5"]]}}`)
	if err := arc.handleMessage(f2, now, lastPub); err != nil {
		t.Fatal(err)
	}
	d2, ok := depth.ReadDepth(1003, ExchangeArcus)
	if !ok || d2.Bids[0].Price != 145.11 || d2.Bids[1].Price != 0 || d2.Asks[0].Price != 145.16 || d2.Asks[1].Price != 0 {
		t.Fatalf("stale levels leaked into SHM depth: ok=%v %+v", ok, d2)
	}

	// 3. Wrong JSON types are ignored cleanly without mutating valid BBO.
	badTypes := [][]byte{
		[]byte(`{invalid`),
		[]byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726200000,"bids":123,"asks":[["145.16","4.5"]]}}`),
		[]byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726200000,"bids":[{"px":"145.11"}],"asks":[["145.16","4.5"]]}}`),
	}
	for _, raw := range badTypes {
		if err := arc.handleMessage(raw, now, lastPub); err != nil {
			t.Fatalf("expected malformed JSON/type ignored cleanly, got %v", err)
		}
	}
	if bbo, ok := matrix.ReadBBO(ExchangeArcus, 1003); !ok || bbo.BidPrice != 145.11 {
		t.Fatalf("wrong JSON type disturbed valid BBO: ok=%v %+v", ok, bbo)
	}

	// 4. Empty side invalidates without error; non-monotonic ladder errors & invalidates.
	emptyBids := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726300000,"bids":[],"asks":[["145.16","4.5"]]}}`)
	if err := arc.handleMessage(emptyBids, now, lastPub); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeArcus, 1003); ok {
		t.Fatal("empty bids did not invalidate BBO")
	}

	// Restore, then feed non-monotonic bids.
	if err := arc.handleMessage([]byte(`{"type":"subscribed","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726400000,"bids":[["145.10","2.0"]],"asks":[["145.20","2.0"]]}}`), now, lastPub); err != nil {
		t.Fatal(err)
	}
	nonMono := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726500000,"bids":[["145.10","2.0"],["145.11","1.0"]],"asks":[["145.20","2.0"]]}}`)
	if err := arc.handleMessage(nonMono, now, lastPub); err == nil {
		t.Fatal("expected non-monotonic bid ladder error")
	}
	if _, ok := matrix.ReadBBO(ExchangeArcus, 1003); ok {
		t.Fatal("non-monotonic bid ladder left BBO readable")
	}
}

func TestArcusConcurrentPoolSafety(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("arcus-conc")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(t, map[string]uint16{"NVDA": 1003})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"NVDA": "NVDA-USD"},
	}, matrix, nil)

	now := time.UnixMilli(1_794_712_727_000)
	payload := buildArcusBenchmarkPayload(20, 1_794_712_726_000_000)

	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			localPub := make(map[uint16]int64)
			for i := 0; i < 100; i++ {
				localPub[1003] = 0
				if err := arc.handleMessage(payload, now, localPub); err != nil {
					t.Errorf("concurrent handleMessage error: %v", err)
					return
				}
			}
		}()
	}
	wg.Wait()
}

func TestArcusWebSocketReaderLoopEndToEnd(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("arcus-ws-e2e")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	tsUs := time.Now().UnixMicro()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		var sub map[string]any
		if wsjson.Read(r.Context(), c, &sub) != nil {
			return
		}
		_ = c.Write(r.Context(), websocket.MessageText, buildArcusBenchmarkPayload(10, tsUs))
		_ = c.Write(r.Context(), websocket.MessageText, []byte(fmt.Sprintf(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":%d,"bids":[["146.00","5.0"]],"asks":[["146.10","6.0"]]}}`, tsUs+100)))
		<-r.Context().Done()
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	withTestSymbolRegistry(t, map[string]uint16{"NVDA": 1003})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   wsURL,
		Symbols: map[string]string{"NVDA": "NVDA-USD"},
	}, matrix, depth)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- arc.connect(ctx, make(map[uint16]int64))
	}()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if snap, ok := matrix.ReadBBO(ExchangeArcus, 1003); ok && snap.BidPrice == 146.00 {
			d, dok := depth.ReadDepth(1003, ExchangeArcus)
			if !dok || d.Bids[1].Price != 0 || d.Asks[0].Price != 146.10 {
				t.Fatalf("unexpected end-to-end depth: dok=%v %+v", dok, d)
			}
			cancel()
			<-done
			if _, live := matrix.ReadBBO(ExchangeArcus, 1003); live {
				t.Fatal("expected BBO invalidated on disconnect")
			}
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("timed out waiting for Arcus WS BBO")
}

func BenchmarkArcusHandleMessage20Levels(b *testing.B) {
	tmp := b.TempDir()
	b.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("arcus-bench")
	if err != nil {
		b.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		b.Fatal(err)
	}
	defer depth.Close()

	withTestSymbolRegistry(b, map[string]uint16{"NVDA": 1003})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"NVDA": "NVDA-USD"},
	}, matrix, depth)

	const tsUs = int64(1_794_712_726_850_123)
	payload := buildArcusBenchmarkPayload(20, tsUs)
	lastPub := map[uint16]int64{1003: 0}
	now := time.UnixMilli(1_794_712_727_000)

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		lastPub[1003] = 0
		if err := arc.handleMessage(payload, now, lastPub); err != nil {
			b.Fatal(err)
		}
	}
}
