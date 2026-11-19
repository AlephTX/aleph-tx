package exchanges

import (
	"context"
	"fmt"
	"testing"

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
