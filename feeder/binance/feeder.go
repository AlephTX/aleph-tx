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
	Exchange  string `json:"exchange"`
	Symbol    string `json:"symbol"`
	Bid       string `json:"bid"`
	Ask       string `json:"ask"`
	Last      string `json:"last"`
	Volume24h string `json:"volume_24h"`
	Ts        int64  `json:"ts"`
}

// DepthUpdate is a partial orderbook update (incremental diff).
type DepthUpdate struct {
	Exchange string     `json:"exchange"`
	Symbol   string     `json:"symbol"`
	Bids     [][]string `json:"bids"` // [price, qty] — qty "0" means remove
	Asks     [][]string `json:"asks"`
	Ts       int64      `json:"ts"`
}

// raw Binance bookTicker
type binanceTicker struct {
	UpdateID int64  `json:"u"`
	Symbol   string `json:"s"`
	BidPrice string `json:"b"`
	BidQty   string `json:"B"`
	AskPrice string `json:"a"`
	AskQty   string `json:"A"`
}

// raw Binance depth diff stream
type binanceDepth struct {
	EventType string     `json:"e"`
	EventTime int64      `json:"E"`
	Symbol    string     `json:"s"`
	Bids      [][]string `json:"b"`
	Asks      [][]string `json:"a"`
}

// Feeder subscribes to Binance combined stream and publishes normalised events.
type Feeder struct {
	symbols []string
	pub     Publisher
}

func NewFeeder(symbols []string, pub Publisher) *Feeder {
	return &Feeder{symbols: symbols, pub: pub}
}

func (f *Feeder) Run(ctx context.Context) error {
	streams := make([]string, 0, len(f.symbols)*2)
	for _, s := range f.symbols {
		s = strings.ToLower(s)
		streams = append(streams, s+"@bookTicker")
		streams = append(streams, s+"@depth@100ms")
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

		if strings.HasSuffix(envelope.Stream, "@bookTicker") {
			var raw binanceTicker
			if err := json.Unmarshal(envelope.Data, &raw); err != nil {
				continue
			}
			f.pub.Publish("ticker", Ticker{
				Exchange: "binance",
				Symbol:   raw.Symbol,
				Bid:      raw.BidPrice,
				Ask:      raw.AskPrice,
				Last:     raw.BidPrice,
				Ts:       time.Now().UnixMilli(),
			})
		} else if strings.Contains(envelope.Stream, "@depth") {
			var raw binanceDepth
			if err := json.Unmarshal(envelope.Data, &raw); err != nil {
				continue
			}
			f.pub.Publish("depth", DepthUpdate{
				Exchange: "binance",
				Symbol:   raw.Symbol,
				Bids:     raw.Bids,
				Asks:     raw.Asks,
				Ts:       raw.EventTime,
			})
		}
	}
}
