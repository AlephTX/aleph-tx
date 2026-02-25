package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
)

// ZeroOne Exchange (01.xyz) WebSocket Adapter
type ZeroOne struct {
	cfg    config.ExchangeConfig
	matrix *shm.Matrix
	symMap map[string]uint16
}

func NewZeroOne(cfg config.ExchangeConfig, matrix *shm.Matrix) *ZeroOne {
	return &ZeroOne{
		cfg:    cfg,
		matrix: matrix,
		symMap: BuildReverseSymbolMap(cfg.Symbols),
	}
}

type zeroOneSubMessage struct {
	Type     string `json:"type"`
	Topic    string `json:"topic"`
	Market   string `json:"market"`
}

type zeroOneEvent struct {
	Topic    string           `json:"topic"`
	Market   string           `json:"market"`
	Type     string           `json:"type"`
	Data     zeroOneData      `json:"data"`
}

type zeroOneData struct {
	Bids [][]string `json:"bids"`
	Asks [][]string `json:"asks"`
}

func (z *ZeroOne) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "01", z.connect)
}

func (z *ZeroOne) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, z.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()

	log.Printf("01: connected to %s", z.cfg.WSURL)

	// Subscribe to orderbook events for all configured symbols
	for _, rawSym := range z.cfg.Symbols {
		sub := zeroOneSubMessage{
			Type:   "subscribe",
			Topic:  "orderbook",
			Market: rawSym,
		}
		if err := c.Write(ctx, websocket.MessageText, mustJSON(sub)); err != nil {
			return fmt.Errorf("subscribe %s: %w", rawSym, err)
		}
		log.Printf("01: subscribed to orderbook for %s", rawSym)
	}

	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		var event zeroOneEvent
		if err := json.Unmarshal(data, &event); err != nil {
			continue
		}

		if event.Topic != "orderbook" || (event.Type != "snapshot" && event.Type != "update") {
			continue
		}

		if len(event.Data.Bids) == 0 || len(event.Data.Asks) == 0 {
			continue
		}

		// Parse the Best Bid and Best Ask
		bidPx, err := strconv.ParseFloat(event.Data.Bids[0][0], 64)
		if err != nil { continue }
		bidSz, err := strconv.ParseFloat(event.Data.Bids[0][1], 64)
		if err != nil { continue }
		
		askPx, err := strconv.ParseFloat(event.Data.Asks[0][0], 64)
		if err != nil { continue }
		askSz, err := strconv.ParseFloat(event.Data.Asks[0][1], 64)
		if err != nil { continue }

		symID, ok := z.symMap[event.Market]
		if !ok {
			continue
		}

		tsNs := uint64(time.Now().UnixNano())
		z.matrix.WriteBBO(Exchange01, symID, tsNs, bidPx, bidSz, askPx, askSz)
	}
}
