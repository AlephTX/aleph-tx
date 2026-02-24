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

func packSymbol(s string) []byte {
	b := make([]byte, 12)
	copy(b, s)
	return b
}

func packFixed(s string, n int) []byte {
	b := make([]byte, n)
	copy(b, s)
	return b
}

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
			// Binary: type(1) + symbol(12) + bid(16) + ask(16) + ts(8) = 53 bytes
			var buf [53]byte
			buf[0] = shm.MsgTypeTicker
			copy(buf[1:13], packSymbol(raw.Symbol))
			copy(buf[13:29], packFixed(raw.BidPrice, 16))
			copy(buf[29:45], packFixed(raw.AskPrice, 16))
			ts := time.Now().UnixMilli()
			binary.LittleEndian.PutUint64(buf[45:53], uint64(ts))
			f.pub.Publish(buf[0], buf[:])
		} else if strings.Contains(envelope.Stream, "@depth") {
			var raw struct {
				Symbol string     `json:"s"`
				Bids   [][]string `json:"b"`
				Asks   [][]string `json:"a"`
			}
			if json.Unmarshal(envelope.Data, &raw) != nil {
				continue
			}
			// Fixed: type(1) + + 6 bids symbol(12)(96) + 6 asks(96) + ts(8) = 213 bytes
			var buf [213]byte
			buf[0] = shm.MsgTypeDepth
			copy(buf[1:13], packSymbol(raw.Symbol))

			off := 13
			// 6 bids, each 16 bytes (price + qty)
			for i := 0; i < 6; i++ {
				if i < len(raw.Bids) && len(raw.Bids[i]) >= 2 {
					copy(buf[off:], packFixed(raw.Bids[i][0], 8))
					copy(buf[off+8:], packFixed(raw.Bids[i][1], 8))
				}
				off += 16
			}
			// 6 asks
			for i := 0; i < 6; i++ {
				if i < len(raw.Asks) && len(raw.Asks[i]) >= 2 {
					copy(buf[off:], packFixed(raw.Asks[i][0], 8))
					copy(buf[off+8:], packFixed(raw.Asks[i][1], 8))
				}
				off += 16
			}
			ts := time.Now().UnixMilli()
			binary.LittleEndian.PutUint64(buf[205:213], uint64(ts))
			f.pub.Publish(buf[0], buf[:])
		}
	}
}
