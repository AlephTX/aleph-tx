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
	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

type hlTarget struct {
	exchangeID uint8
	symbolID   uint16
}

// Hyperliquid connects to the Hyperliquid L2/BBO WebSocket and routes native
// core coins (ExchangeHyperliquid), Entropy io:* coins (ExchangeEntropy), and
// TradeXYZ xyz:* coins (ExchangeTradeXYZ) into isolated SHM matrix and depth slots.
type Hyperliquid struct {
	wsURL       string
	matrix      *shm.Matrix
	depthWriter *shm.DepthWriter
	symMap      map[string]hlTarget
	fastBook    bool
	bboChannel  bool
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
	return NewHyperliquidWithDepth(cfg, matrix, nil)
}

func NewHyperliquidWithDepth(cfg config.ExchangeConfig, matrix *shm.Matrix, depthWriter *shm.DepthWriter) *Hyperliquid {
	return NewHyperliquidGroupWithDepth(cfg.WSURL, []config.ExchangeConfig{cfg}, matrix, depthWriter)
}

func NewHyperliquidGroup(wsURL string, sections []config.ExchangeConfig, matrix *shm.Matrix) *Hyperliquid {
	return NewHyperliquidGroupWithDepth(wsURL, sections, matrix, nil)
}

func NewHyperliquidGroupWithDepth(
	wsURL string,
	sections []config.ExchangeConfig,
	matrix *shm.Matrix,
	depthWriter *shm.DepthWriter,
) *Hyperliquid {
	symMap := make(map[string]hlTarget)
	fastBook := false
	bboChannel := false
	for _, sec := range sections {
		if sec.FastBook {
			fastBook = true
		}
		if sec.BBOChannel {
			bboChannel = true
		}
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
		wsURL:       wsURL,
		matrix:      matrix,
		depthWriter: depthWriter,
		symMap:      symMap,
		fastBook:    fastBook,
		bboChannel:  bboChannel,
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

type hlBBO struct {
	Coin string     `json:"coin"`
	Time int64      `json:"time"`
	BBO  []*hlLevel `json:"bbo"`
}

type hlLevel struct {
	Px string `json:"px"`
	Sz string `json:"sz"`
}

func (h *Hyperliquid) invalidateBBO(target hlTarget) {
	if h.matrix != nil {
		h.matrix.InvalidateBBO(target.exchangeID, target.symbolID)
	}
}

func (h *Hyperliquid) invalidateTarget(target hlTarget) {
	h.invalidateBBO(target)
	if h.depthWriter != nil {
		h.depthWriter.InvalidateDepth(target.symbolID, target.exchangeID)
	}
}

func (h *Hyperliquid) invalidateAll() {
	for _, target := range h.symMap {
		h.invalidateTarget(target)
	}
}

func (h *Hyperliquid) Run(ctx context.Context) error {
	h.invalidateAll()
	lastBboPub := make(map[hlTarget]int64, len(h.symMap))
	lastDepthPub := make(map[hlTarget]int64, len(h.symMap))
	return RunConnectionLoop(ctx, "hyperliquid", func(ctx context.Context) error {
		return h.connect(ctx, lastBboPub, lastDepthPub)
	})
}

func (h *Hyperliquid) connect(ctx context.Context, lastBboPub, lastDepthPub map[hlTarget]int64) error {
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

	// Subscribe to l2Book (optionally fast top-5) and optional block-driven bbo channel.
	for _, rawCoin := range coins {
		subReq := map[string]any{
			"type": "l2Book",
			"coin": rawCoin,
		}
		if h.fastBook {
			subReq["fast"] = true
		}
		sub := map[string]any{
			"method":       "subscribe",
			"subscription": subReq,
		}
		if err := wsjson.Write(ctx, c, sub); err != nil {
			return fmt.Errorf("subscribe l2Book %s: %w", rawCoin, err)
		}
		if h.bboChannel {
			bboSub := map[string]any{
				"method": "subscribe",
				"subscription": map[string]any{
					"type": "bbo",
					"coin": rawCoin,
				},
			}
			if err := wsjson.Write(ctx, c, bboSub); err != nil {
				return fmt.Errorf("subscribe bbo %s: %w", rawCoin, err)
			}
		}
		log.Printf("hyperliquid: subscribed to %s (fast=%v, bbo=%v)", rawCoin, h.fastBook, h.bboChannel)
	}

	stopKeepalive := wsutil.StartKeepalive(ctx, "hyperliquid", c, 15*time.Second, []byte(`{"method":"ping"}`))
	defer stopKeepalive()

	for {
		var raw json.RawMessage
		if err := wsjson.Read(ctx, c, &raw); err != nil {
			return err
		}
		if err := h.handleMessageWithDepth(raw, time.Now(), lastBboPub, lastDepthPub); err != nil {
			return err
		}
	}
}

func (h *Hyperliquid) handleMessage(raw []byte, now time.Time, lastPublication map[hlTarget]int64) error {
	return h.handleMessageWithDepth(raw, now, lastPublication, lastPublication)
}

func (h *Hyperliquid) handleMessageWithDepth(
	raw []byte,
	now time.Time,
	lastBboPub, lastDepthPub map[hlTarget]int64,
) error {
	var env hlEnvelope
	if json.Unmarshal(raw, &env) != nil {
		return nil
	}
	switch env.Channel {
	case "l2Book":
		return h.handleL2Book(env.Data, now, lastBboPub, lastDepthPub)
	case "bbo":
		return h.handleBBO(env.Data, now, lastBboPub)
	default:
		return nil
	}
}

func (h *Hyperliquid) handleL2Book(
	data json.RawMessage,
	now time.Time,
	lastBboPub, lastDepthPub map[hlTarget]int64,
) error {
	var book hlL2Book
	if json.Unmarshal(data, &book) != nil {
		return nil
	}

	target, ok := h.symMap[book.Coin]
	if !ok || len(book.Levels) < 2 || book.Time <= 0 {
		return nil
	}
	if book.Time > now.Add(time.Second).UnixMilli() {
		return fmt.Errorf("market %s: future book timestamp", book.Coin)
	}
	// Evaluate both streams before updating so aliased test maps work consistently.
	bboFresh := book.Time > lastBboPub[target]
	depthFresh := book.Time > lastDepthPub[target]
	if !bboFresh && !depthFresh {
		return nil
	}

	rawBids := book.Levels[0]
	rawAsks := book.Levels[1]
	if len(rawBids) == 0 || len(rawAsks) == 0 {
		h.invalidateTarget(target)
		if bboFresh {
			lastBboPub[target] = book.Time
		}
		if depthFresh {
			lastDepthPub[target] = book.Time
		}
		return nil
	}

	bids, bidCount, bidOK := parseHyperliquidDepth(rawBids, true)
	asks, askCount, askOK := parseHyperliquidDepth(rawAsks, false)
	if !bidOK || !askOK || bidCount == 0 || askCount == 0 || bids[0].Price >= asks[0].Price {
		h.invalidateTarget(target)
		if bboFresh {
			lastBboPub[target] = book.Time
		}
		if depthFresh {
			lastDepthPub[target] = book.Time
		}
		return nil
	}

	tsNs := uint64(book.Time) * 1_000_000 // ms -> ns
	if bboFresh && h.matrix != nil {
		h.matrix.WriteBBO(target.exchangeID, target.symbolID, tsNs, bids[0].Price, bids[0].Size, asks[0].Price, asks[0].Size)
		lastBboPub[target] = book.Time
	}
	if depthFresh && h.depthWriter != nil {
		h.depthWriter.WriteDepth(target.symbolID, target.exchangeID, tsNs, bids, asks)
		lastDepthPub[target] = book.Time
	}
	return nil
}

func (h *Hyperliquid) handleBBO(
	data json.RawMessage,
	now time.Time,
	lastBboPub map[hlTarget]int64,
) error {
	var ev hlBBO
	if json.Unmarshal(data, &ev) != nil {
		return nil
	}
	target, ok := h.symMap[ev.Coin]
	if !ok || len(ev.BBO) != 2 || ev.Time <= 0 || ev.Time <= lastBboPub[target] {
		return nil
	}
	if ev.Time > now.Add(time.Second).UnixMilli() {
		return fmt.Errorf("market %s: future bbo timestamp", ev.Coin)
	}
	if ev.BBO[0] == nil || ev.BBO[1] == nil {
		h.invalidateBBO(target)
		lastBboPub[target] = ev.Time
		return nil
	}
	bidPx, bidSz, bidOK := parsePositiveLevel(*ev.BBO[0])
	askPx, askSz, askOK := parsePositiveLevel(*ev.BBO[1])
	if !bidOK || !askOK || bidPx >= askPx {
		h.invalidateTarget(target)
		lastBboPub[target] = ev.Time
		return nil
	}
	if h.matrix != nil {
		tsNs := uint64(ev.Time) * 1_000_000 // ms -> ns
		h.matrix.WriteBBO(target.exchangeID, target.symbolID, tsNs, bidPx, bidSz, askPx, askSz)
	}
	lastBboPub[target] = ev.Time
	return nil
}

func parsePositiveLevel(lv hlLevel) (float64, float64, bool) {
	px, errPx := strconv.ParseFloat(lv.Px, 64)
	sz, errSz := strconv.ParseFloat(lv.Sz, 64)
	if errPx != nil || errSz != nil || math.IsNaN(px) || math.IsInf(px, 0) ||
		math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz <= 0 {
		return 0, 0, false
	}
	return px, sz, true
}

func parseHyperliquidDepth(raw []hlLevel, descending bool) ([shm.DepthLevels]shm.PriceLevel, int, bool) {
	var out [shm.DepthLevels]shm.PriceLevel
	count := 0
	for _, lv := range raw {
		if count >= shm.DepthLevels {
			break
		}
		px, errPx := strconv.ParseFloat(lv.Px, 64)
		sz, errSz := strconv.ParseFloat(lv.Sz, 64)
		if errPx != nil || errSz != nil || math.IsNaN(px) || math.IsInf(px, 0) ||
			math.IsNaN(sz) || math.IsInf(sz, 0) || px <= 0 || sz < 0 {
			return out, 0, false
		}
		if sz == 0 {
			continue
		}
		if count > 0 {
			if descending && px >= out[count-1].Price {
				return out, 0, false
			}
			if !descending && px <= out[count-1].Price {
				return out, 0, false
			}
		}
		out[count] = shm.PriceLevel{Price: px, Size: sz}
		count++
	}
	return out, count, true
}
