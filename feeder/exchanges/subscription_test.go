package exchanges

import (
	"context"
	"testing"
	"time"

	"github.com/tidwall/gjson"
)

func TestSubscriptionRequiresMatchingAcknowledgment(t *testing.T) {
	parent := context.Background()
	g := newSubscriptionGuard(parent, "account_market", []string{"account_market/0/42"}, time.Second)
	defer g.cancel()
	for _, frame := range []string{
		`{"type":"ping"}`,
		`{"type":"subscribed/account_market","channel":"account_market:0:99"}`,
		`{"type":"update/account_market","channel":"account_market:0:42"}`,
	} {
		if err := g.observe(gjson.Parse(frame)); err != nil {
			t.Fatal(err)
		}
		if g.pending != 1 || g.readContext(parent) != g.ctx {
			t.Fatal("unconfirmed subscription escaped deadline")
		}
	}
	ack := gjson.Parse(`{"type":"subscribed/account_market","channel":"account_market:0:42"}`)
	if err := g.observe(ack); err != nil {
		t.Fatal(err)
	}
	if g.pending != 0 || g.readContext(parent) != parent || !g.acceptsData(ack) {
		t.Fatal("valid subscription not confirmed")
	}
	if err := g.observe(ack); err != nil || g.pending != 0 {
		t.Fatal("duplicate acknowledgment changed state")
	}
}

func TestSubscriptionErrorsReconnectEvenAfterAcknowledgment(t *testing.T) {
	g := newSubscriptionGuard(context.Background(), "user_stats", []string{"user_stats/42"}, time.Second)
	defer g.cancel()
	if err := g.observe(gjson.Parse(`{"type":"subscribed/user_stats","channel":"user_stats:42"}`)); err != nil {
		t.Fatal(err)
	}
	for _, frame := range []string{`{"type":"error","code":401}`, `{"error":{"code":403,"message":"denied"}}`} {
		if err := g.observe(gjson.Parse(frame)); err == nil {
			t.Fatalf("ignored %s", frame)
		}
	}
	if g.acceptsData(gjson.Parse(`{"type":"update/user_stats","channel":"user_stats:99"}`)) {
		t.Fatal("accepted different account")
	}
}

func TestSubscriptionDeadlineDoesNotResetOnPings(t *testing.T) {
	parent := context.Background()
	g := newSubscriptionGuard(parent, "user_stats", []string{"user_stats/42"}, -time.Second)
	defer g.cancel()
	deadline, _ := g.ctx.Deadline()
	for i := 0; i < 5; i++ {
		if err := g.observe(gjson.Parse(`{"type":"ping"}`)); err != nil {
			t.Fatal(err)
		}
	}
	current, _ := g.readContext(parent).Deadline()
	if current != deadline || g.readContext(parent).Err() != context.DeadlineExceeded {
		t.Fatal("pings kept missing subscription alive")
	}
}
