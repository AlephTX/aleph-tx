package exchanges

import (
	"context"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

func TestArcusHandleMessageWritesBBOAndDepthAndInvalidates(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("arcus-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	SetSymbolRegistry(map[string]uint16{"ETH": 1002, "NVDA": 1003})
	arc := NewArcus(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://api.arcus.xyz/v1/ws",
		Symbols: map[string]string{"NVDA": "NVDA-USD", "ETH": "ETH-USD"},
	}, matrix, depth)

	lastPub := make(map[uint16]int64)
	now := time.UnixMilli(1_794_712_727_000)

	valid := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726850123,"bids":[["145.10","25.0"],["145.05","40.0"]],"asks":[["145.15","30.0"],["145.20","50.0"]]}}`)
	if err := arc.handleMessage(valid, now, lastPub); err != nil {
		t.Fatal(err)
	}
	snap, ok := matrix.ReadBBO(ExchangeArcus, 1003)
	if !ok || snap.BidPrice != 145.10 || snap.AskPrice != 145.15 || snap.TimestampNs != 1_794_712_726_850_123_000 {
		t.Fatalf("unexpected Arcus BBO: ok=%v snap=%+v", ok, snap)
	}
	if depth.GetVersion(1003) == 0 {
		t.Fatal("expected Arcus depth version increment")
	}

	// Non-increasing microsecond timestamp is ignored without overwriting.
	stale := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726850123,"bids":[["140.00","1.0"]],"asks":[["140.10","1.0"]]}}`)
	if err := arc.handleMessage(stale, now, lastPub); err != nil {
		t.Fatal(err)
	}
	snap2, ok := matrix.ReadBBO(ExchangeArcus, 1003)
	if !ok || snap2.BidPrice != 145.10 {
		t.Fatalf("stale update mutated BBO: %+v", snap2)
	}

	// Crossed book invalidates slot and returns an error to force reconnect.
	crossed := []byte(`{"type":"channel_data","channel":"l2Orderbook","id":"NVDA-USD","contents":{"timestamp":1794712726850200,"bids":[["145.20","1.0"]],"asks":[["145.15","1.0"]]}}`)
	if err := arc.handleMessage(crossed, now, lastPub); err == nil {
		t.Fatal("expected crossed Arcus book error")
	}
	if _, ok := matrix.ReadBBO(ExchangeArcus, 1003); ok {
		t.Fatal("crossed Arcus book remained readable in SHM")
	}
}

func TestHyperliquidGroupRoutesCoreEntropyAndTradeXYZWithoutSlotCollision(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("hl-group-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	SetSymbolRegistry(map[string]uint16{"ETH": 1002, "NVDA": 1003})
	hl := NewHyperliquidGroup("wss://api.hyperliquid.xyz/ws", []config.ExchangeConfig{
		{Enabled: true, WSURL: "wss://api.hyperliquid.xyz/ws", Symbols: map[string]string{"ETH": "ETH"}},
		{Enabled: true, WSURL: "wss://api.hyperliquid.xyz/ws", Symbols: map[string]string{"NVDA": "io:NVDA"}},
		{Enabled: true, WSURL: "wss://api.hyperliquid.xyz/ws", Symbols: map[string]string{"NVDA": "xyz:NVDA"}},
	}, matrix)

	lastPub := make(map[hlTarget]int64)
	now := time.UnixMilli(1_794_712_727_000)

	frames := [][]byte{
		[]byte(`{"channel":"l2Book","data":{"coin":"ETH","time":1794712726500,"levels":[[{"px":"2450.0","sz":"5.0"}],[{"px":"2450.5","sz":"4.0"}]]}}`),
		[]byte(`{"channel":"l2Book","data":{"coin":"io:NVDA","time":1794712726510,"levels":[[{"px":"145.00","sz":"10.0"}],[{"px":"145.10","sz":"12.0"}]]}}`),
		[]byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726520,"levels":[[{"px":"145.05","sz":"8.0"}],[{"px":"145.12","sz":"9.0"}]]}}`),
	}
	for _, f := range frames {
		if err := hl.handleMessage(f, now, lastPub); err != nil {
			t.Fatal(err)
		}
	}

	coreETH, ok := matrix.ReadBBO(ExchangeHyperliquid, 1002)
	if !ok || coreETH.BidPrice != 2450.0 {
		t.Fatalf("missing core ETH BBO: ok=%v %+v", ok, coreETH)
	}
	entropyNVDA, ok := matrix.ReadBBO(ExchangeEntropy, 1003)
	if !ok || entropyNVDA.BidPrice != 145.00 || entropyNVDA.AskPrice != 145.10 {
		t.Fatalf("missing Entropy NVDA BBO: ok=%v %+v", ok, entropyNVDA)
	}
	xyzNVDA, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003)
	if !ok || xyzNVDA.BidPrice != 145.05 || xyzNVDA.AskPrice != 145.12 {
		t.Fatalf("missing TradeXYZ NVDA BBO: ok=%v %+v", ok, xyzNVDA)
	}

	hl.invalidateAll()
	if _, ok := matrix.ReadBBO(ExchangeEntropy, 1003); ok {
		t.Fatal("Entropy slot not invalidated")
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("TradeXYZ slot not invalidated")
	}

	validETH := []byte(`{"channel":"l2Book","data":{"coin":"ETH","time":1794712726550,"levels":[[{"px":"2450.2","sz":"5.0"}],[{"px":"2450.5","sz":"4.0"}]]}}`)
	if err := hl.handleMessage(validETH, now, lastPub); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeHyperliquid, 1002); !ok {
		t.Fatal("expected ETH BBO live before crossed check")
	}
	crossed := []byte(`{"channel":"l2Book","data":{"coin":"ETH","time":1794712726600,"levels":[[{"px":"2451.0","sz":"5.0"}],[{"px":"2450.5","sz":"4.0"}]]}}`)
	if err := hl.handleMessage(crossed, now, lastPub); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeHyperliquid, 1002); ok {
		t.Fatal("crossed Hyperliquid book remained readable in SHM")
	}
}

func TestLighterRobinhoodUsesDedicatedExchangeSlot(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("lighter-rh-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	tsMs := time.Now().UnixMilli()
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
		_ = wsjson.Write(r.Context(), c, map[string]any{
			"type":    "subscribed/order_book",
			"channel": "order_book:0",
			"order_book": map[string]any{
				"nonce":           10,
				"begin_nonce":     0,
				"offset":          1,
				"last_updated_at": tsMs * 1000,
				"bids":            []map[string]string{{"price": "2451.10", "size": "2.5"}},
				"asks":            []map[string]string{{"price": "2451.30", "size": "3.0"}},
			},
		})
		<-r.Context().Done()
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	SetSymbolRegistry(config.DefaultSymbols())
	rh := NewLighterWithID(ExchangeLighterRobinhood, "lighter-rh-test", config.ExchangeConfig{
		Enabled: true,
		WSURL:   wsURL,
		Symbols: map[string]string{"ETH": "0"},
	}, matrix, nil, nil)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() { done <- rh.connectPublic(ctx) }()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if snap, ok := matrix.ReadBBO(ExchangeLighterRobinhood, SymbolETHPERP); ok && snap.BidPrice == 2451.10 {
			if _, coreTouched := matrix.ReadBBO(ExchangeLighter, SymbolETHPERP); coreTouched {
				t.Fatal("Lighter Robinhood adapter polluted Lighter Core slot")
			}
			cancel()
			<-done
			if _, stillLive := matrix.ReadBBO(ExchangeLighterRobinhood, SymbolETHPERP); stillLive {
				t.Fatal("Lighter Robinhood slot was not invalidated on disconnect")
			}
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("timed out waiting for Lighter Robinhood BBO in slot 8")
}

func TestHyperliquidWritesTop5DepthAndHandlesBBOInterleaving(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("hl-depth-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	SetSymbolRegistry(map[string]uint16{"NVDA": 1003})
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

	// 1. l2Book updates both BBO matrix and Top-5 aleph-depth.
	l2 := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726000,"levels":[
		[{"px":"145.00","sz":"1.0"},{"px":"144.95","sz":"2.0"},{"px":"144.90","sz":"3.0"},{"px":"144.85","sz":"4.0"},{"px":"144.80","sz":"5.0"},{"px":"144.75","sz":"6.0"}],
		[{"px":"145.10","sz":"1.5"},{"px":"145.15","sz":"2.5"},{"px":"145.20","sz":"3.5"},{"px":"145.25","sz":"4.5"},{"px":"145.30","sz":"5.5"}]
	]}}`)
	if err := hl.handleMessageWithDepth(l2, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	dSnap, ok := depth.ReadDepth(1003, ExchangeTradeXYZ)
	if !ok || dSnap.Bids[4].Price != 144.80 || dSnap.Asks[4].Price != 145.30 {
		t.Fatalf("unexpected HL top-5 depth: ok=%v %+v", ok, dSnap)
	}

	// 2. Fresher bbo channel advances matrix BBO ahead of l2Book.
	bboMsg := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726200,"bbo":[{"px":"145.02","sz":"2.2","n":1},{"px":"145.09","sz":"1.8","n":2}]}}`)
	if err := hl.handleMessageWithDepth(bboMsg, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	bSnap, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003)
	if !ok || bSnap.BidPrice != 145.02 || bSnap.AskPrice != 145.09 {
		t.Fatalf("bbo channel did not advance matrix: ok=%v %+v", ok, bSnap)
	}

	// 3. Delayed l2Book (t=1794712726100) must update depth WITHOUT regressing newer BBO (t=1794712726200).
	olderL2 := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726100,"levels":[
		[{"px":"145.00","sz":"1.0"},{"px":"144.96","sz":"9.0"}],
		[{"px":"145.10","sz":"1.5"},{"px":"145.16","sz":"8.0"}]
	]}}`)
	if err := hl.handleMessageWithDepth(olderL2, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	bSnapAfter, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003)
	if !ok || bSnapAfter.BidPrice != 145.02 {
		t.Fatalf("older l2Book regressed newer BBO: %+v", bSnapAfter)
	}
	dSnapAfter, ok := depth.ReadDepth(1003, ExchangeTradeXYZ)
	if !ok || dSnapAfter.Bids[1].Price != 144.96 || dSnapAfter.Asks[1].Size != 8.0 {
		t.Fatalf("newer l2Book did not update depth: ok=%v %+v", ok, dSnapAfter)
	}

	// 4. Null side in bbo invalidates matrix BBO safely without wiping depth.
	nullBbo := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726300,"bbo":[null,{"px":"145.10","sz":"1.0","n":1}]}}`)
	if err := hl.handleMessageWithDepth(nullBbo, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("null bbo side failed to invalidate matrix BBO")
	}

	// 5. Crossed l2Book invalidates both BBO and depth.
	crossedL2 := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726400,"levels":[
		[{"px":"145.20","sz":"1.0"}],
		[{"px":"145.10","sz":"1.0"}]
	]}}`)
	if err := hl.handleMessageWithDepth(crossedL2, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := depth.ReadDepth(1003, ExchangeTradeXYZ); ok {
		t.Fatal("crossed l2Book left stale depth readable")
	}
}

func TestHyperliquidSubscribesFastBookAndBBO(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("hl-sub-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	subs := make(chan map[string]any, 4)
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
			subs <- sub
		}
		<-r.Context().Done()
	}))
	defer srv.Close()

	wsURL := "ws" + strings.TrimPrefix(srv.URL, "http")
	SetSymbolRegistry(map[string]uint16{"NVDA": 1003})
	hl := NewHyperliquidWithDepth(config.ExchangeConfig{
		Enabled:    true,
		WSURL:      wsURL,
		Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
		FastBook:   true,
		BBOChannel: true,
	}, matrix, nil)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- hl.connect(ctx, make(map[hlTarget]int64), make(map[hlTarget]int64))
	}()

	gotFast := false
	gotBBO := false
	for range 2 {
		select {
		case sub := <-subs:
			req, _ := sub["subscription"].(map[string]any)
			if req["type"] == "l2Book" && req["fast"] == true && req["coin"] == "xyz:NVDA" {
				gotFast = true
			}
			if req["type"] == "bbo" && req["coin"] == "xyz:NVDA" {
				gotBBO = true
			}
		case <-ctx.Done():
			t.Fatal("timed out waiting for HL subscriptions")
		}
	}
	cancel()
	<-done
	if !gotFast || !gotBBO {
		t.Fatalf("expected both l2Book fast=true and bbo subscriptions, got fast=%v bbo=%v", gotFast, gotBBO)
	}
}
