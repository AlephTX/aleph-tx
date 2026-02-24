// Package binance connects to Binance WebSocket streams and normalises data.
package binance

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strings"
	"time"

	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

// Publisher is the minimal interface the feeder needs.
type Publisher interface {
	Publish(msgType string, payload any)
}

// Ticker is the normalised AlephTX ticker format.
type Ticker struct {
	Exchange  string  `json:"exchange"`
	Symbol    string  `json:"symbol"`
	Bid       string  `json:"bid"`
	Ask       string  `json:"ask"`
	Last      string  `json:"last"`
	Volume24h string  `json:"volume_24h"`
	Ts        int64   `json:"ts"` // unix ms
}

// binanceTicker is the raw Binance 24hr mini-ticker stream payload.
type binanceTicker struct {
	EventType string `json:"e"`
	Symbol    string `json:"s"`
	Close     string `json:"c"` // last price
	BidPrice  string `json:"b"`
	AskPrice  string `json:"a"`
	Volume    string `json:"v"`
	EventTime int64  `json:"E"`
}

// Feeder subscribes to Binance combined stream and publishes normalised tickers.
type Feeder struct {
	symbols []string
	pub     Publisher
}

func NewFeeder(symbols []string, pub Publisher) *Feeder {
	return &Feeder{symbols: symbols, pub: pub}
}

func (f *Feeder) Run(ctx context.Context) error {
	streams := make([]string, len(f.symbols))
	for i, s := range f.symbols {
		streams[i] = strings.ToLower(s) + "@bookTicker"
	}
	url := "wss://stream.binance.com:9443/stream?streams=" + strings.Join(streams, "/")

	for {
		if err := f.connect(ctx, url); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("binance: disconnected (%v), reconnecting in 5s...", err)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(5 * time.Second):
			}
		}
	}
}

func (f *Feeder) connect(ctx context.Context, url string) error {
	conn, _, err := websocket.Dial(ctx, url, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.CloseNow()
	log.Println("binance: connected")

	for {
		var envelope struct {
			Stream string          `json:"stream"`
			Data   json.RawMessage `json:"data"`
		}
		if err := wsjson.Read(ctx, conn, &envelope); err != nil {
			return err
		}

		var raw binanceTicker
		if err := json.Unmarshal(envelope.Data, &raw); err != nil {
			continue
		}

		ticker := Ticker{
			Exchange:  "binance",
			Symbol:    raw.Symbol,
			Bid:       raw.BidPrice,
			Ask:       raw.AskPrice,
			Last:      raw.Close,
			Volume24h: raw.Volume,
			Ts:        raw.EventTime,
		}
		f.pub.Publish("ticker", ticker)
	}
}
