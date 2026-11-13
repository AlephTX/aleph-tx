package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
)

const extendedHost = "api.starknet.extended.exchange"

func extendedSubscriptions(ctx context.Context, raw string) ([]subscription, error) {
	markets, err := perpetualSymbols(raw)
	if err != nil {
		return nil, err
	}
	query := url.Values{"market": markets}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet,
		"https://"+extendedHost+"/api/v1/info/markets?"+query.Encode(), nil)
	if err != nil {
		return nil, err
	}
	request.Header.Set("User-Agent", "AlephTX-public-capture/1.0")
	client := http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	defer client.CloseIdleConnections()
	response, err := client.Do(request)
	if err != nil {
		return nil, fmt.Errorf("Extended market metadata: %w", err)
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("Extended market metadata HTTP %d", response.StatusCode)
	}
	return extendedMarkets(io.LimitReader(response.Body, 1<<20), markets)
}

func extendedMarkets(body io.Reader, markets []string) ([]subscription, error) {
	var result struct {
		Status string
		Data   []struct {
			Name, Type, Status string
			Active             bool
			IsRFQ              *bool
		}
	}
	if err := json.NewDecoder(body).Decode(&result); err != nil || result.Status != "OK" {
		return nil, errors.New("invalid Extended market metadata")
	}
	eligible := make(map[string]bool)
	for _, m := range result.Data {
		eligible[m.Name] = m.Active && m.Status == "ACTIVE" && m.Type == "PERPETUAL" && m.IsRFQ != nil && !*m.IsRFQ
	}
	var feeds []subscription
	for _, market := range markets {
		if !eligible[market] {
			return nil, fmt.Errorf("Extended %s requires active perpetual CLOB metadata", market)
		}
		stream := "wss://" + extendedHost + "/stream.extended.exchange/v1/"
		feeds = append(feeds,
			subscription{source: "extended", market: market, url: stream + "orderbooks/" + market},
			subscription{source: "extended_trades", market: market, url: stream + "publicTrades/" + market},
		)
	}
	return feeds, nil
}

func extendedPublication(raw []byte, source subscription, previous uint64) (uint64, error) {
	var msg struct {
		Type  string
		Ts    uint64
		Seq   uint64
		Data  json.RawMessage
		Error json.RawMessage
	}
	if err := json.Unmarshal(raw, &msg); err != nil || len(msg.Error) > 0 && string(msg.Error) != "null" ||
		msg.Ts == 0 || msg.Seq == 0 || len(msg.Data) == 0 {
		return 0, errors.New("invalid Extended publication")
	}
	if (previous > 0 && msg.Seq != previous+1) || (previous == 0 && source.source == "extended" && (msg.Type != "SNAPSHOT" || msg.Seq != 1)) {
		return 0, errors.New("Extended sequence discontinuity")
	}
	if source.source == "extended" {
		var book struct {
			Market string `json:"m"`
		}
		if err := json.Unmarshal(msg.Data, &book); err != nil || book.Market != source.market ||
			(msg.Type != "SNAPSHOT" && msg.Type != "DELTA") {
			return 0, errors.New("invalid Extended book identity or type")
		}
	} else {
		var trades []struct {
			Market string `json:"m"`
		}
		if err := json.Unmarshal(msg.Data, &trades); err != nil || len(trades) == 0 {
			return 0, errors.New("invalid Extended trades publication")
		}
		for _, trade := range trades {
			if trade.Market != source.market {
				return 0, errors.New("invalid Extended trade identity")
			}
		}
	}
	return msg.Seq, nil
}
