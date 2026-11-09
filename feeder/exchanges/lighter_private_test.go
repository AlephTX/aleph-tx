package exchanges

import (
	"bytes"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"testing"

	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/tidwall/gjson"
)

type eventRecorder struct {
	fills   []shm.ShmPrivateEventV2
	cancels int
}

func (r *eventRecorder) PushPrivateStreamLost() {}

func (r *eventRecorder) PushOrderCreatedV2(uint8, uint16, uint64, int64, int64, float64, float64, bool, uint64) {
}
func (r *eventRecorder) PushOrderCanceledV2(uint8, uint16, uint64, int64, int64, float64, uint64) {
	r.cancels++
}
func (r *eventRecorder) PushOrderFilledV2(ex uint8, sym uint16, order uint64, client, index int64, price, size, remaining, fee float64, ask bool, ts, trade uint64, source uint8) {
	var side uint8
	if ask {
		side = 1
	}
	r.fills = append(r.fills, shm.ShmPrivateEventV2{ExchangeID: ex, SymbolID: sym, ExchangeOrderID: order, ClientOrderID: client, OrderIndex: index, FillPrice: price, FillSize: size, RemainingSize: remaining, FeePaid: fee, IsAsk: side, TimestampNs: ts, TradeID: trade, FeeSource: source})
}
func privateForTest() (*LighterPrivate, *eventRecorder) {
	r := &eventRecorder{}
	return &LighterPrivate{eventBuffer: r, auth: &LighterAuth{accountIndex: 42}, mktMap: map[int]uint16{0: 1002}, orderSizes: map[uint64]float64{}, orderDetails: map[uint64]orderDetail{}, seenTradeIDs: newTradeIDSet(tradeIDCapacity)}, r
}
func TestPrivateTradeIdentifiesTakerAndFees(t *testing.T) {
	lp, r := privateForTest()
	trade := gjson.Parse(`{"trade_id":1,"market_id":0,"price":"100","size":"2","is_maker_ask":true,"ask_id":10,"bid_id":11,"ask_account_id":99,"bid_account_id":42,"bid_client_id":123,"maker_fee":10,"taker_fee":200}`)
	lp.processTradeFast(trade)
	lp.processTradeFast(trade)
	if len(r.fills) != 1 {
		t.Fatalf("fills=%d", len(r.fills))
	}
	fill := r.fills[0]
	if fill.ExchangeOrderID != 11 || fill.ClientOrderID != 123 || fill.IsAsk != 0 || math.Abs(fill.FeePaid-.04) > 1e-12 {
		t.Fatalf("bad taker fill: %+v", fill)
	}
}
func TestPrivateTradeBidMakerUsesMakerFee(t *testing.T) {
	lp, r := privateForTest()
	lp.orderDetails[11] = orderDetail{clientOrderIndex: 123, orderIndex: 11, isAsk: false}
	lp.processTradeFast(gjson.Parse(`{"trade_id":2,"market_id":0,"price":"100","size":"2","is_maker_ask":false,"ask_id":10,"bid_id":11,"maker_fee":10,"taker_fee":200}`))
	if len(r.fills) != 1 || math.Abs(r.fills[0].FeePaid-.002) > 1e-12 {
		t.Fatalf("maker fee incorrect: %+v", r.fills)
	}
}
func TestPrivateTradeMalformedDoesNotPoisonDedup(t *testing.T) {
	lp, r := privateForTest()
	lp.orderDetails[11] = orderDetail{orderIndex: 11}
	lp.processTradeFast(gjson.Parse(`{"trade_id":3,"market_id":0,"price":"NaN","size":"1","bid_id":11}`))
	lp.processTradeFast(gjson.Parse(`{"trade_id":3,"market_id":0,"price":"100","size":"1","bid_id":11}`))
	if len(r.fills) != 1 {
		t.Fatal("valid replay lost")
	}
	lp.processTradeFast(gjson.Parse(`{"trade_id":4,"market_id":0,"price":"100","size":"1","bid_id":50,"ask_id":51,"is_maker_ask":true}`))
	if len(r.fills) != 1 {
		t.Fatal("ambiguous trade guessed as maker")
	}
}

func TestPrivatePartialOrderUpdateDoesNotDoubleSubtractFill(t *testing.T) {
	lp, r := privateForTest()
	lp.processOrderFast(gjson.Parse(`{"market_index":0,"order_index":11,"client_order_index":123,"status":"open","price":"100","initial_base_amount":"10","remaining_base_amount":"10"}`))
	lp.processOrderFast(gjson.Parse(`{"market_index":0,"order_index":11,"client_order_index":123,"status":"open","price":"100","initial_base_amount":"10","remaining_base_amount":"8"}`))
	lp.processTradeFast(gjson.Parse(`{"trade_id":5,"market_id":0,"price":"100","size":"2","bid_id":11,"ask_id":10}`))
	if len(r.fills) != 1 || r.fills[0].RemainingSize != 8 {
		t.Fatalf("partial fill subtracted twice: %+v", r.fills)
	}
	lp.processOrderFast(gjson.Parse(`{"market_index":0,"order_index":11,"client_order_index":123,"status":"canceled-expired","remaining_base_amount":"8"}`))
	if r.cancels != 1 || lp.isTrackedOrderID(11) {
		t.Fatal("terminal cancel reason did not clear order")
	}
}

// The fee source labels the selected role's rate without changing feePaid:
// absent and null stay missing (Lighter omits zero fields), an explicit 0 is
// a known zero, the other side's rate never fills in, and a guessed role or
// a malformed rate is never "rate derived".
func TestPrivateTradeLabelsFeeSourceWithoutChangingFeePaid(t *testing.T) {
	base := `"market_id":0,"price":"100","size":"2","ask_id":10,"bid_id":11,"ask_account_id":99,"bid_account_id":42`
	cases := []struct {
		name   string
		fields string
		fee    float64
		source uint8
	}{
		{"bid maker rate", `"is_maker_ask":false,"maker_fee":10,"taker_fee":200`, .002, shm.FeeSourceRateDerived},
		{"bid taker rate", `"is_maker_ask":true,"maker_fee":10,"taker_fee":200`, .04, shm.FeeSourceRateDerived},
		{"rebate", `"is_maker_ask":false,"maker_fee":-25`, -.005, shm.FeeSourceRateDerived},
		{"explicit zero", `"is_maker_ask":false,"maker_fee":0,"taker_fee":200`, 0, shm.FeeSourceExplicitZero},
		{"absent, other side high", `"is_maker_ask":false,"taker_fee":200`, 0, shm.FeeSourceMissing},
		{"null", `"is_maker_ask":false,"maker_fee":null`, 0, shm.FeeSourceMissing},
		{"fractional", `"is_maker_ask":false,"maker_fee":2.5`, .0005, shm.FeeSourceInvalid},
		{"string", `"is_maker_ask":false,"maker_fee":"10"`, .002, shm.FeeSourceInvalid},
		{"out of range", `"is_maker_ask":false,"maker_fee":1000001`, 200.0002, shm.FeeSourceInvalid},
		// gjson reads an absent or non-boolean is_maker_ask as false, so the bid is
		// treated as maker exactly as before; only the label records the guess.
		{"role unknown", `"taker_fee":200`, 0, shm.FeeSourceRoleUnknown},
		{"role not bool", `"is_maker_ask":"false","maker_fee":10,"taker_fee":200`, .002, shm.FeeSourceRoleUnknown},
	}
	for i, c := range cases {
		lp, r := privateForTest()
		lp.processTradeFast(gjson.Parse(fmt.Sprintf(`{"trade_id":%d,%s,%s}`, 100+i, base, c.fields)))
		if len(r.fills) != 1 {
			t.Fatalf("%s: fills=%d", c.name, len(r.fills))
		}
		fill := r.fills[0]
		if fill.FeeSource != c.source || math.Abs(fill.FeePaid-c.fee) > 1e-9 {
			t.Errorf("%s: source=%d fee=%v, want %d %v", c.name, fill.FeeSource, fill.FeePaid, c.source, c.fee)
		}
	}
}

// Drives the real processTradeFast into a real mmap'd V2 ring and keeps the
// event slots as src/types/events/v2_fee_sources.bin, which the Rust V2
// reader and TelemetrySink replay (v2_fee_source_fixture_matches_the_feeder).
// The last slot is an older writer's fill: its fee_source byte is 0.
// ALEPH_WRITE_FIXTURES=1 rewrites the file; otherwise it must match.
func TestPrivateTradeFeeSourceRingFixture(t *testing.T) {
	t.Setenv("ALEPH_SHM_DIR", t.TempDir())
	ring, err := shm.NewEventRingBufferV2()
	if err != nil {
		t.Fatal(err)
	}
	defer ring.Close()
	lp, _ := privateForTest()
	lp.eventBuffer = ring
	base := `"market_id":0,"price":"2000.5","size":"0.01","ask_id":990,"bid_id":110,"ask_account_id":99,"bid_account_id":42,"bid_client_id":11`
	for i, fields := range []string{
		`"is_maker_ask":false,"maker_fee":20,"taker_fee":400`, // maker, rate derived
		`"is_maker_ask":true,"maker_fee":20,"taker_fee":400`,  // taker, other side's rate
		`"is_maker_ask":false,"maker_fee":0,"taker_fee":400`,  // explicit zero
		`"is_maker_ask":false,"maker_fee":-25`,                // rebate
		`"is_maker_ask":false,"taker_fee":400`,                // missing for the role
		`"is_maker_ask":false,"maker_fee":2.5`,                // invalid
		`"maker_fee":20`,                                      // role unknown
	} {
		lp.processTradeFast(gjson.Parse(fmt.Sprintf(`{"trade_id":%d,%s,%s}`, 501+i, base, fields)))
	}
	// An older feeder: same fill, fee_source byte left at its reserved 0.
	ring.PushOrderFilledV2(shm.ExchangeLighter, 1002, 110, 11, 110, 2000.5, 0.01, 0, 0, false, 0, 508, 0)
	raw, err := os.ReadFile(shm.Path("aleph-events-v2"))
	if err != nil {
		t.Fatal(err)
	}
	const slots = 8
	events := raw[shm.V2HeaderSize : shm.V2HeaderSize+slots*shm.V2EventSize]
	// The feeder stamps wall-clock time; zero it so the fixture is stable.
	for i := 0; i < slots; i++ {
		clear(events[i*shm.V2EventSize+96 : i*shm.V2EventSize+104])
	}
	path := filepath.Join("..", "..", "src", "types", "events", "v2_fee_sources.bin")
	if os.Getenv("ALEPH_WRITE_FIXTURES") != "" {
		if err := os.WriteFile(path, events, 0o644); err != nil {
			t.Fatal(err)
		}
	}
	want, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("fixture missing (ALEPH_WRITE_FIXTURES=1 creates it): %v", err)
	}
	if !bytes.Equal(events, want) {
		t.Fatal("V2 bytes differ from the fixture the Rust reader replays")
	}
}
