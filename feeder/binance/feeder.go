// Package binance connects to Binance WebSocket streams and writes to shared memory ring buffer.
package binance

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"log"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

// RingPublisher writes raw binary messages to shared memory ring.
type RingPublisher struct {
	ring *shm.RingBuffer
}

func (p *RingPublisher) Publish(msgType byte, payload []byte) {
	_ = p.ring.Write(msgType, payload)
}

// Feeder subscribes to Binance and writes binary to ring buffer.
type Feeder struct {
	symbols []string
	pub     *RingPublisher
}

func NewFeeder(symbols []string, ring *shm.RingBuffer) *Feeder {
	return &Feeder{symbols: symbols, pub: &RingPublisher{ring: ring}}
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
			var raw struct {
				Symbol   string `json:"s"`
				BidPrice string `json:"b"`
				AskPrice string `json:"a"`
			}
			if json.Unmarshal(envelope.Data, &raw) != nil {
				continue
			}
			// Format: symbol|bid|ask
			payload := []byte(raw.Symbol + "|" + raw.BidPrice + "|" + raw.AskPrice)
			// msg = type(1) + len(2) + payload
			msg := make([]byte, 1+2+len(payload))
			msg[0] = shm.MsgTypeTicker
			binary.LittleEndian.PutUint16(msg[1:3], uint16(len(payload)))
			copy(msg[3:], payload)
			f.pub.Publish(msg[0], msg)
		} else if strings.Contains(envelope.Stream, "@depth") {
			var raw struct {
				Symbol string     `json:"s"`
				Bids   [][]string `json:"b"`
				Asks   [][]string `json:"a"`
			}
			if json.Unmarshal(envelope.Data, &raw) != nil {
				continue
			}
			// Format: symbol|bids|asks (price,qty;price,qty)
			var bidsStr, asksStr string
			for i, b := range raw.Bids {
				if i > 0 {
					bidsStr += ";"
				}
				if len(b) >= 2 {
					bidsStr += b[0] + "," + b[1]
				}
			}
			for i, a := range raw.Asks {
				if i > 0 {
					asksStr += ";"
				}
				if len(a) >= 2 {
					asksStr += a[0] + "," + a[1]
				}
			}
			payload := []byte(raw.Symbol + "|" + bidsStr + "|" + asksStr)
			msg := make([]byte, 1+2+len(payload))
			msg[0] = shm.MsgTypeDepth
			binary.LittleEndian.PutUint16(msg[1:3], uint16(len(payload)))
			copy(msg[3:], payload)
			f.pub.Publish(msg[0], msg)
		}
	}
}
