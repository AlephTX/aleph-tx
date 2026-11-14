package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"strconv"
	"strings"
)

// Each symbol has its own public USD-M connection and reconnect epoch. Binance
// update IDs need not be consecutive; the offline consumer checks regressions.
func binanceSubscriptions(raw string) ([]subscription, error) {
	markets, err := symbols(raw)
	if err != nil {
		return nil, err
	}
	var result []subscription
	for _, market := range markets {
		if !strings.HasSuffix(market, "USDT") || strings.IndexFunc(market, func(r rune) bool {
			return !(r >= 'A' && r <= 'Z' || r >= '0' && r <= '9')
		}) >= 0 {
			return nil, errors.New("Binance requires uppercase USD-M USDT symbols")
		}
		result = append(result, subscription{source: "binance", market: market,
			url: "wss://fstream.binance.com/public/ws/" + strings.ToLower(market) + "@bookTicker"})
	}
	return result, nil
}

// Only public BBO fields enter the archive. Missing source times/IDs remain
// missing, never substituted with local time. Their absence fails replay input
// admission; keeping the publication makes that coverage failure auditable.
func publicBinanceBBO(raw []byte, market string) ([]byte, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(raw, &fields); err != nil {
		return nil, err
	}
	var event, symbol string
	if json.Unmarshal(fields["e"], &event) != nil || event != "bookTicker" ||
		json.Unmarshal(fields["s"], &symbol) != nil || symbol != market {
		return nil, errors.New("unexpected Binance BBO identity")
	}
	prices := make(map[string]float64)
	for _, key := range []string{"b", "B", "a", "A"} {
		var value string
		if json.Unmarshal(fields[key], &value) != nil {
			return nil, fmt.Errorf("invalid Binance %s", key)
		}
		number, err := strconv.ParseFloat(value, 64)
		if err != nil || math.IsNaN(number) || math.IsInf(number, 0) || number <= 0 {
			return nil, fmt.Errorf("invalid Binance %s", key)
		}
		prices[key] = number
	}
	if prices["b"] >= prices["a"] {
		return nil, errors.New("crossed Binance BBO")
	}
	for _, key := range []string{"u", "E", "T", "st"} {
		if value, exists := fields[key]; exists {
			var number uint64
			if json.Unmarshal(value, &number) != nil || number == 0 || key == "st" && number != 1 {
				return nil, fmt.Errorf("invalid Binance %s", key)
			}
		}
	}
	clean := make(map[string]json.RawMessage)
	for _, key := range []string{"e", "s", "u", "E", "T", "b", "B", "a", "A", "st"} {
		if value, exists := fields[key]; exists {
			clean[key] = value
		}
	}
	return json.Marshal(clean)
}
