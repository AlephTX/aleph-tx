package main

import "encoding/json"

// The public feed also carries account, order and position fields. Capture
// only the fields needed for market replay.
func publicLighterTrades(raw []byte) ([]byte, error) {
	var event struct {
		Type    string `json:"type"`
		Channel string `json:"channel"`
		Trades  []struct {
			TradeID    json.Number `json:"trade_id"`
			MarketID   int         `json:"market_id"`
			Timestamp  int64       `json:"timestamp"`
			Price      string      `json:"price"`
			Size       string      `json:"size"`
			IsMakerAsk *bool       `json:"is_maker_ask"`
		} `json:"trades"`
	}
	if err := json.Unmarshal(raw, &event); err != nil {
		return nil, err
	}
	return json.Marshal(event)
}
