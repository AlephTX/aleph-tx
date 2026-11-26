package exchanges

import (
	"context"
	"encoding/json"
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

	lastPub := make(map[uint16]arcusSymbolState)
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
			localPub := make(map[uint16]arcusSymbolState)
			for i := 0; i < 100; i++ {
				localPub[1003] = arcusSymbolState{}
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
		done <- arc.connect(ctx, make(map[uint16]arcusSymbolState))
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
	lastPub := map[uint16]arcusSymbolState{}
	now := time.UnixMilli(1_794_712_727_000)

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		lastPub[1003] = arcusSymbolState{}
		if err := arc.handleMessage(payload, now, lastPub); err != nil {
			b.Fatal(err)
		}
	}
}

func newArcusBBOTestFeed(t *testing.T, name string) (*Arcus, *shm.Matrix, *shm.DepthWriter) {
	t.Helper()
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix(name)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { matrix.Close() })
	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { depth.Close() })
	withTestSymbolRegistry(t, map[string]uint16{"BTC": 1})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"BTC": "BTC-USD"},
	}, matrix, depth)
	return arc, matrix, depth
}

func arcusBBOFrame(kind string, tsUs int64, seq uint64, bid, ask string) []byte {
	return []byte(fmt.Sprintf(`{"type":%q,"channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":%q,"size":"0.08408347"},"bestAsk":{"price":%q,"size":"0.01058685"},"timestamp":%d,"lastSequenceId":980274961,"globalSequenceId":%d}}`, kind, bid, ask, tsUs, seq))
}

func arcusL2Frame(tsUs int64, seq uint64, bid, ask string) []byte {
	return []byte(fmt.Sprintf(`{"type":"channel_data","channel":"l2Orderbook","id":"BTC-USD","contents":{"timestamp":%d,"lastSequenceId":980274900,"globalSequenceId":%d,"bids":[[%q,"1.5"],["60000.0","2"]],"asks":[[%q,"1.25"],["61000.0","3"]]}}`, tsUs, seq, bid, ask))
}

func TestArcusParsesBBOChannel(t *testing.T) {
	arc, matrix, depth := newArcusBBOTestFeed(t, "arcus-bbo-parse")
	states := make(map[uint16]arcusSymbolState)
	now := time.UnixMicro(1_795_691_150_500_000)

	// Real public message shape from wss://api.arcus.xyz/v1/ws.
	raw := []byte(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"60415.1","size":"0.08408347"},"bestAsk":{"price":"60415.2","size":"0.01058685"},"timestamp":1795691150080124,"lastSequenceId":980274961,"globalSequenceId":16893834613}}`)
	if err := arc.handleMessage(raw, now, states); err != nil {
		t.Fatal(err)
	}
	bbo, ok := matrix.ReadBBO(ExchangeArcus, 1)
	if !ok || bbo.BidPrice != 60415.1 || bbo.BidSize != 0.08408347 || bbo.AskPrice != 60415.2 ||
		bbo.AskSize != 0.01058685 || bbo.TimestampNs != 1795691150080124*1_000 {
		t.Fatalf("unexpected bbo publication: ok=%v %+v", ok, bbo)
	}
	if st := states[1]; st.bboSeq != 16893834613 || st.bboTsUs != 1795691150080124 || st.depthSeq != 0 {
		t.Fatalf("unexpected watermark after bbo: %+v", st)
	}
	if _, ok := depth.ReadDepth(1, ExchangeArcus); ok {
		t.Fatal("bbo must not publish depth")
	}

	// A subscribed snapshot of the bbo channel is accepted too.
	if err := arc.handleMessage(arcusBBOFrame("subscribed", 1795691150090000, 16893834620, "60415.0", "60415.3"), now, states); err != nil {
		t.Fatal(err)
	}
	if bbo, ok := matrix.ReadBBO(ExchangeArcus, 1); !ok || bbo.BidPrice != 60415.0 {
		t.Fatalf("subscribed bbo snapshot not published: ok=%v %+v", ok, bbo)
	}

	// Unknown key order and extra fields decode identically; a decoding
	// allocation regression would show up in the benchmark.
	reordered := []byte(`{"contents":{"globalSequenceId":16893834630,"timestamp":1795691150095000,"bestAsk":{"size":"2","price":"60415.4","extra":[1,{"a":"b"}]},"bestBid":{"size":"1","price":"60415.1"}},"id":"BTC-USD","channel":"bbo","type":"channel_data"}`)
	if err := arc.handleMessage(reordered, now, states); err != nil {
		t.Fatal(err)
	}
	if bbo, ok := matrix.ReadBBO(ExchangeArcus, 1); !ok || bbo.BidPrice != 60415.1 || bbo.AskPrice != 60415.4 || bbo.AskSize != 2 {
		t.Fatalf("reordered bbo not published: ok=%v %+v", ok, bbo)
	}

	// Empty side invalidates without error and advances the watermark.
	empty := []byte(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":null,"bestAsk":{"price":"60415.4","size":"1"},"timestamp":1795691150096000,"globalSequenceId":16893834640}}`)
	if err := arc.handleMessage(empty, now, states); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeArcus, 1); ok {
		t.Fatal("empty bbo side did not invalidate the quote")
	}
	// An older l2 snapshot cannot resurrect the invalidated quote.
	if err := arc.handleMessage(arcusL2Frame(1795691150070000, 16893834500, "60415.0", "60415.5"), now, states); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeArcus, 1); ok {
		t.Fatal("stale l2 snapshot resurrected an invalidated bbo")
	}
}

func TestArcusSequenceGuardKeepsFresherBBO(t *testing.T) {
	arc, matrix, depth := newArcusBBOTestFeed(t, "arcus-bbo-seq")
	states := make(map[uint16]arcusSymbolState)
	now := time.UnixMicro(1_795_691_151_000_000)

	if err := arc.handleMessage(arcusBBOFrame("channel_data", 1795691150080124, 16893834613, "60415.1", "60415.2"), now, states); err != nil {
		t.Fatal(err)
	}
	// The l2 snapshot is older by sequence, even with a later timestamp
	// (snapshot emission time). Its top of book must not overwrite the bbo,
	// but its depth is still published.
	if err := arc.handleMessage(arcusL2Frame(1795691150100000, 16893834600, "60410.0", "60420.0"), now, states); err != nil {
		t.Fatal(err)
	}
	bbo, ok := matrix.ReadBBO(ExchangeArcus, 1)
	if !ok || bbo.BidPrice != 60415.1 || bbo.AskPrice != 60415.2 {
		t.Fatalf("stale l2 snapshot overwrote fresher bbo: ok=%v %+v", ok, bbo)
	}
	d, ok := depth.ReadDepth(1, ExchangeArcus)
	if !ok || d.Bids[0].Price != 60410.0 || d.Asks[1].Price != 61000.0 {
		t.Fatalf("l2 depth not published alongside guarded bbo: ok=%v %+v", ok, d)
	}

	// Equal sequence is not strictly newer.
	if err := arc.handleMessage(arcusL2Frame(1795691150110000, 16893834613, "60411.0", "60421.0"), now, states); err != nil {
		t.Fatal(err)
	}
	if bbo, _ := matrix.ReadBBO(ExchangeArcus, 1); bbo.BidPrice != 60415.1 {
		t.Fatalf("equal-sequence l2 overwrote bbo: %+v", bbo)
	}

	// A replayed older bbo is ignored.
	if err := arc.handleMessage(arcusBBOFrame("channel_data", 1795691150070000, 16893834500, "60400.0", "60401.0"), now, states); err != nil {
		t.Fatal(err)
	}
	if bbo, _ := matrix.ReadBBO(ExchangeArcus, 1); bbo.BidPrice != 60415.1 {
		t.Fatalf("older bbo overwrote newer bbo: %+v", bbo)
	}

	// A newer l2 snapshot publishes both top of book and depth.
	if err := arc.handleMessage(arcusL2Frame(1795691150200000, 16893834700, "60412.0", "60412.5"), now, states); err != nil {
		t.Fatal(err)
	}
	if bbo, ok := matrix.ReadBBO(ExchangeArcus, 1); !ok || bbo.BidPrice != 60412.0 || bbo.AskPrice != 60412.5 || bbo.BidSize != 1.5 {
		t.Fatalf("newer l2 snapshot did not publish top of book: ok=%v %+v", ok, bbo)
	}
	if d, _ := depth.ReadDepth(1, ExchangeArcus); d.Bids[0].Price != 60412.0 {
		t.Fatalf("newer l2 snapshot did not publish depth: %+v", d)
	}
	if st := states[1]; st.bboSeq != 16893834700 || st.depthSeq != 16893834700 {
		t.Fatalf("unexpected watermarks: %+v", st)
	}
}

func TestArcusRejectsCrossedAndMalformedBBO(t *testing.T) {
	cases := map[string][]byte{
		"crossed": arcusBBOFrame("channel_data", 1795691150090000, 16893834620, "60415.3", "60415.2"),
		"locked":  arcusBBOFrame("channel_data", 1795691150090000, 16893834620, "60415.2", "60415.2"),
		"bad_px":  arcusBBOFrame("channel_data", 1795691150090000, 16893834620, "abc", "60415.2"),
		"neg_sz":  []byte(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"60415.1","size":"-1"},"bestAsk":{"price":"60415.2","size":"1"},"timestamp":1795691150090000,"globalSequenceId":16893834620}}`),
	}
	for name, raw := range cases {
		t.Run(name, func(t *testing.T) {
			arc, matrix, _ := newArcusBBOTestFeed(t, "arcus-bbo-bad-"+name)
			states := make(map[uint16]arcusSymbolState)
			now := time.UnixMicro(1_795_691_151_000_000)
			if err := arc.handleMessage(arcusBBOFrame("channel_data", 1795691150080124, 16893834613, "60415.1", "60415.2"), now, states); err != nil {
				t.Fatal(err)
			}
			if err := arc.handleMessage(raw, now, states); err == nil {
				t.Fatal("expected invalid bbo to be rejected with an error")
			}
			if _, ok := matrix.ReadBBO(ExchangeArcus, 1); ok {
				t.Fatal("invalid bbo left the previous quote readable")
			}
		})
	}
}

func TestArcusSubscribeRequestsIncludeBBOAndL2(t *testing.T) {
	got := arcusSubscribeRequests([]string{"BTC-USD", "ETH-USD"})
	want := []string{
		`{"type":"subscribe","channel":"l2Orderbook","id":"BTC-USD","nLevels":20}`,
		`{"type":"subscribe","channel":"bbo","id":"BTC-USD"}`,
		`{"type":"subscribe","channel":"l2Orderbook","id":"ETH-USD","nLevels":20}`,
		`{"type":"subscribe","channel":"bbo","id":"ETH-USD"}`,
	}
	if len(got) != len(want) {
		t.Fatalf("got %d requests, want %d", len(got), len(want))
	}
	for i := range want {
		b, err := json.Marshal(got[i])
		if err != nil {
			t.Fatal(err)
		}
		if string(b) != want[i] {
			t.Fatalf("request %d: got %s want %s", i, b, want[i])
		}
	}
}

func TestArcusWebSocketSubscribesAndPublishesBBO(t *testing.T) {
	arc, matrix, _ := newArcusBBOTestFeed(t, "arcus-bbo-ws")
	tsUs := time.Now().UnixMicro()
	subs := make(chan []string, 1)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		var channels []string
		for range 2 {
			var sub map[string]any
			if wsjson.Read(r.Context(), c, &sub) != nil {
				return
			}
			channels = append(channels, fmt.Sprint(sub["channel"], ":", sub["id"]))
		}
		subs <- channels
		_ = c.Write(r.Context(), websocket.MessageText, arcusL2Frame(tsUs, 100, "60410.0", "60420.0"))
		_ = c.Write(r.Context(), websocket.MessageText, arcusBBOFrame("channel_data", tsUs+50, 102, "60415.1", "60415.2"))
		// Newer depth (seq 101 > 100) but older top of book (101 < 102).
		_ = c.Write(r.Context(), websocket.MessageText, arcusL2Frame(tsUs+100, 101, "60409.0", "60421.0"))
		<-r.Context().Done()
	}))
	defer srv.Close()
	arc.cfg.WSURL = "ws" + strings.TrimPrefix(srv.URL, "http")

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() { done <- arc.connect(ctx, make(map[uint16]arcusSymbolState)) }()
	// Stop the reader before the SHM mappings are closed by earlier cleanups.
	var stopOnce sync.Once
	stop := func() { stopOnce.Do(func() { cancel(); <-done }) }
	t.Cleanup(stop)

	select {
	case channels := <-subs:
		if strings.Join(channels, ",") != "l2Orderbook:BTC-USD,bbo:BTC-USD" {
			t.Fatalf("unexpected subscriptions: %v", channels)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for subscriptions")
	}
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if d, ok := arc.depthWriter.ReadDepth(1, ExchangeArcus); ok && d.Bids[0].Price == 60409.0 {
			bbo, ok := matrix.ReadBBO(ExchangeArcus, 1)
			if !ok || bbo.BidPrice != 60415.1 {
				t.Fatalf("stale l2 overwrote bbo over the wire: ok=%v %+v", ok, bbo)
			}
			stop()
			if _, live := matrix.ReadBBO(ExchangeArcus, 1); live {
				t.Fatal("expected BBO invalidated on disconnect")
			}
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("timed out waiting for Arcus WS publications")
}

func BenchmarkArcusHandleMessageBBO(b *testing.B) {
	tmp := b.TempDir()
	b.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("arcus-bbo-bench")
	if err != nil {
		b.Fatal(err)
	}
	defer matrix.Close()
	withTestSymbolRegistry(b, map[string]uint16{"BTC": 1})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"BTC": "BTC-USD"},
	}, matrix, nil)
	payload := arcusBBOFrame("channel_data", 1795691150080124, 16893834613, "60415.1", "60415.2")
	states := map[uint16]arcusSymbolState{}
	now := time.UnixMicro(1_795_691_151_000_000)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		states[1] = arcusSymbolState{}
		if err := arc.handleMessage(payload, now, states); err != nil {
			b.Fatal(err)
		}
	}
}

func TestArcusHardenedValidationTableDriven(t *testing.T) {
	const (
		baseTs  int64  = 1_795_691_150_080_124
		baseSeq uint64 = 16_893_834_613
	)
	now := time.UnixMicro(baseTs + 500_000)

	type tc struct {
		name         string
		payload      []byte
		wantErr      bool
		wantBBOValid bool
	}

	cases := []tc{
		{
			name:         "malformed_json_ignored_preserves_valid_bbo",
			payload:      []byte(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":`),
			wantErr:      false,
			wantBBOValid: true,
		},
		{
			name:         "nan_bid_price_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "NaN", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "inf_ask_price_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "60415.1", "Inf"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "underscore_float_rejected_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "60_415.1", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "hex_float_rejected_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "0x1p15", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "zero_bid_price_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "0", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "negative_bid_price_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "-60415.1", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "zero_ask_size_invalidates_cleanly",
			payload:      []byte(fmt.Sprintf(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"60415.1","size":"1.0"},"bestAsk":{"price":"60415.2","size":"0"},"timestamp":%d,"globalSequenceId":%d}}`, baseTs+1000, baseSeq+1)),
			wantErr:      false,
			wantBBOValid: false,
		},
		{
			name:         "crossed_bbo_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "60415.3", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "locked_bbo_invalidates_and_errors",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq+1, "60415.2", "60415.2"),
			wantErr:      true,
			wantBBOValid: false,
		},
		{
			name:         "regressed_sequence_ignored_preserves_valid_bbo",
			payload:      arcusBBOFrame("channel_data", baseTs+1000, baseSeq-1, "60400.0", "60401.0"),
			wantErr:      false,
			wantBBOValid: true,
		},
		{
			name:         "missing_sequence_cannot_bypass_seq_watermark",
			payload:      []byte(fmt.Sprintf(`{"type":"channel_data","channel":"bbo","id":"BTC-USD","contents":{"bestBid":{"price":"60400.0","size":"1.0"},"bestAsk":{"price":"60401.0","size":"1.0"},"timestamp":%d}}`, baseTs+1000)),
			wantErr:      false,
			wantBBOValid: true,
		},
		{
			name:         "regressed_timestamp_with_newer_seq_ignored",
			payload:      arcusBBOFrame("channel_data", baseTs-1, baseSeq+1, "60400.0", "60401.0"),
			wantErr:      false,
			wantBBOValid: true,
		},
		{
			name:         "millisecond_epoch_rejected_preserves_valid_bbo",
			payload:      arcusBBOFrame("channel_data", 1_795_691_150_080, baseSeq+1, "60400.0", "60401.0"),
			wantErr:      false,
			wantBBOValid: true,
		},
		{
			name:         "future_timestamp_errors",
			payload:      arcusBBOFrame("channel_data", now.Add(5*time.Second).UnixMicro(), baseSeq+1, "60415.1", "60415.2"),
			wantErr:      true,
			wantBBOValid: true,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			arc, matrix, _ := newArcusBBOTestFeed(t, "arcus-hard-"+tc.name)
			states := make(map[uint16]arcusSymbolState)
			if err := arc.handleMessage(arcusBBOFrame("channel_data", baseTs, baseSeq, "60415.1", "60415.2"), now, states); err != nil {
				t.Fatal(err)
			}
			err := arc.handleMessage(tc.payload, now, states)
			if (err != nil) != tc.wantErr {
				t.Fatalf("err=%v wantErr=%v", err, tc.wantErr)
			}
			bbo, ok := matrix.ReadBBO(ExchangeArcus, 1)
			if ok != tc.wantBBOValid {
				t.Fatalf("bbo readable=%v want=%v (%+v)", ok, tc.wantBBOValid, bbo)
			}
			if tc.wantBBOValid && bbo.BidPrice != 60415.1 {
				t.Fatalf("valid BBO mutated by rejected frame: %+v", bbo)
			}
		})
	}
}

func TestArcusConcurrentBBOAndL2Race(t *testing.T) {
	arc, _, _ := newArcusBBOTestFeed(t, "arcus-bbo-l2-race")
	now := time.UnixMicro(1_795_691_151_000_000)
	bboMsg := arcusBBOFrame("channel_data", 1_795_691_150_080_124, 100, "60415.1", "60415.2")
	l2Msg := arcusL2Frame(1_795_691_150_090_000, 101, "60415.0", "60415.3")

	var wg sync.WaitGroup
	for g := 0; g < 8; g++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			states := make(map[uint16]arcusSymbolState)
			for i := 0; i < 100; i++ {
				states[1] = arcusSymbolState{}
				if err := arc.handleMessage(bboMsg, now, states); err != nil {
					t.Errorf("concurrent BBO error: %v", err)
					return
				}
				if err := arc.handleMessage(l2Msg, now, states); err != nil {
					t.Errorf("concurrent L2 error: %v", err)
					return
				}
			}
		}()
	}
	wg.Wait()
}
