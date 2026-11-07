package exchanges

import (
	"math"
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
func (r *eventRecorder) PushOrderFilledV2(ex uint8, sym uint16, order uint64, client, index int64, price, size, remaining, fee float64, ask bool, ts, trade uint64) {
	var side uint8
	if ask {
		side = 1
	}
	r.fills = append(r.fills, shm.ShmPrivateEventV2{ExchangeID: ex, SymbolID: sym, ExchangeOrderID: order, ClientOrderID: client, OrderIndex: index, FillPrice: price, FillSize: size, RemainingSize: remaining, FeePaid: fee, IsAsk: side, TimestampNs: ts, TradeID: trade})
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
