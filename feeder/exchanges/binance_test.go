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
	"nhooyr.io/websocket"
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
