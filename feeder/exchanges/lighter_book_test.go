package exchanges

import (
	"context"
	"fmt"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/tidwall/gjson"
)

func TestLighterBookReconstructsDeltasAndDeletes(t *testing.T) {
	var book lighterBook
	snapshot := gjson.Parse(`{"offset":10,"bids":[{"price":"99","size":"2"},{"price":"100","size":"1"}],"asks":[{"price":"102","size":"2"},{"price":"101","size":"1"}]}`)
	if _, err := book.apply(snapshot, true); err != nil {
		t.Fatal(err)
	}
	bids, asks := book.depth()
	if bids[0].Price != 100 || asks[0].Price != 101 {
		t.Fatal("snapshot not sorted")
	}
	delta := gjson.Parse(`{"offset":11,"bids":[{"price":"100","size":"0"},{"price":"98","size":"4"}],"asks":[]}`)
	if _, err := book.apply(delta, false); err != nil {
		t.Fatal(err)
	}
	bids, asks = book.depth()
	if bids[0].Price != 99 || bids[1].Price != 98 || asks[0].Price != 101 {
		t.Fatalf("delta lost unchanged levels: %v %v", bids, asks)
	}
	if changed, err := book.apply(snapshot, false); err != nil || changed {
		t.Fatal("old offset reapplied")
	}
	if _, err := book.apply(gjson.Parse(`{"bids":[],"asks":[{"price":"103","size":"1"}]}`), true); err != nil {
		t.Fatal(err)
	}
	bids, asks = book.depth()
	if bids[0].Size != 0 || asks[0].Price != 103 {
		t.Fatal("reconnect snapshot retained old levels")
	}
}

func TestLighterBookRejectsInvalidWithoutPartialMutation(t *testing.T) {
	for _, raw := range []string{
		`{"bids":[{"price":"NaN","size":"1"}],"asks":[]}`,
		`{"bids":[{"price":"100","size":"-1"}],"asks":[]}`,
		`{"bids":[{"price":"100","size":"2"}],"asks":[{"price":"101","size":"bad"}]}`,
	} {
		var book lighterBook
		if _, err := book.apply(gjson.Parse(`{"bids":[{"price":"100","size":"1"}],"asks":[{"price":"101","size":"1"}]}`), true); err != nil {
			t.Fatal(err)
		}
		if _, err := book.apply(gjson.Parse(raw), false); err == nil {
			t.Fatalf("accepted %s", raw)
		}
		bids, _ := book.depth()
		if bids[0].Size != 1 {
			t.Fatal("invalid frame partially applied")
		}
	}
	var empty lighterBook
	if _, err := empty.apply(gjson.Parse(`{"bids":[],"asks":[]}`), false); err == nil {
		t.Fatal("delta accepted before snapshot")
	}
}

func TestLighterBookCrossedAndChannelValidation(t *testing.T) {
	var book lighterBook
	if _, err := book.apply(gjson.Parse(`{"bids":[{"price":"102","size":"1"}],"asks":[{"price":"101","size":"1"}]}`), true); err == nil {
		t.Fatal("crossed book accepted")
	}
	l := &Lighter{}
	for _, channel := range []string{"order_book:bad", "order_book/-1", "order_book:"} {
		if got := l.parseMarketIndex(channel); got != -1 {
			t.Fatalf("invalid channel mapped to %d", got)
		}
	}
}

func TestLighterBookNonceContinuityAndTimestampGuard(t *testing.T) {
	var book lighterBook
	// Production subscribed/order_book snapshots emit begin_nonce = 0.
	snap := gjson.Parse(`{"nonce":100,"begin_nonce":0,"last_updated_at":1700000000000000,"bids":[{"price":"100","size":"2"}],"asks":[{"price":"101","size":"3"}]}`)
	if changed, err := book.apply(snap, true); err != nil || !changed {
		t.Fatalf("snapshot failed: changed=%v err=%v", changed, err)
	}
	// Contiguous delta (begin_nonce == previous nonce) succeeds.
	delta1 := gjson.Parse(`{"nonce":105,"begin_nonce":100,"last_updated_at":1700000000001000,"bids":[{"price":"99.5","size":"4"}],"asks":[]}`)
	if changed, err := book.apply(delta1, false); err != nil || !changed {
		t.Fatalf("contiguous delta failed: changed=%v err=%v", changed, err)
	}
	// begin_nonce must match prior nonce even if nonce increments sequentially (+1).
	var stepCheck lighterBook
	if _, err := stepCheck.apply(snap, true); err != nil {
		t.Fatalf("stepCheck snapshot failed: %v", err)
	}
	if _, err := stepCheck.apply(gjson.Parse(`{"nonce":101,"begin_nonce":99,"last_updated_at":1700000000001000,"bids":[],"asks":[]}`), false); err == nil {
		t.Fatal("accepted begin_nonce=99, nonce=101 over prior nonce=100")
	}
	// Duplicate / older nonce is ignored without rolling back or erroring.
	if changed, err := book.apply(delta1, false); err != nil || changed {
		t.Fatalf("duplicate nonce reapplied: changed=%v err=%v", changed, err)
	}
	// Sequence gap (begin_nonce != 105) fails closed and invalidates initialized state.
	gapDelta := gjson.Parse(`{"nonce":108,"begin_nonce":106,"last_updated_at":1700000000002000,"bids":[{"price":"99","size":"1"}],"asks":[]}`)
	if _, err := book.apply(gapDelta, false); err == nil {
		t.Fatal("nonce gap accepted")
	}
	if book.initialized {
		t.Fatal("nonce gap left book initialized")
	}
	// Subsequent delta before fresh snapshot must be rejected.
	if _, err := book.apply(gjson.Parse(`{"nonce":109,"begin_nonce":108,"bids":[],"asks":[]}`), false); err == nil {
		t.Fatal("delta accepted after gap without snapshot")
	}
	// Fresh snapshot recovers cleanly; timestamp regression afterwards fails closed.
	if _, err := book.apply(snap, true); err != nil {
		t.Fatalf("recovery snapshot failed: %v", err)
	}
	regressed := gjson.Parse(`{"nonce":101,"begin_nonce":100,"last_updated_at":1699999999999999,"bids":[],"asks":[]}`)
	if _, err := book.apply(regressed, false); err == nil || book.initialized {
		t.Fatalf("timestamp regression accepted: err=%v initialized=%v", err, book.initialized)
	}
}

func TestConnectionLoopDoesNotDialAfterCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	called := false
	err := RunConnectionLoop(ctx, "test", func(context.Context) error { called = true; return nil })
	if called || err != context.Canceled {
		t.Fatalf("called=%v err=%v", called, err)
	}
}

func TestLighterAuthRejectsReservedKey157OnRobinhoodChain(t *testing.T) {
	t.Setenv("LIGHTER_PRIVATE_KEY", "01010101010101010101010101010101010101010101010101010101010101010101010101010101")
	t.Setenv("LIGHTER_ACCOUNT_INDEX", "42")
	t.Setenv("LIGHTER_API_KEY_INDEX", "157")
	t.Setenv("LIGHTER_CHAIN_ID", "466324")
	if _, err := LoadLighterAuthFromEnv(); err == nil {
		t.Fatal("accepted reserved API key index 157 on Robinhood Chain 466324")
	}
	t.Setenv("LIGHTER_API_KEY_INDEX", "7")
	if _, err := LoadLighterAuthFromEnv(); err != nil {
		t.Fatalf("rejected valid Robinhood API key index 7: %v", err)
	}
}

func TestLighterAuthCreateAuthTokenConcurrentCacheAndNoKeyLeak(t *testing.T) {
	sentinel := "zz-secret-sentinel-key-do-not-leak-0123456789"
	t.Setenv("LIGHTER_PRIVATE_KEY", sentinel)
	t.Setenv("LIGHTER_ACCOUNT_INDEX", "42")
	t.Setenv("LIGHTER_API_KEY_INDEX", "7")
	if _, err := LoadLighterAuthFromEnv(); err == nil {
		t.Fatal("expected invalid private key error")
	} else if strings.Contains(err.Error(), sentinel) {
		t.Fatalf("private key leaked into error: %v", err)
	}

	t.Setenv("LIGHTER_PRIVATE_KEY", strings.Repeat("01", 40))
	t.Setenv("LIGHTER_CHAIN_ID", "466324")
	auth, err := LoadLighterAuthFromEnv()
	if err != nil {
		t.Fatalf("failed to load synthetic Lighter auth: %v", err)
	}

	const workers = 8
	tokens := make(chan string, workers*4)
	errs := make(chan error, workers*4)
	var wg sync.WaitGroup
	for i := 0; i < workers; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			for j := 0; j < 4; j++ {
				tok, err := auth.CreateAuthToken()
				if err != nil {
					errs <- err
					return
				}
				tokens <- tok
			}
		}()
	}
	wg.Wait()
	close(tokens)
	close(errs)

	for err := range errs {
		t.Fatalf("CreateAuthToken returned error: %v", err)
	}
	var first string
	count := 0
	for tok := range tokens {
		if !strings.Contains(tok, ":42:7:") {
			t.Fatalf("unexpected auth token format: %q", tok)
		}
		if first == "" {
			first = tok
		} else if tok != first {
			t.Fatalf("expected cached token reuse, got %q != %q", tok, first)
		}
		count++
	}
	if count != workers*4 {
		t.Fatalf("expected %d tokens, got %d", workers*4, count)
	}
}

func BenchmarkLighterBookDelta(b *testing.B) {
	var book lighterBook
	levels := `{"bids":[`
	for i := 0; i < 1000; i++ {
		if i > 0 {
			levels += ","
		}
		levels += fmt.Sprintf(`{"price":"%d","size":"1"}`, 10000-i)
	}
	levels += `],"asks":[{"price":"10001","size":"1"}]}`
	if _, err := book.apply(gjson.Parse(levels), true); err != nil {
		b.Fatal(err)
	}
	delta := gjson.Parse(`{"bids":[{"price":"10000","size":"2"}],"asks":[]}`)
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		if _, err := book.apply(delta, false); err != nil {
			b.Fatal(err)
		}
		book.depth()
	}
}

func buildLighterSnapshotFrame(levels int, nonce, offset, lastUpdatedAtUs, tsMs int64) []byte {
	var b strings.Builder
	fmt.Fprintf(&b, `{"type":"subscribed/order_book","channel":"order_book:0","timestamp":%d,"order_book":{"nonce":%d,"begin_nonce":0,"offset":%d,"last_updated_at":%d,"bids":[`,
		tsMs, nonce, offset, lastUpdatedAtUs)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"price":"%d","size":"1.5"}`, 10000-i)
	}
	b.WriteString(`],"asks":[`)
	for i := 0; i < levels; i++ {
		if i > 0 {
			b.WriteByte(',')
		}
		fmt.Fprintf(&b, `{"price":"%d","size":"2.0"}`, 10001+i)
	}
	b.WriteString(`]}}`)
	return []byte(b.String())
}

func TestLighterProcessOrderBookFrameAndObserveFields(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("lighter-frame-unit")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	withTestSymbolRegistry(t, map[string]uint16{"ETH": SymbolETHPERP})
	l := NewLighter(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://mainnet.zklighter.elliot.ai/stream",
		Symbols: map[string]string{"ETH": "0"},
	}, matrix, nil, depth)

	guard := newSubscriptionGuard(context.Background(), "order_book", []string{"order_book/0"}, time.Second)
	defer guard.cancel()
	books := map[int]*lighterBook{0: {}}
	scratch := newLighterFrameScratch()
	nowMs := int64(1_794_712_726_900)

	// 1. Ping frame does not confirm subscription and requests pong.
	sendPong, err := l.processOrderBookFrame([]byte(`{"type":"ping"}`), nowMs, guard, books, &scratch)
	if err != nil || !sendPong {
		t.Fatalf("ping failed: sendPong=%v err=%v", sendPong, err)
	}
	if guard.pending != 1 {
		t.Fatalf("ping mutated pending count: %d", guard.pending)
	}

	// 2. Delta before snapshot fails closed.
	if _, err := l.processOrderBookFrame([]byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726000,"order_book":{"nonce":101,"begin_nonce":100,"bids":[],"asks":[]}}`), nowMs, guard, books, &scratch); err == nil {
		t.Fatal("accepted delta before snapshot")
	}

	// 3. Reordered snapshot with "error": null confirms subscription (colon channel) and populates SHM.
	snapReordered := []byte(`{"order_book":{"asks":[{"size":"3.0","price":"10001"}],"bids":[{"size":"2.0","price":"10000"}],"last_updated_at":1794712726000000,"offset":10,"begin_nonce":0,"nonce":100},"error":null,"channel":"order_book:0","type":"subscribed/order_book"}`)
	sendPong, err = l.processOrderBookFrame(snapReordered, nowMs, guard, books, &scratch)
	if err != nil || sendPong {
		t.Fatalf("reordered snapshot failed: sendPong=%v err=%v", sendPong, err)
	}
	if guard.pending != 0 {
		t.Fatalf("expected subscription confirmed, got pending=%d", guard.pending)
	}
	bbo, ok := matrix.ReadBBO(ExchangeLighter, SymbolETHPERP)
	if !ok || bbo.BidPrice != 10000 || bbo.AskPrice != 10001 || bbo.TimestampNs != 1_794_712_726_000_000_000 {
		t.Fatalf("unexpected BBO after snapshot: ok=%v bbo=%+v", ok, bbo)
	}

	// 4. Invalid delta level must NOT partially mutate book.
	if _, err := l.processOrderBookFrame([]byte(`{"type":"update/order_book","channel":"order_book/0","timestamp":1794712726100,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712726100000,"bids":[{"price":"10000","size":"99"}],"asks":[{"price":"10001","size":"bad"}]}}`), nowMs, guard, books, &scratch); err == nil {
		t.Fatal("accepted invalid ask size")
	}
	if gotBids, _ := books[0].depth(); gotBids[0].Size != 2.0 {
		t.Fatalf("invalid frame partially mutated book: %+v", gotBids[0])
	}

	// 5. Contiguous delta updates book; duplicate nonce is skipped without error or mutation.
	if sendPong, err := l.processOrderBookFrame([]byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726100,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712726100000,"bids":[{"price":"10000","size":"2.5"}],"asks":[]}}`), nowMs, guard, books, &scratch); err != nil || sendPong {
		t.Fatalf("contiguous delta failed: sendPong=%v err=%v", sendPong, err)
	}
	if gotBids, _ := books[0].depth(); gotBids[0].Size != 2.5 {
		t.Fatalf("contiguous delta did not update book: %+v", gotBids[0])
	}
	if sendPong, err := l.processOrderBookFrame([]byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726100,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712726100000,"bids":[{"price":"10000","size":"99"}],"asks":[]}}`), nowMs, guard, books, &scratch); err != nil || sendPong {
		t.Fatalf("duplicate nonce failed: sendPong=%v err=%v", sendPong, err)
	}
	if gotBids, _ := books[0].depth(); gotBids[0].Size != 2.5 {
		t.Fatalf("duplicate frame mutated book: %+v", gotBids[0])
	}

	// 6. Empty side invalidates SHM BBO & depth cleanly.
	if _, err := l.processOrderBookFrame([]byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726150,"order_book":{"nonce":102,"begin_nonce":101,"offset":12,"last_updated_at":1794712726150000,"bids":[{"price":"10000","size":"0"}],"asks":[]}}`), nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}
	if _, ok := matrix.ReadBBO(ExchangeLighter, SymbolETHPERP); ok {
		t.Fatal("empty bids left stale BBO in SHM")
	}
	if _, ok := depth.ReadDepth(SymbolETHPERP, ExchangeLighter); ok {
		t.Fatal("empty bids left stale depth in SHM")
	}

	// 7. Re-snapshot, then future timestamp triggers reconnect error.
	if _, err := l.processOrderBookFrame(snapReordered, nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}
	futureFrame := []byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712728500,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712728500000,"bids":[{"price":"10000","size":"2.5"}],"asks":[]}}`)
	if _, err := l.processOrderBookFrame(futureFrame, nowMs, guard, books, &scratch); err == nil {
		t.Fatal("future timestamp did not fail closed")
	}

	// 8. Error frame triggers reconnect even after subscription confirmation.
	if _, err := l.processOrderBookFrame([]byte(`{"error":{"code":403,"message":"denied"}}`), nowMs, guard, books, &scratch); err == nil {
		t.Fatal("non-null error field did not fail closed")
	}
	if _, err := l.processOrderBookFrame([]byte(`{"type":"error","code":401}`), nowMs, guard, books, &scratch); err == nil {
		t.Fatal("error type frame did not fail closed")
	}

	// 9. Handshake observeFields with colon vs slash matching is zero-alloc.
	gHandshake := newSubscriptionGuard(context.Background(), "order_book", []string{"order_book/0"}, time.Second)
	defer gHandshake.cancel()
	kindBytes := []byte("subscribed/order_book")
	chanBytes := []byte("order_book:0")
	handshakeAllocs := testing.AllocsPerRun(1000, func() {
		gHandshake.pending = 1
		gHandshake.channels["order_book/0"] = false
		if err := gHandshake.observeFields(kindBytes, chanBytes, false, nil); err != nil {
			t.Fatal(err)
		}
	})
	if handshakeAllocs != 0 {
		t.Fatalf("expected 0 allocations in observeFields handshake, got %v", handshakeAllocs)
	}

	// 10. Fallback to last_updated_at/1000 when outer timestamp is absent; error when both missing.
	var fbBook lighterBook
	fbBooks := map[int]*lighterBook{0: &fbBook}
	fbGuard := newSubscriptionGuard(context.Background(), "order_book", []string{"order_book/0"}, time.Second)
	defer fbGuard.cancel()
	fbSnap := []byte(`{"type":"subscribed/order_book","channel":"order_book:0","order_book":{"nonce":10,"begin_nonce":0,"offset":1,"last_updated_at":1794712726222000,"bids":[{"price":"10000","size":"1.0"}],"asks":[{"price":"10001","size":"1.0"}]}}`)
	if _, err := l.processOrderBookFrame(fbSnap, nowMs, fbGuard, fbBooks, &scratch); err != nil {
		t.Fatalf("last_updated_at fallback failed: %v", err)
	}
	if bbo, ok := matrix.ReadBBO(ExchangeLighter, SymbolETHPERP); !ok || bbo.TimestampNs != 1_794_712_726_222_000_000 {
		t.Fatalf("unexpected fallback timestamp: ok=%v bbo=%+v", ok, bbo)
	}
	noTsSnap := []byte(`{"type":"subscribed/order_book","channel":"order_book:0","order_book":{"nonce":11,"begin_nonce":0,"offset":2,"bids":[{"price":"10000","size":"1.0"}],"asks":[{"price":"10001","size":"1.0"}]}}`)
	if _, err := l.processOrderBookFrame(noTsSnap, nowMs, fbGuard, fbBooks, &scratch); err == nil {
		t.Fatal("missing both timestamp and last_updated_at did not error")
	}
	if _, ok := matrix.ReadBBO(ExchangeLighter, SymbolETHPERP); ok {
		t.Fatal("missing timestamp failed to invalidate SHM")
	}
}

func TestLighterApplyParsedParityAndGuards(t *testing.T) {
	var book lighterBook

	// Delta before snapshot rejected.
	if _, err := book.applyParsed(&lighterParsedBook{
		hasBids: true,
		hasAsks: true,
	}, false); err == nil {
		t.Fatal("delta accepted before snapshot")
	}

	// Snapshot with nonce=100, begin_nonce=0 succeeds.
	if _, err := book.applyParsed(&lighterParsedBook{
		meta: lighterBookMeta{
			hasNonce: true, nonce: 100, nonceRaw: []byte("100"),
			hasBeginNonce: true, beginNonce: 0, beginNonceRaw: []byte("0"),
			hasLastUpdatedAt: true, lastUpdatedAt: 1_700_000_000_000_000,
		},
		hasBids: true,
		hasAsks: true,
		bids:    []shm.PriceLevel{{Price: 100, Size: 2}},
		asks:    []shm.PriceLevel{{Price: 101, Size: 3}},
	}, true); err != nil {
		t.Fatal(err)
	}

	// Invalid level in applyParsed does not partially mutate book.
	if _, err := book.applyParsed(&lighterParsedBook{
		meta: lighterBookMeta{
			hasNonce: true, nonce: 101, nonceRaw: []byte("101"),
			hasBeginNonce: true, beginNonce: 100, beginNonceRaw: []byte("100"),
			hasLastUpdatedAt: true, lastUpdatedAt: 1_700_000_000_001_000,
		},
		hasBids: true,
		hasAsks: true,
		bids:    []shm.PriceLevel{{Price: 100, Size: 9}},
		asks:    []shm.PriceLevel{{Price: 101, Size: -1}},
	}, false); err == nil {
		t.Fatal("accepted negative ask size")
	}
	if bids, _ := book.depth(); bids[0].Size != 2 {
		t.Fatalf("invalid applyParsed partially mutated book: %+v", bids[0])
	}

	// Nonce gap resets book.initialized.
	if _, err := book.applyParsed(&lighterParsedBook{
		meta: lighterBookMeta{
			hasNonce: true, nonce: 105, nonceRaw: []byte("105"),
			hasBeginNonce: true, beginNonce: 102, beginNonceRaw: []byte("102"),
		},
		hasBids: true,
		hasAsks: true,
	}, false); err == nil || book.initialized {
		t.Fatalf("nonce gap failed to reset book: err=%v initialized=%v", err, book.initialized)
	}

	// Crossed book resets book.initialized.
	if _, err := book.applyParsed(&lighterParsedBook{
		hasBids: true,
		hasAsks: true,
		bids:    []shm.PriceLevel{{Price: 102, Size: 1}},
		asks:    []shm.PriceLevel{{Price: 101, Size: 1}},
	}, true); err == nil || book.initialized {
		t.Fatalf("crossed book failed to reset: err=%v initialized=%v", err, book.initialized)
	}

	// Max levels cap resets book.initialized.
	tooMany := make([]shm.PriceLevel, maxLighterBookLevels+1)
	for i := range tooMany {
		tooMany[i] = shm.PriceLevel{Price: float64(maxLighterBookLevels + 1 - i), Size: 1}
	}
	if _, err := book.applyParsed(&lighterParsedBook{
		hasBids: true,
		hasAsks: true,
		bids:    tooMany,
		asks:    []shm.PriceLevel{{Price: float64(maxLighterBookLevels + 2), Size: 1}},
	}, true); err == nil || book.initialized {
		t.Fatalf("exceeding maxLighterBookLevels failed to reset: err=%v initialized=%v", err, book.initialized)
	}
}

func TestLighterOrderBookDeltaZeroAlloc(t *testing.T) {
	tmp := t.TempDir()
	t.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("lighter-zero-alloc")
	if err != nil {
		t.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		t.Fatal(err)
	}
	defer depth.Close()

	withTestSymbolRegistry(t, map[string]uint16{"ETH": SymbolETHPERP})
	l := NewLighter(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://mainnet.zklighter.elliot.ai/stream",
		Symbols: map[string]string{"ETH": "0"},
	}, matrix, nil, depth)

	guard := newSubscriptionGuard(context.Background(), "order_book", []string{"order_book/0"}, time.Second)
	defer guard.cancel()
	books := map[int]*lighterBook{0: {}}
	scratch := newLighterFrameScratch()
	nowMs := int64(1_794_712_726_900)

	snapFrame := buildLighterSnapshotFrame(1000, 100, 10, 1_794_712_726_000_000, 1_794_712_726_000)
	if _, err := l.processOrderBookFrame(snapFrame, nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}
	if guard.pending != 0 {
		t.Fatalf("expected pending=0 after snapshot, got %d", guard.pending)
	}

	deltaUpdate := []byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726100,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712726100000,"bids":[{"price":"10000","size":"2.5"}],"asks":[{"price":"10001","size":"3.2"}]}}`)
	if _, err := l.processOrderBookFrame(deltaUpdate, nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}

	book := books[0]
	allocs := testing.AllocsPerRun(1000, func() {
		book.nonce = 100
		book.offset = 10
		book.lastUpdatedAt = 1_794_712_726_000_000
		sendPong, err := l.processOrderBookFrame(deltaUpdate, nowMs, guard, books, &scratch)
		if err != nil || sendPong {
			t.Fatalf("unexpected delta failure: sendPong=%v err=%v", sendPong, err)
		}
	})
	if allocs != 0 {
		t.Fatalf("expected 0 allocations in Lighter steady-state delta, got %v", allocs)
	}

	// Warm up an insert + delete pair so slice capacity is grown, then assert 0 allocs across insert+delete.
	deltaInsert := []byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726200,"order_book":{"nonce":102,"begin_nonce":101,"offset":12,"last_updated_at":1794712726200000,"bids":[{"price":"9999.5","size":"4.0"}],"asks":[]}}`)
	deltaDelete := []byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726300,"order_book":{"nonce":103,"begin_nonce":102,"offset":13,"last_updated_at":1794712726300000,"bids":[{"price":"9999.5","size":"0"}],"asks":[]}}`)
	if _, err := l.processOrderBookFrame(deltaInsert, nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}
	if _, err := l.processOrderBookFrame(deltaDelete, nowMs, guard, books, &scratch); err != nil {
		t.Fatal(err)
	}
	pairAllocs := testing.AllocsPerRun(1000, func() {
		book.nonce = 101
		book.offset = 11
		book.lastUpdatedAt = 1_794_712_726_100_000
		if _, err := l.processOrderBookFrame(deltaInsert, nowMs, guard, books, &scratch); err != nil {
			t.Fatal(err)
		}
		if _, err := l.processOrderBookFrame(deltaDelete, nowMs, guard, books, &scratch); err != nil {
			t.Fatal(err)
		}
	})
	if pairAllocs != 0 {
		t.Fatalf("expected 0 allocations in Lighter insert+delete delta pair, got %v", pairAllocs)
	}
}

func BenchmarkLighterOrderBookDelta(b *testing.B) {
	tmp := b.TempDir()
	b.Setenv("ALEPH_SHM_DIR", tmp)
	matrix, err := shm.NewMatrix("lighter-bench")
	if err != nil {
		b.Fatal(err)
	}
	defer matrix.Close()

	depth, err := shm.NewDepthWriter(filepath.Join(tmp, "aleph-depth"), shm.NumSymbols)
	if err != nil {
		b.Fatal(err)
	}
	defer depth.Close()

	withTestSymbolRegistry(b, map[string]uint16{"ETH": SymbolETHPERP})
	l := NewLighter(config.ExchangeConfig{
		Enabled: true,
		WSURL:   "wss://mainnet.zklighter.elliot.ai/stream",
		Symbols: map[string]string{"ETH": "0"},
	}, matrix, nil, depth)

	guard := newSubscriptionGuard(context.Background(), "order_book", []string{"order_book/0"}, time.Second)
	defer guard.cancel()
	books := map[int]*lighterBook{0: {}}
	scratch := newLighterFrameScratch()
	nowMs := int64(1_794_712_726_900)

	snapFrame := buildLighterSnapshotFrame(1000, 100, 10, 1_794_712_726_000_000, 1_794_712_726_000)
	if _, err := l.processOrderBookFrame(snapFrame, nowMs, guard, books, &scratch); err != nil {
		b.Fatal(err)
	}
	deltaFrame := []byte(`{"type":"update/order_book","channel":"order_book:0","timestamp":1794712726100,"order_book":{"nonce":101,"begin_nonce":100,"offset":11,"last_updated_at":1794712726100000,"bids":[{"price":"10000","size":"2.5"}],"asks":[{"price":"10001","size":"3.2"}]}}`)
	if _, err := l.processOrderBookFrame(deltaFrame, nowMs, guard, books, &scratch); err != nil {
		b.Fatal(err)
	}

	book := books[0]
	b.ReportAllocs()
	b.ResetTimer()
	for i := 0; i < b.N; i++ {
		book.nonce = 100
		book.offset = 10
		book.lastUpdatedAt = 1_794_712_726_000_000
		if _, err := l.processOrderBookFrame(deltaFrame, nowMs, guard, books, &scratch); err != nil {
			b.Fatal(err)
		}
	}
}
