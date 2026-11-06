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

func TestConnectionLoopDoesNotDialAfterCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	called := false
	err := RunConnectionLoop(ctx, "test", func(context.Context) error { called = true; return nil })
	if called || err != context.Canceled {
		t.Fatalf("called=%v err=%v", called, err)
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
