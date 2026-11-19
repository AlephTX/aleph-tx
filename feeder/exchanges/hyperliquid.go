package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"math"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

type hlTarget struct {
	exchangeID uint8
	symbolID   uint16
}

// Hyperliquid connects to the Hyperliquid L2 book WebSocket and routes native
// core coins (ExchangeHyperliquid), Entropy io:* coins (ExchangeEntropy), and
// TradeXYZ xyz:* coins (ExchangeTradeXYZ) into isolated SHM slots.
type Hyperliquid struct {
	wsURL  string
	matrix *shm.Matrix
	symMap map[string]hlTarget
}

func classifyHyperliquidExchange(rawCoin string) (uint8, bool) {
	if !strings.Contains(rawCoin, ":") {
		return ExchangeHyperliquid, rawCoin != ""
	}
	if strings.HasPrefix(rawCoin, "io:") && len(rawCoin) > 3 {
		return ExchangeEntropy, true
	}
	if strings.HasPrefix(rawCoin, "xyz:") && len(rawCoin) > 4 {
		return ExchangeTradeXYZ, true
	}
	return 0, false
}

func NewHyperliquid(cfg config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	return NewHyperliquidGroup(cfg.WSURL, []config.ExchangeConfig{cfg}, matrix)
}

func NewHyperliquidGroup(wsURL string, sections []config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	symMap := make(map[string]hlTarget)
	for _, sec := range sections {
		for localSym, rawCoin := range sec.AllPublicSymbols() {
			symID, ok := SymbolNameToID[localSym]
			if !ok {
				log.Printf("hyperliquid: WARNING: symbol %s not found in SymbolNameToID", localSym)
				continue
			}
			exchID, valid := classifyHyperliquidExchange(rawCoin)
			if !valid {
				log.Printf("hyperliquid: WARNING: unsupported HIP-3 coin %q for %s", rawCoin, localSym)
				continue
			}
			symMap[rawCoin] = hlTarget{exchangeID: exchID, symbolID: symID}
		}
	}
	return &Hyperliquid{
		wsURL:  wsURL,
		matrix: matrix,
		symMap: symMap,
	}
}

type hlEnvelope struct {
	Channel string          `json:"channel"`
	Data    json.RawMessage `json:"data"`
}

type hlL2Book struct {
	Coin   string      `json:"coin"`
	Time   int64       `json:"time"`
	Levels [][]hlLevel `json:"levels"`
}

type hlLevel struct {
	Px string `json:"px"`
	Sz string `json:"sz"`
}

func (h *Hyperliquid) invalidateAll() {
	if h.matrix == nil {
		return
	}
	for _, target := range h.symMap {
		h.matrix.InvalidateBBO(target.exchangeID, target.symbolID)
	}
}

func (h *Hyperliquid) Run(ctx context.Context) error {
	h.invalidateAll()
	lastPublication := make(map[hlTarget]int64, len(h.symMap))
	return RunConnectionLoop(ctx, "hyperliquid", func(ctx context.Context) error {
		return h.connect(ctx, lastPublication)
	})
}

func (h *Hyperliquid) connect(ctx context.Context, lastPublication map[hlTarget]int64) error {
	c, _, err := websocket.Dial(ctx, h.wsURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	defer h.invalidateAll()

	log.Printf("hyperliquid: connected to %s", h.wsURL)

	coins := make([]string, 0, len(h.symMap))
	for rawCoin := range h.symMap {
		coins = append(coins, rawCoin)
	}
	slices.Sort(coins)

	// Subscribe to l2Book for each configured core or HIP-3 coin
	for _, rawCoin := range coins {
		sub := map[string]any{
			"method": "subscribe",
			"subscription": map[string]any{
				"type": "l2Book",
				"coin": rawCoin,
			},
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe %s: %w", rawCoin, err)
		}
		log.Printf("hyperliquid: subscribed to %s", rawCoin)
	}

	stopKeepalive := wsutil.StartKeepalive(ctx, "hyperliquid", c, 15*time.Second, []byte(`{"method":"ping"}`))
	defer stopKeepalive()

	for {
		var raw json.RawMessage
		if err := wsjson.Read(ctx, c, &raw); err != nil {
			return err
		}
		if err := h.handleMessage(raw, time.Now(), lastPublication); err != nil {
			return err
		}
	}
}

func (h *Hyperliquid) handleMessage(raw []byte, now time.Time, lastPublication map[hlTarget]int64) error {
	var env hlEnvelope
	if json.Unmarshal(raw, &env) != nil || env.Channel != "l2Book" {
		return nil
	}

	var book hlL2Book
	if json.Unmarshal(env.Data, &book) != nil {
		return nil
	}

	target, ok := h.symMap[book.Coin]
	if !ok || len(book.Levels) < 2 || book.Time <= 0 || book.Time <= lastPublication[target] {
		return nil
	}
	if book.Time > now.Add(time.Second).UnixMilli() {
		return fmt.Errorf("market %s: future book timestamp", book.Coin)
	}

	bids := book.Levels[0]
	asks := book.Levels[1]
	if len(bids) == 0 || len(asks) == 0 {
		if h.matrix != nil {
			h.matrix.InvalidateBBO(target.exchangeID, target.symbolID)
		}
		return nil
	}

	bidPx, err := strconv.ParseFloat(bids[0].Px, 64)
	if err != nil || math.IsNaN(bidPx) || math.IsInf(bidPx, 0) || bidPx <= 0 {
		log.Printf("hyperliquid: failed to parse bid price: %v", err)
		return nil
	}
	bidSz, err := strconv.ParseFloat(bids[0].Sz, 64)
	if err != nil || math.IsNaN(bidSz) || math.IsInf(bidSz, 0) || bidSz <= 0 {
		log.Printf("hyperliquid: failed to parse bid size: %v", err)
		return nil
	}
	askPx, err := strconv.ParseFloat(asks[0].Px, 64)
	if err != nil || math.IsNaN(askPx) || math.IsInf(askPx, 0) || askPx <= 0 {
		log.Printf("hyperliquid: failed to parse ask price: %v", err)
		return nil
	}
	askSz, err := strconv.ParseFloat(asks[0].Sz, 64)
	if err != nil || math.IsNaN(askSz) || math.IsInf(askSz, 0) || askSz <= 0 {
		log.Printf("hyperliquid: failed to parse ask size: %v", err)
		return nil
	}
	if bidPx >= askPx {
		if h.matrix != nil {
			h.matrix.InvalidateBBO(target.exchangeID, target.symbolID)
		}
		return nil
	}

	tsNs := uint64(book.Time) * 1_000_000 // ms -> ns
	if h.matrix != nil {
		h.matrix.WriteBBO(target.exchangeID, target.symbolID, tsNs, bidPx, bidSz, askPx, askSz)
	}
	lastPublication[target] = book.Time
	return nil
}
