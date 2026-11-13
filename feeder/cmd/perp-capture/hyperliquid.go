package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"slices"
	"time"
)

const fundingLookbackMS = int64(3 * time.Hour / time.Millisecond)

func (source subscription) isHyperliquidWS() bool {
	return source.source == "hyperliquid" || source.source == "hyperliquid_trades"
}

type hyperliquidSubscription struct {
	Type string `json:"type"`
	Coin string `json:"coin"`
}

type hyperliquidSubscribe struct {
	Method       string                  `json:"method"`
	Subscription hyperliquidSubscription `json:"subscription"`
}

func hyperliquidExtraSources(markets string, trades bool, funding time.Duration) ([]subscription, error) {
	coins, err := symbols(markets)
	if err != nil {
		return nil, err
	}
	if (trades || funding != 0) && len(coins) == 0 {
		return nil, errors.New("Hyperliquid trades and funding need markets")
	}
	var sources []subscription
	if trades {
		for _, coin := range coins {
			sources = append(sources, subscription{
				source: "hyperliquid_trades", url: "wss://api.hyperliquid.xyz/ws", market: coin,
				coins: []string{coin}, requests: []any{hyperliquidSubscribe{"subscribe", hyperliquidSubscription{"trades", coin}}},
			})
		}
	}
	if funding != 0 {
		if funding < time.Minute {
			return nil, errors.New("Hyperliquid funding polling interval must be at least 1m")
		}
		sources = append(sources, subscription{source: "hyperliquid_funding", url: "https://api.hyperliquid.xyz/info", coins: coins, interval: funding})
	}
	return sources, nil
}

// Keep exact public market fields without the participants and transaction hash.
type hyperliquidTrade struct {
	Coin string `json:"coin"`
	Side string `json:"side"`
	Px   string `json:"px"`
	Sz   string `json:"sz"`
	Time int64  `json:"time"`
	ID   uint64 `json:"tid"`
}

func hyperliquidPublication(source subscription, channel string, raw []byte, acknowledged map[string]bool) (record, error) {
	row := record{Source: source.source}
	var message struct {
		Data json.RawMessage `json:"data"`
	}
	if err := json.Unmarshal(raw, &message); err != nil {
		return record{}, errors.New("invalid Hyperliquid publication")
	}
	switch channel {
	case "subscriptionResponse":
		var ack hyperliquidSubscribe
		if err := json.Unmarshal(message.Data, &ack); err != nil || ack.Method != "subscribe" ||
			!slices.Contains(source.coins, ack.Subscription.Coin) {
			return record{}, errors.New("invalid Hyperliquid subscription response")
		}
		switch ack.Subscription.Type {
		case "trades":
			if source.source != "hyperliquid_trades" {
				return record{}, errors.New("unexpected Hyperliquid trade subscription")
			}
		case "l2Book", "activeAssetCtx":
			if source.source != "hyperliquid" {
				return record{}, errors.New("unexpected Hyperliquid book subscription")
			}
		default:
			return record{}, errors.New("unexpected Hyperliquid subscription channel")
		}
		row.Market = ack.Subscription.Coin
		row.Event = "subscribed"
		payload, err := json.Marshal(ack.Subscription)
		if err != nil {
			return record{}, err
		}
		row.Payload = payload
		acknowledged[row.Market] = true
	case "trades":
		if source.source != "hyperliquid_trades" || !acknowledged[source.market] {
			return record{}, errors.New("Hyperliquid trades without subscription acknowledgment")
		}
		var trades []hyperliquidTrade
		if err := json.Unmarshal(message.Data, &trades); err != nil || len(trades) == 0 {
			return record{}, errors.New("invalid Hyperliquid trade batch")
		}
		for _, trade := range trades {
			if trade.Coin != source.market || trade.Time <= 0 || trade.ID == 0 ||
				(trade.Side != "B" && trade.Side != "A") || trade.Px == "" || trade.Sz == "" {
				return record{}, errors.New("invalid Hyperliquid trade identity or fields")
			}
		}
		payload, err := json.Marshal(struct {
			Channel string             `json:"channel"`
			Data    []hyperliquidTrade `json:"data"`
		}{channel, trades})
		if err != nil {
			return record{}, err
		}
		row.Market, row.Event, row.Payload = source.market, "message", payload
	case "l2Book", "activeAssetCtx":
		var data struct {
			Coin string `json:"coin"`
		}
		if err := json.Unmarshal(message.Data, &data); err != nil || source.source != "hyperliquid" || !slices.Contains(source.coins, data.Coin) {
			return record{}, errors.New("invalid Hyperliquid market identity")
		}
		row.Market, row.Event, row.Payload = data.Coin, "message", raw
	}
	return row, nil
}

type hyperliquidFundingQuery struct {
	Type      string `json:"type"`
	Coin      string `json:"coin"`
	StartTime int64  `json:"startTime"`
	EndTime   int64  `json:"endTime"`
}

func fundingQuery(coin string, sent int64) hyperliquidFundingQuery {
	return hyperliquidFundingQuery{"fundingHistory", coin, sent - fundingLookbackMS, sent}
}

func hyperliquidFundingRequest(ctx context.Context, url, coin string, sent int64) (*http.Request, error) {
	query, err := json.Marshal(fundingQuery(coin, sent))
	if err != nil {
		return nil, err
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(query))
	if err == nil {
		request.Header.Set("Content-Type", "application/json")
	}
	return request, err
}

func emitHyperliquidFunding(source subscription, coin string, sent, received int64, raw []byte, emit func(record) error) error {
	query := fundingQuery(coin, sent)
	var rates []struct {
		Coin        string `json:"coin"`
		Time        int64  `json:"time"`
		FundingRate string `json:"fundingRate"`
	}
	if received < sent || json.Unmarshal(raw, &rates) != nil || rates == nil {
		return errors.New("invalid Hyperliquid funding history response")
	}
	for _, rate := range rates {
		if rate.Coin != coin || rate.Time < query.StartTime || rate.Time > query.EndTime || rate.FundingRate == "" {
			return errors.New("invalid Hyperliquid funding history market or time")
		}
	}
	payload, err := json.Marshal(struct {
		Query hyperliquidFundingQuery `json:"query"`
		Data  json.RawMessage         `json:"data"`
	}{query, raw})
	if err != nil {
		return err
	}
	return emit(record{Source: source.source, Market: coin, Event: "message", ReceivedAt: received, RequestSentAt: sent, Payload: payload})
}
