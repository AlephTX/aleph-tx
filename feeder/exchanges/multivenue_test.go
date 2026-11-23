package exchanges

import (
	"context"
	"fmt"
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

	for _, tc := range []struct {
		name     string
		levels   string
		badTime  string
		nextTime string
	}{
		{"one-sided", `[[],[{"px":"145.10","sz":"1.0"}]]`, "1794712726150", "1794712726160"},
		{"crossed", `[[{"px":"145.20","sz":"1.0"}],[{"px":"145.10","sz":"1.0"}]]`, "1794712726170", "1794712726180"},
	} {
		t.Run(tc.name+" delayed L2", func(t *testing.T) {
			invalid := fmt.Sprintf(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":%s,"levels":%s}}`, tc.badTime, tc.levels)
			if err := hl.handleMessageWithDepth([]byte(invalid), now, lastBbo, lastDepth); err != nil {
				t.Fatal(err)
			}
			if snap, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); !ok || snap.TimestampNs != bSnap.TimestampNs {
				t.Fatal("delayed invalid L2 erased the newer BBO")
			}
			if err := hl.handleMessageWithDepth(olderL2, now, lastBbo, lastDepth); err != nil {
				t.Fatal(err)
			}
			if _, ok := depth.ReadDepth(1003, ExchangeTradeXYZ); ok {
				t.Fatal("invalid L2 did not clear depth or older L2 restored it")
			}
			fresh := strings.Replace(string(olderL2), "1794712726100", tc.nextTime, 1)
			if err := hl.handleMessageWithDepth([]byte(fresh), now, lastBbo, lastDepth); err != nil {
				t.Fatal(err)
			}
			if _, ok := depth.ReadDepth(1003, ExchangeTradeXYZ); !ok {
				t.Fatal("valid newer L2 did not restore depth")
			}
		})
	}

	// 4. Null side or crossed BBO in bbo channel invalidates matrix BBO safely without wiping depth.
	nullBbo := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726300,"bbo":[null,{"px":"145.10","sz":"1.0","n":1}]}}`)
	if err := hl.handleMessageWithDepth(nullBbo, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("null bbo side failed to invalidate matrix BBO")
	}
	if _, ok := depth.ReadDepth(1003, ExchangeTradeXYZ); !ok {
		t.Fatal("null bbo side unexpectedly wiped valid l2Book depth")
	}
	crossedBbo := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726350,"bbo":[{"px":"145.20","sz":"1.0","n":1},{"px":"145.10","sz":"1.0","n":1}]}}`)
	if err := hl.handleMessageWithDepth(crossedBbo, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("crossed bbo failed to invalidate matrix BBO")
	}
	if _, ok := depth.ReadDepth(1003, ExchangeTradeXYZ); !ok {
		t.Fatal("crossed bbo unexpectedly wiped valid l2Book depth")
	}

	// 5. Crossed l2Book invalidates both BBO and depth.
	if err := hl.handleMessageWithDepth([]byte(strings.Replace(string(bboMsg), "1794712726200", "1794712726375", 1)), now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); !ok {
		t.Fatal("valid BBO did not restore the matrix")
	}
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
	if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003); ok {
		t.Fatal("crossed l2Book left older BBO readable")
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

func TestHyperliquidGroupIsolatesFastBookAndBBOBySection(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("hl-group-iso-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	subs := make(chan map[string]any, 8)
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		for range 3 {
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
	SetSymbolRegistry(map[string]uint16{"ETH": 0, "NVDA": 1003})
	hl := NewHyperliquidGroupWithDepth(wsURL, []config.ExchangeConfig{
		{
			Enabled:    true,
			WSURL:      wsURL,
			Symbols:    map[string]string{"ETH": "ETH"},
			FastBook:   false,
			BBOChannel: false,
		},
		{
			Enabled:    true,
			WSURL:      wsURL,
			Symbols:    map[string]string{"NVDA": "xyz:NVDA"},
			FastBook:   true,
			BBOChannel: true,
		},
	}, matrix, nil)

	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- hl.connect(ctx, make(map[hlTarget]int64), make(map[hlTarget]int64))
	}()

	var ethL2, nvdaFastL2, nvdaBBO int
	var ethUnexpectedFast, ethUnexpectedBBO bool
	for range 3 {
		select {
		case sub := <-subs:
			req, _ := sub["subscription"].(map[string]any)
			switch req["coin"] {
			case "ETH":
				if req["type"] == "l2Book" {
					ethL2++
					if fast, ok := req["fast"].(bool); ok && fast {
						ethUnexpectedFast = true
					}
				} else if req["type"] == "bbo" {
					ethUnexpectedBBO = true
				}
			case "xyz:NVDA":
				if req["type"] == "l2Book" && req["fast"] == true {
					nvdaFastL2++
				} else if req["type"] == "bbo" {
					nvdaBBO++
				}
			}
		case <-ctx.Done():
			t.Fatal("timed out waiting for grouped HL subscriptions")
		}
	}
	cancel()
	<-done

	if ethL2 != 1 || nvdaFastL2 != 1 || nvdaBBO != 1 || ethUnexpectedFast || ethUnexpectedBBO {
		t.Fatalf("unexpected per-section subscription isolation: ethL2=%d nvdaFastL2=%d nvdaBBO=%d ethUnexpectedFast=%v ethUnexpectedBBO=%v",
			ethL2, nvdaFastL2, nvdaBBO, ethUnexpectedFast, ethUnexpectedBBO)
	}
}

func TestHyperliquidSurfacesServerErrorFrameWithoutDroppingHealthyBooks(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	tmp := t.TempDir()
	matrix, err := shm.NewMatrix("hl-err-unit")
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

	l2 := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726000,"levels":[
		[{"px":"145.00","sz":"1.0"}],
		[{"px":"145.10","sz":"1.5"}]
	]}}`)
	if err := hl.handleMessageWithDepth(l2, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}

	errFrame := []byte(`{"channel":"error","data":"Invalid subscription: io:DELISTED"}`)
	if err := hl.handleMessageWithDepth(errFrame, now, lastBbo, lastDepth); err != nil {
		t.Fatalf("expected server error frame to be surfaced without crash-looping, got %v", err)
	}
	if hl.ErrorFrames() != 1 {
		t.Fatalf("expected ErrorFrames() == 1, got %d", hl.ErrorFrames())
	}

	bboMsg := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726200,"bbo":[{"px":"145.02","sz":"2.2","n":1},{"px":"145.09","sz":"1.8","n":2}]}}`)
	if err := hl.handleMessageWithDepth(bboMsg, now, lastBbo, lastDepth); err != nil {
		t.Fatal(err)
	}
	bSnap, ok := matrix.ReadBBO(ExchangeTradeXYZ, 1003)
	if !ok || bSnap.BidPrice != 145.02 || bSnap.AskPrice != 145.09 {
		t.Fatalf("healthy BBO was dropped or not updated after server error frame: ok=%v %+v", ok, bSnap)
	}
	dSnap, ok := depth.ReadDepth(1003, ExchangeTradeXYZ)
	if !ok || dSnap.Bids[0].Price != 145.00 || dSnap.Asks[0].Price != 145.10 {
		t.Fatalf("healthy depth was dropped after server error frame: ok=%v %+v", ok, dSnap)
	}
}

func TestHyperliquidSameTimestampInvalidationIsOrderIndependent(t *testing.T) {
	SetSymbolRegistry(map[string]uint16{"NVDA": 1003})
	const symbolID = uint16(1003)
	now := time.UnixMilli(1_794_712_726_010)

	l2Valid := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726000,"levels":[[{"px":"145.00","sz":"1.0"}],[{"px":"145.10","sz":"1.5"}]]}}`)
	l2Crossed := []byte(`{"channel":"l2Book","data":{"coin":"xyz:NVDA","time":1794712726000,"levels":[[{"px":"145.20","sz":"1.0"}],[{"px":"145.10","sz":"1.5"}]]}}`)
	bboValid := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726000,"bbo":[{"px":"145.00","sz":"1.0"},{"px":"145.10","sz":"1.5"}]}}`)
	bboNull := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726000,"bbo":[null,{"px":"145.10","sz":"1.5"}]}}`)
	bboCrossed := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726000,"bbo":[{"px":"145.20","sz":"1.0"},{"px":"145.10","sz":"1.5"}]}}`)
	bboOlderNull := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712725999,"bbo":[null,{"px":"145.10","sz":"1.5"}]}}`)
	bboNewer := []byte(`{"channel":"bbo","data":{"coin":"xyz:NVDA","time":1794712726001,"bbo":[{"px":"145.01","sz":"1.2"},{"px":"145.09","sz":"1.4"}]}}`)

	cases := []struct {
		name      string
		frames    [][]byte
		wantDepth bool
	}{
		{"valid L2 then null BBO", [][]byte{l2Valid, bboNull}, true},
		{"null BBO then valid L2", [][]byte{bboNull, l2Valid}, true},
		{"valid L2 then crossed BBO", [][]byte{l2Valid, bboCrossed}, true},
		{"valid BBO then crossed L2", [][]byte{bboValid, l2Crossed}, false},
		{"crossed L2 then valid BBO", [][]byte{l2Crossed, bboValid}, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			tmp := t.TempDir()
			t.Setenv("ALEPH_SHM_DIR", tmp)
			matrix, err := shm.NewMatrix("hl-same-ms-order")
			if err != nil {
				t.Fatal(err)
			}
			defer matrix.Close()
			depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
			if err != nil {
				t.Fatal(err)
			}
			defer depth.Close()

			hl := NewHyperliquidWithDepth(config.ExchangeConfig{
				Enabled: true, Symbols: map[string]string{"NVDA": "xyz:NVDA"},
				FastBook: true, BBOChannel: true,
			}, matrix, depth)
			lastBBO := map[hlTarget]int64{}
			lastDepth := map[hlTarget]int64{}

			for i, frame := range tc.frames {
				if err := hl.handleMessageWithDepth(frame, now.Add(time.Duration(i)*time.Millisecond), lastBBO, lastDepth); err != nil {
					t.Fatal(err)
				}
			}
			if _, ok := matrix.ReadBBO(ExchangeTradeXYZ, symbolID); ok {
				t.Fatalf("expected fail-closed BBO invalidation on equal-timestamp mismatch")
			}
			_, gotDepth := depth.ReadDepth(symbolID, ExchangeTradeXYZ)
			if gotDepth != tc.wantDepth {
				t.Fatalf("unexpected depth readable=%v, want %v", gotDepth, tc.wantDepth)
			}

			// A strictly newer (+1ms) valid BBO recovers Matrix BBO cleanly.
			if err := hl.handleMessageWithDepth(bboNewer, now.Add(2*time.Millisecond), lastBBO, lastDepth); err != nil {
				t.Fatal(err)
			}
			if snap, ok := matrix.ReadBBO(ExchangeTradeXYZ, symbolID); !ok || snap.BidPrice != 145.01 {
				t.Fatalf("expected +1ms recovery BBO, got ok=%v %+v", ok, snap)
			}

			// A strictly older (-1ms) null BBO must never erase the newer valid BBO.
			if err := hl.handleMessageWithDepth(bboOlderNull, now.Add(3*time.Millisecond), lastBBO, lastDepth); err != nil {
				t.Fatal(err)
			}
			if snap, ok := matrix.ReadBBO(ExchangeTradeXYZ, symbolID); !ok || snap.BidPrice != 145.01 {
				t.Fatalf("older null BBO erased newer BBO: ok=%v %+v", ok, snap)
			}
		})
	}
}
