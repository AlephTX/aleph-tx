package exchanges

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/coder/websocket"
)

func TestBuildBinanceStreamURLUsesPublicCombinedStream(t *testing.T) {
	url, err := buildBinanceStreamURL("wss://fstream.binance.com/", map[string]string{
		"ETH": "ETHUSDT",
		"BTC": "BTCUSDT",
	})
	if err != nil {
		t.Fatal(err)
	}
	want := "wss://fstream.binance.com/public/stream?streams=btcusdt@bookTicker/ethusdt@bookTicker"
	if url != want {
		t.Fatalf("url = %q, want %q", url, want)
	}

	urlPub, err := buildBinanceStreamURL("wss://fstream.binance.com/public", map[string]string{"ETH": "ETHUSDT"})
	if err != nil {
		t.Fatal(err)
	}
	if urlPub != "wss://fstream.binance.com/public/stream?streams=ethusdt@bookTicker" {
		t.Fatalf("urlPub = %q", urlPub)
	}

	explicit := "wss://fstream.binance.com/public/ws/ethusdt@bookTicker"
	gotExplicit, err := buildBinanceStreamURL(explicit, map[string]string{"ETH": "ETHUSDT"})
	if err != nil || gotExplicit != explicit {
		t.Fatalf("explicit URL altered: %q, err=%v", gotExplicit, err)
	}

	if _, err := buildBinanceStreamURL("", map[string]string{"ETH": "ETHUSDT"}); err == nil {
		t.Fatal("accepted empty URL")
	}
	if _, err := buildBinanceStreamURL("wss://fstream.binance.com", nil); err == nil {
		t.Fatal("accepted empty symbol map")
	}
}

func TestBinanceProcessMessageValidatesSequenceStatusAndSourceClock(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("binance-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	SetSymbolRegistry(config.DefaultSymbols())
	bn := NewBinance(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://fstream.binance.com",
		Symbols: map[string]string{"ETH": "ETHUSDT", "BTC": "BTCUSDT"},
	}, matrix)

	lastUpdateID := make(map[uint16]uint64)
	nowMs := int64(1_794_712_726_900)

	// 1. Valid combined-stream bookTicker publishes pure source time T * 1e6.
	validFrame := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":100,"s":"ETHUSDT","b":"1825.44","B":"21.5","a":"1825.45","A":"18.0","T":1794712726856,"E":1794712726857,"st":1}}`)
	if err := bn.processMessage(validFrame, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	snap, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
	if !ok || snap.BidPrice != 1825.44 || snap.AskPrice != 1825.45 || snap.TimestampNs != 1_794_712_726_856_000_000 {
		t.Fatalf("unexpected BBO snapshot: ok=%v snap=%+v", ok, snap)
	}

	// 2. Duplicate/regressed updateId, missing T, T > E, crossed BBO, NaN/zero must NOT overwrite.
	for _, ignored := range [][]byte{
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":100,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726860,"E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":99,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726860,"E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":101,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":102,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726870,"E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":103,"s":"ETHUSDT","b":"1830.10","B":"1.0","a":"1830.10","A":"1.0","T":1794712726860,"E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":104,"s":"ETHUSDT","b":"NaN","B":"1.0","a":"1830.10","A":"1.0","T":1794712726860,"E":1794712726860,"st":1}}`),
		[]byte(`{"stream":"btcusdt@bookTicker","data":{"e":"bookTicker","u":105,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726860,"E":1794712726860,"st":1}}`),
	} {
		if err := bn.processMessage(ignored, nowMs, lastUpdateID); err != nil {
			t.Fatalf("unexpected fatal error on malformed frame: %v", err)
		}
		after, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
		if !ok || after.BidPrice != 1825.44 || after.TimestampNs != 1_794_712_726_856_000_000 {
			t.Fatalf("corrupt frame mutated SHM: %+v", after)
		}
	}

	// 3. Future timestamp > 1s triggers reconnect error.
	future := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":110,"s":"ETHUSDT","b":"1826.00","B":"1.0","a":"1826.10","A":"1.0","T":1794712729000,"E":1794712729000,"st":1}}`)
	if err := bn.processMessage(future, nowMs, lastUpdateID); err == nil {
		t.Fatal("future bookTicker timestamp did not return reconnect error")
	}

	// 4. Non-trading status st != 1 immediately invalidates SHM BBO.
	halted := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":111,"s":"ETHUSDT","b":"1826.00","B":"1.0","a":"1826.10","A":"1.0","T":1794712726880,"E":1794712726880,"st":0}}`)
	if err := bn.processMessage(halted, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); ok {
		t.Fatal("st != 1 failed to invalidate SHM BBO")
	}
}

func TestBinanceDisconnectInvalidatesSHMBBO(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("binance-ws")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	published := make(chan struct{})
	releaseClose := make(chan struct{})
	var requestedPath string

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		requestedPath = r.URL.RequestURI()
		c, err := websocket.Accept(w, r, nil)
		if err != nil {
			return
		}
		defer c.CloseNow()
		now := time.Now().UnixMilli()
		msg := `{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":501,"s":"ETHUSDT","b":"2000.10","B":"3.0","a":"2000.20","A":"4.0","T":` +
			itoa64(now) + `,"E":` + itoa64(now) + `,"st":1}}`
		_ = c.Write(r.Context(), websocket.MessageText, []byte(msg))
		close(published)
		<-releaseClose
		_ = c.Close(websocket.StatusNormalClosure, "bye")
	}))
	defer srv.Close()

	SetSymbolRegistry(config.DefaultSymbols())
	wsBase := "ws" + strings.TrimPrefix(srv.URL, "http")
	bn := NewBinance(config.ExchangeConfig{
		Enabled: true,
		WSURL:   wsBase,
		Symbols: map[string]string{"ETH": "ETHUSDT"},
	}, matrix)

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- bn.connect(ctx)
	}()

	<-published
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		if snap, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); ok && snap.BidPrice == 2000.10 {
			break
		}
		time.Sleep(5 * time.Millisecond)
	}
	if requestedPath != "/public/stream?streams=ethusdt@bookTicker" {
		t.Fatalf("unexpected WS request URI: %q", requestedPath)
	}
	if _, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); !ok {
		t.Fatal("expected live BBO before disconnect")
	}

	close(releaseClose)
	<-done
	if _, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); ok {
		t.Fatal("disconnect left stale Binance BBO in SHM")
	}
}

func TestBinanceParserEdgeCasesAndArbitraryKeyOrder(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("binance-edge")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(t, map[string]uint16{"ETH": SymbolETHPERP, "BTC": SymbolBTCPERP})
	bn := NewBinance(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://fstream.binance.com",
		Symbols: map[string]string{"ETH": "ETHUSDT", "BTC": "BTCUSDT"},
	}, matrix)

	lastUpdateID := make(map[uint16]uint64)
	nowMs := int64(1_794_712_726_900)

	// 1. Reordered outer keys ("data" before "stream"), reordered inner keys, extra nested field, and "st": null.
	reordered := []byte(`{"data":{"extra":{"nested":[1,{"k":"v"}]},"st":null,"A":"9.5","a":"1825.55","B":"7.5","b":"1825.50","E":1794712726860,"T":1794712726859,"s":"ETHUSDT","u":100,"e":"bookTicker"},"stream":"EthUsdt@bookTicker"}`)
	if err := bn.processMessage(reordered, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	snap, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
	if !ok || snap.BidPrice != 1825.50 || snap.AskPrice != 1825.55 || snap.TimestampNs != 1_794_712_726_859_000_000 {
		t.Fatalf("reordered combined frame failed: ok=%v snap=%+v", ok, snap)
	}

	// 2. Direct flat bookTicker JSON (no combined envelope) and missing "st".
	flat := []byte(`{"e":"bookTicker","u":101,"s":"ETHUSDT","b":"1825.60","B":"8.0","a":"1825.65","A":"6.0","T":1794712726861,"E":1794712726861}`)
	if err := bn.processMessage(flat, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	snap, ok = matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
	if !ok || snap.BidPrice != 1825.60 {
		t.Fatalf("flat bookTicker failed: ok=%v snap=%+v", ok, snap)
	}

	// 3. Escaped JSON string symbol and stream still match cleanly.
	escaped := []byte(`{"stream":"ethusdt\u0040bookTicker","data":{"e":"bookTicker","u":102,"s":"ETH\u0055SDT","b":"1825.70","B":"5.0","a":"1825.75","A":"5.0","T":1794712726862,"E":1794712726862,"st":1}}`)
	if err := bn.processMessage(escaped, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	snap, ok = matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
	if !ok || snap.BidPrice != 1825.70 {
		t.Fatalf("escaped JSON frame failed: ok=%v snap=%+v", ok, snap)
	}

	// 4. Ignored / malformed / wrong-type frames must not mutate SHM.
	for _, ignored := range [][]byte{
		[]byte(`{invalid`),
		[]byte(`{"stream":"ethusdt@depth","data":{"e":"bookTicker","u":200,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726863,"E":1794712726863}}`),
		[]byte(`{"stream":"ethusdt@BOOKTICKER","data":{"e":"bookTicker","u":201,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726863,"E":1794712726863}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":null}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":123}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":-1,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726863,"E":1794712726863}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":202,"s":"ETHUSDT","b":1830.00,"B":"1.0","a":"1830.10","A":"1.0","T":1794712726863,"E":1794712726863}}`),
		[]byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":203,"s":"ETHUSDT","b":"1830.00","B":"1.0","a":"1830.10","A":"1.0","T":1794712726863,"E":1794712726863,"st":"1"}}`),
	} {
		if err := bn.processMessage(ignored, nowMs, lastUpdateID); err != nil {
			t.Fatalf("unexpected error on ignored frame: %v", err)
		}
		after, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
		if !ok || after.BidPrice != 1825.70 {
			t.Fatalf("ignored frame mutated SHM: ok=%v snap=%+v", ok, after)
		}
	}

	// 5. Non-trading status st != 1 invalidates SHM and clears lastUpdateID[symID].
	halted := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":103,"s":"ETHUSDT","b":"1825.80","B":"1.0","a":"1825.90","A":"1.0","T":1794712726864,"E":1794712726864,"st":0}}`)
	if err := bn.processMessage(halted, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); ok {
		t.Fatal("st != 1 failed to invalidate SHM")
	}
	if _, exists := lastUpdateID[SymbolETHPERP]; exists {
		t.Fatal("st != 1 did not clear lastUpdateID[symID]")
	}
	// After halt cleared lastUpdateID, a resumed stream with restarted updateId succeeds.
	resumed := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":1,"s":"ETHUSDT","b":"1825.85","B":"2.0","a":"1825.95","A":"2.0","T":1794712726865,"E":1794712726865,"st":1}}`)
	if err := bn.processMessage(resumed, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	if snap, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP); !ok || snap.BidPrice != 1825.85 {
		t.Fatalf("resumed stream after halt failed: ok=%v snap=%+v", ok, snap)
	}
}

func TestBinanceProcessMessageZeroAlloc(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	matrix, err := shm.NewMatrix("binance-zero-alloc")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(t, map[string]uint16{"ETH": SymbolETHPERP, "BTC": SymbolBTCPERP})
	bn := NewBinance(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://fstream.binance.com",
		Symbols: map[string]string{"ETH": "ETHUSDT", "BTC": "BTCUSDT"},
	}, matrix)

	lastUpdateID := map[uint16]uint64{SymbolETHPERP: 0}
	nowMs := int64(1_794_712_726_900)
	frame := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":100,"s":"ETHUSDT","b":"1825.44","B":"21.5","a":"1825.45","A":"18.0","T":1794712726856,"E":1794712726857,"st":1}}`)

	if err := bn.processMessage(frame, nowMs, lastUpdateID); err != nil {
		t.Fatal(err)
	}
	allocs := testing.AllocsPerRun(1000, func() {
		lastUpdateID[SymbolETHPERP] = 0
		if err := bn.processMessage(frame, nowMs, lastUpdateID); err != nil {
			t.Fatal(err)
		}
	})
	if allocs != 0 {
		t.Fatalf("expected 0 allocations in Binance processMessage, got %v", allocs)
	}
	snap, ok := matrix.ReadBBO(ExchangeBinance, SymbolETHPERP)
	if !ok || snap.BidPrice != 1825.44 || snap.AskPrice != 1825.45 {
		t.Fatalf("unexpected BBO snapshot: ok=%v snap=%+v", ok, snap)
	}
}

func BenchmarkBinanceProcessMessage(b *testing.B) {
	b.Setenv("ALEPH_SHM_DIR", b.TempDir())
	matrix, err := shm.NewMatrix("binance-bench")
	if err != nil {
		b.Fatal(err)
	}
	defer matrix.Close()

	withTestSymbolRegistry(b, map[string]uint16{"ETH": SymbolETHPERP, "BTC": SymbolBTCPERP})
	bn := NewBinance(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://fstream.binance.com",
		Symbols: map[string]string{"ETH": "ETHUSDT", "BTC": "BTCUSDT"},
	}, matrix)

	lastUpdateID := map[uint16]uint64{SymbolETHPERP: 0}
	nowMs := int64(1_794_712_726_900)
	frame := []byte(`{"stream":"ethusdt@bookTicker","data":{"e":"bookTicker","u":100,"s":"ETHUSDT","b":"1825.44","B":"21.5","a":"1825.45","A":"18.0","T":1794712726856,"E":1794712726857,"st":1}}`)

	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		lastUpdateID[SymbolETHPERP] = 0
		if err := bn.processMessage(frame, nowMs, lastUpdateID); err != nil {
			b.Fatal(err)
		}
	}
}

func itoa64(v int64) string {
	var buf [32]byte
	i := len(buf)
	for v > 0 {
		i--
		buf[i] = byte('0' + v%10)
		v /= 10
	}
	return string(buf[i:])
}
