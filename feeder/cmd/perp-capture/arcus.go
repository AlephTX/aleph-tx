package main

import (
	"encoding/json"
	"errors"
	"math"
	"strconv"
	"strings"
)

type arcusTradeSubscribe struct {
	Type    string `json:"type"`
	Channel string `json:"channel"`
	ID      string `json:"id"`
}

type arcusSubscribedAck struct {
	Channel string `json:"channel"`
	ID      string `json:"id"`
}

// Keep exact public market fields without maker/taker wallet addresses or order IDs.
type arcusPublicTrade struct {
	MarketDisplayName string  `json:"marketDisplayName"`
	MarketID          uint16  `json:"marketId"`
	Side              string  `json:"side"`
	Price             string  `json:"price"`
	Size              string  `json:"size"`
	TradeID           string  `json:"tradeId"`
	Timestamp         uint64  `json:"timestamp"`
	SequenceNumber    *uint64 `json:"sequenceNumber,omitempty"`
}

func arcusTradeSubscriptions(arcusWS, arcusREST string, enabled bool) ([]subscription, error) {
	if !enabled {
		return nil, nil
	}
	wsCoins, err := perpetualSymbols(arcusWS)
	if err != nil {
		return nil, err
	}
	restCoins, err := perpetualSymbols(arcusREST)
	if err != nil {
		return nil, err
	}
	seen := make(map[string]bool)
	var coins []string
	for _, coin := range append(wsCoins, restCoins...) {
		if !seen[coin] {
			seen[coin] = true
			coins = append(coins, coin)
		}
	}
	if len(coins) == 0 {
		return nil, errors.New("Arcus trades require -arcus-ws or -arcus markets")
	}
	if len(coins) > 8 {
		return nil, errors.New("at most eight markets per source")
	}
	sources := make([]subscription, 0, len(coins))
	for _, coin := range coins {
		sources = append(sources, subscription{
			source:   "arcus_trades",
			url:      "wss://api.arcus.xyz/v1/ws",
			market:   coin,
			coins:    []string{coin},
			requests: []any{arcusTradeSubscribe{Type: "subscribe", Channel: "trades", ID: coin}},
		})
	}
	return sources, nil
}

func isValidPositiveDecimalString(s string) bool {
	if s == "" || strings.TrimSpace(s) != s || strings.ContainsAny(s, "eE+-") {
		return false
	}
	dotCount := strings.Count(s, ".")
	if dotCount > 1 || strings.HasPrefix(s, ".") || strings.HasSuffix(s, ".") {
		return false
	}
	for i := 0; i < len(s); i++ {
		c := s[i]
		if c != '.' && (c < '0' || c > '9') {
			return false
		}
	}
	v, err := strconv.ParseFloat(s, 64)
	return err == nil && !math.IsNaN(v) && !math.IsInf(v, 0) && v > 0
}

func arcusTradePublication(source subscription, raw []byte, acknowledged map[string]bool) (record, error) {
	if source.source != "arcus_trades" {
		return record{}, errors.New("unexpected Arcus trade source")
	}
	var message struct {
		Type     string          `json:"type"`
		Channel  string          `json:"channel"`
		ID       string          `json:"id"`
		Contents json.RawMessage `json:"contents"`
	}
	if err := json.Unmarshal(raw, &message); err != nil {
		return record{}, errors.New("invalid Arcus trade publication")
	}
	switch message.Type {
	case "connected":
		return record{}, nil
	case "subscribed":
		if message.Channel != "trades" || message.ID != source.market {
			return record{}, errors.New("invalid Arcus trade subscription acknowledgment")
		}
		payload, err := json.Marshal(arcusSubscribedAck{
			Channel: "trades",
			ID:      source.market,
		})
		if err != nil {
			return record{}, err
		}
		acknowledged[source.market] = true
		return record{
			Source:  source.source,
			Market:  source.market,
			Event:   "subscribed",
			Payload: payload,
		}, nil
	case "channel_data":
		if message.Channel != "trades" || message.ID != source.market || !acknowledged[source.market] {
			return record{}, errors.New("Arcus trades without matching subscription acknowledgment")
		}
		var trades []arcusPublicTrade
		if err := json.Unmarshal(message.Contents, &trades); err != nil || len(trades) == 0 {
			return record{}, errors.New("invalid Arcus trade batch")
		}
		for _, trade := range trades {
			if trade.MarketDisplayName != source.market ||
				trade.MarketID == 0 ||
				(trade.Side != "BUY" && trade.Side != "SELL") ||
				trade.TradeID == "" ||
				trade.Timestamp == 0 ||
				!isValidPositiveDecimalString(trade.Price) ||
				!isValidPositiveDecimalString(trade.Size) {
				return record{}, errors.New("invalid Arcus trade identity or fields")
			}
		}
		payload, err := json.Marshal(struct {
			Channel  string             `json:"channel"`
			ID       string             `json:"id"`
			Contents []arcusPublicTrade `json:"contents"`
		}{
			Channel:  "trades",
			ID:       source.market,
			Contents: trades,
		})
		if err != nil {
			return record{}, err
		}
		return record{
			Source:  source.source,
			Market:  source.market,
			Event:   "message",
			Payload: payload,
		}, nil
	default:
		return record{}, errors.New("unexpected Arcus trade message type")
	}
}
