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

func buildHyperliquidL2BenchmarkPayload(levels int, tsMs int64) []byte {
	var b strings.Builder
	fmt.Fprintf(&b, `{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":%d,"levels":[[`, tsMs)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"px":"%.2f","sz":"%.2f","n":1}`, 145.00-float64(i)*0.05, 5.0+float64(i))
	}
	b.WriteString(`],[`)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"px":"%.2f","sz":"%.2f","n":1}`, 145.10+float64(i)*0.05, 6.0+float64(i))
	}
	b.WriteString(`]]}}`)
	return []byte(b.String())
}

func TestHyperliquidPooledDecodersNoStaleLeakageAndEdgeCases(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("hl-pool-edge")
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
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      "wss://api.hyperliquid.xyz/ws",
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, depth)

	lastBbo := make(map[hlTarget]int64)
	lastDepth := make(map[hlTarget]int64)
	now := time.UnixMilli(1_794_712_730_000)

	// 1. 20-level book fills all 5 depth slots.
	f1 := buildHyperliquidL2BenchmarkPayload(20, 1_794_712_726_000)
	if err := hl.handleMessageWithDepth(f1, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	d1, ok := depth.ReadDepth(1003, ExchangeTradeXYZ)
	if !ok || d1.Bids[4].Price == 0 || d1.Asks[4].Price == 0 {
		t.Fatalf("expected 5 HL depth levels, got ok=%v %+v", ok, d1)
	}

	// 2. 1-level book with reordered fields ("n" before "sz" before "px") and escaped coin clears levels 2..5.
	f2 := []byte(`{"channel":"l2Book","data":{"coin":"xyz\u003aNVDA","time":1794712726100,"levels":[[{"n":2,"sz":"0","px":"145.50"},{"n":1,"sz":"3.0","px":"145.05"}],[{"n":1,"sz":"4.0","px":"145.15"}]]}}`)
	if err := hl.handleMessageWithDepth(f2, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	d2, ok := depth.ReadDepth(1003, ExchangeTradeXYZ)
	if !ok || d2.Bids[0].Price != 145.05 || d2.Bids[1].Price != 0 || d2.Asks[0].Price != 145.15 || d2.Asks[1].Price != 0 {
		t.Fatalf("stale levels leaked in HL depth: ok=%v %+v", ok, d2)
	}

	// 3. BBO frame followed by BBO frame with missing "sz" fails closed (no pooled field leakage).
	bboValid := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726200,"bbo":[{"n":1,"sz":"2.2","px":"145.06"},{"n":2,"sz":"1.8","px":"145.14"}]}}`)
	if err := hl.handleMessageWithDepth(bboValid, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if bbo, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); !ok || bbo.BidPrice != 145.06 {
		t.Fatalf("expected valid BBO 145.06, got ok=%v %+v", ok, bbo)
	}
	bboMissingSz := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726250,"bbo":[{"px":"145.07"},{"px":"145.14","sz":"1.8"}]}}`)
	if err := hl.handleMessageWithDepth(bboMissingSz, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("BBO missing sz leaked pooled field instead of invalidating")
	}

	// 4. Malformed JSON/wrong field types are ignored safely.
	for _, raw := range [][]byte{
		[]byte(`{bad`),
		[]byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726300,"levels":123}}`),
		[]byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726300,"bbo":[{"px":145,"sz":"1"},{"px":"146","sz":"1"}]}}`),
	} {
		if err := hl.handleMessageWithDepth(raw, now, lastBbo, lastDepth); err != nil {
			t.Fatalf("unexpected error on malformed frame: %v", err)
		}
	}
}

func TestHyperliquidConcurrentPoolSafety(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("hl-conc")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(t, map[string]uint16{"NVDA": 1003})
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      "wss://api.hyperliquid.xyz/ws",
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, nil)

	target := hlTarget{exchangeID: ExchangeTradeXYZ, symbolID: 1003}
	l2 := buildHyperliquidL2BenchmarkPayload(20, 1_794_712_726_000)
	bbo := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726200,"bbo":[{"px":"145.02","sz":"2.2","n":1},{"px":"145.09","sz":"1.8","n":2}]}}`)
	now := time.UnixMilli(1_794_712_730_000)

	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			lb := make(map[hlTarget]int64)
			ld := make(map[hlTarget]int64)
			for i := 0; i < 100; i++ {
				lb[target] = 0
				ld[target] = 0
				if err := hl.handleMessageWithDepth(l2, now, lb, ld); err != nil {
					t.Errorf("concurrent L2 error: %v", err)
					return
				}
				lb[target] = 0
				if err := hl.handleMessageWithDepth(bbo, now, lb, ld); err != nil {
					t.Errorf("concurrent BBO error: %v", err)
					return
				}
			}
		}()
	}
	wg.Wait()
}

func TestHyperliquidWebSocketReaderLoopEndToEnd(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("hl-ws-e2e")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	tsMs := time.Now().UnixMilli()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		for range 2 {
			var sub map[string]any
			if wsjson.Read(r.Context(), c, &sub) != nil {
				return
			}
		}
		_ = c.Write(r.Context(), websocket.MessageText, buildHyperliquidL2BenchmarkPayload(10, tsMs))
		_ = c.Write(r.Context(), websocket.MessageText, []byte(fmt.Sprintf(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":%d,"bbo":[{"px":"145.03","sz":"3.1","n":1},{"px":"145.08","sz":"2.7","n":1}]}}`, tsMs+50)))
		<-r.Context().Done()
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	withTestSymbolRegistry(t, map[string]uint16{"NVDA": 1003})
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      wsURL,
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, depth)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- hl.connect(ctx, make(map[hlTarget]int64), make(map[hlTarget]int64))
	}()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if snap, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok && snap.BidPrice == 145.03 {
			if _, dok := depth.ReadDepth(1003, ExchangeTradeXYZ); !dok {
				t.Fatal("expected HL depth populated from WS loop")
			}
			cancel()
			<-done
			if _, live := matrix.ReadBBO(ExchangeTradeXYZ, 1003); live {
				t.Fatal("expected HL BBO invalidated on disconnect")
			}
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("timed out waiting for HL WS BBO")
}

func BenchmarkHyperliquidL2Book20Levels(b *testing.B) {
	tmp := b.TempDir()
	b.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("hl-l2-bench")
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
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      "wss://api.hyperliquid.xyz/ws",
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, depth)

	target := hlTarget{exchangeID: ExchangeTradeXYZ, symbolID: 1003}
	const tsMs = int64(1_794_712_726_000)
	payload := buildHyperliquidL2BenchmarkPayload(20, tsMs)
	lastBbo := map[hlTarget]int64{target: 0}
	lastDepth := map[hlTarget]int64{target: 0}
	now := time.UnixMilli(1_794_712_727_000)

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		lastBbo[target] = 0
		lastDepth[target] = 0
		if err := hl.handleMessageWithDepth(payload, now, lastBbo, lastDepth); err != nil {
			b.Fatal(err)
		}
	}
}

func BenchmarkHyperliquidBBO(b *testing.B) {
	tmp := b.TempDir()
	b.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("hl-bbo-bench")
	if err != nil {
		b.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(b, map[string]uint16{"NVDA": 1003})
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      "wss://api.hyperliquid.xyz/ws",
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, nil)

	target := hlTarget{exchangeID: ExchangeTradeXYZ, symbolID: 1003}
	payload := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726200,"bbo":[{"px":"145.02","sz":"2.2","n":1},{"px":"145.09","sz":"1.8","n":2}]}}`)
	lastBbo := map[hlTarget]int64{target: 0}
	lastDepth := map[hlTarget]int64{target: 0}
	now := time.UnixMilli(1_794_712_727_000)

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		lastBbo[target] = 0
		if err := hl.handleMessageWithDepth(payload, now, lastBbo, lastDepth); err != nil {
			b.Fatal(err)
		}
	}
}
