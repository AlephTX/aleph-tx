package main

import "encoding/json"

// The public feed also carries account, order and position fields. Capture
// only public market, fee-signature and execution-clock fields. A fee signature
// is not account identity or incentive qualification; never infer absent values.
func publicLighterTrades(raw []byte) ([]byte, error) {
	var event struct {
		Type             string `json:"type"`
		Channel          string `json:"channel"`
		FeeFieldsVersion int    `json:"fee_fields_version"`
		Trades           []struct {
			TradeID    json.Number `json:"trade_id"`
			MarketID   int         `json:"market_id"`
			Timestamp  int64       `json:"timestamp"`
			Price      string      `json:"price"`
			Size       string      `json:"size"`
			IsMakerAsk *bool       `json:"is_maker_ask"`
			// Preserve native integer metadata without float conversion. Absent
			// values stay absent; a wire zero remains zero, not an execution time.
			// timestamp is still block milliseconds; transaction_time is microseconds.
			TransactionTime *uint64 `json:"transaction_time,omitempty"`
			BlockHeight     *uint64 `json:"block_height,omitempty"`
			// Trade messages need not expose this transaction-scoped counter.
			SequenceIndex *uint64 `json:"sequence_index,omitempty"`
			MakerFee      *int64  `json:"maker_fee,omitempty"`
			TakerFee      *int64  `json:"taker_fee,omitempty"`
		} `json:"trades"`
	}
	if err := json.Unmarshal(raw, &event); err != nil {
		return nil, err
	}
	// Older archives stripped these fields. This collector-owned marker lets
	// audits distinguish historical stripping from wire omission (zero/empty
	// per the venue schema). Neither omission nor a matching fee proves a tier.
	event.FeeFieldsVersion = 1
	return json.Marshal(event)
}
