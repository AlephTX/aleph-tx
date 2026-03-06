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

// LighterPrivate connects to Lighter private WebSocket for order/trade events
type LighterPrivate struct {
	cfg          config.ExchangeConfig
	eventBuffer  *shm.EventRingBuffer
	auth         *LighterAuth
	mktMap       map[int]uint16 // market_index -> symbol_id
	accountStats *LighterAccountStats // For updating position
	statsWriter  *shm.AccountStatsWriter // Direct SHM write for instant position updates
}

// NewLighterPrivate creates a new Lighter private stream client
func NewLighterPrivate(
	cfg config.ExchangeConfig,
	eventBuffer *shm.EventRingBuffer,
	accountStats *LighterAccountStats,
	statsWriter *shm.AccountStatsWriter,
) (*LighterPrivate, error) {
	// Load authentication from .env.lighter
	auth, err := LoadLighterAuthFromEnv()
	if err != nil {
		return nil, fmt.Errorf("failed to load Lighter auth: %w", err)
	}

	mktMap := make(map[int]uint16)
	for localSym, exchIdxStr := range cfg.Symbols {
		if id, ok := SymbolNameToID[localSym]; ok {
			idx, _ := strconv.Atoi(exchIdxStr)
			mktMap[idx] = id
		}
	}

	return &LighterPrivate{
		cfg:          cfg,
		eventBuffer:  eventBuffer,
		auth:         auth,
		mktMap:       mktMap,
		accountStats: accountStats,
		statsWriter:  statsWriter,
	}, nil
}

// lighterAccountMarket is the account_market channel response
type lighterAccountMarket struct {
	Type     string              `json:"type"`
	Channel  string              `json:"channel"`
	Account  int                 `json:"account"`
	Orders   []lighterOrder      `json:"orders"`
	Trades   []lighterTrade      `json:"trades"`
	Position json.RawMessage     `json:"position"`
}

// lighterOrder matches the Order JSON from Lighter docs
type lighterOrder struct {
	OrderIndex        int64   `json:"order_index"`
	ClientOrderIndex  int64   `json:"client_order_index"`
	OrderID           string  `json:"order_id"`
	MarketIndex       int     `json:"market_index"`
	InitialBaseAmount string  `json:"initial_base_amount"`
	Price             string  `json:"price"`
	RemainingBaseAmount string `json:"remaining_base_amount"`
	FilledBaseAmount  string  `json:"filled_base_amount"`
	FilledQuoteAmount string  `json:"filled_quote_amount"`
	IsAsk             bool    `json:"is_ask"`
	Status            string  `json:"status"` // "open", "canceled", "filled"
	Timestamp         int64   `json:"timestamp"`
}

// lighterTrade matches the Trade JSON from Lighter docs
type lighterTrade struct {
	TradeID     int64  `json:"trade_id"`
	TxHash      string `json:"tx_hash"`
	Type        string `json:"type"`
	MarketID    int    `json:"market_id"`
	Size        string `json:"size"`
	Price       string `json:"price"`
	USDAmount   string `json:"usd_amount"`
	AskID       int64  `json:"ask_id"`
	BidID       int64  `json:"bid_id"`
	IsMakerAsk  bool   `json:"is_maker_ask"`
	BlockHeight int64  `json:"block_height"`
	Timestamp   int64  `json:"timestamp"`
	TakerFee    int    `json:"taker_fee,omitempty"`
	MakerFee    int    `json:"maker_fee,omitempty"`
}

func (lp *LighterPrivate) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "lighter-private", lp.connect)
}

func (lp *LighterPrivate) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, lp.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20) // 1MB

	log.Printf("lighter-private: connected to %s", lp.cfg.WSURL)

	// Generate authentication token (valid for 10 minutes)
	authToken, err := lp.auth.CreateAuthToken()
	if err != nil {
		return fmt.Errorf("failed to create auth token: %w", err)
	}

	accountID := lp.auth.GetAccountIndex()

	log.Printf("lighter-private: authenticating with account_index=%d, api_key_index=%d",
		accountID, lp.auth.GetAPIKeyIndex())

	// Subscribe to account_market for each configured market
	for mktIdx := range lp.mktMap {
		sub := fmt.Sprintf(
			`{"type":"subscribe","channel":"account_market/%d/%d","auth":"%s"}`,
			mktIdx,
			accountID,
			authToken,
		)
		if err := c.Write(ctx, websocket.MessageText, []byte(sub)); err != nil {
			return fmt.Errorf("subscribe account_market %d: %w", mktIdx, err)
		}
		log.Printf("lighter-private: subscribed to account_market/%d/%d", mktIdx, accountID)
	}

	// Read loop with automatic pong responses
	for {
		msgType, data, err := c.Read(ctx)
		if err != nil {
			return err
		}

		// Handle ping/pong automatically
		if msgType == websocket.MessageBinary || msgType == websocket.MessageText {
			var env lighterAccountMarket
			if json.Unmarshal(data, &env) != nil {
				// Not a valid JSON message, skip
				continue
			}

			// Log position data if present
			if len(env.Position) > 0 && string(env.Position) != "null" {
				log.Printf("lighter-private: position data: %s", string(env.Position))
				lp.processPosition(env.Position)
			}

			// Process orders
			if len(env.Orders) > 0 {
				log.Printf("lighter-private: received %d order event(s)", len(env.Orders))
			}
			for _, order := range env.Orders {
				lp.processOrder(&order)
			}

			// Process trades
			if len(env.Trades) > 0 {
				log.Printf("lighter-private: received %d trade event(s)", len(env.Trades))
			}
			for _, trade := range env.Trades {
				lp.processTrade(&trade)
			}
		}
		// websocket library handles ping/pong automatically
	}
}

func (lp *LighterPrivate) processPosition(positionData json.RawMessage) {
	var pos struct {
		Sign     int    `json:"sign"`     // 1=long, -1=short
		Position string `json:"position"` // Position size
	}
	if err := json.Unmarshal(positionData, &pos); err != nil {
		log.Printf("lighter-private: failed to parse position: %v", err)
		return
	}

	posSize, _ := strconv.ParseFloat(pos.Position, 64)
	netPosition := float64(pos.Sign) * posSize

	log.Printf("lighter-private: position updated: %.4f ETH", netPosition)

	// Update account stats cache (for user_stats to include in full writes)
	if lp.accountStats != nil {
		lp.accountStats.SetPosition(netPosition)
	}

	// Write position directly to SHM for instant Rust visibility
	if lp.statsWriter != nil {
		timestampNs := uint64(time.Now().UnixNano())
		lp.statsWriter.WritePosition(netPosition, timestampNs)
	}
}

func (lp *LighterPrivate) processOrder(order *lighterOrder) {
	symID, ok := lp.mktMap[order.MarketIndex]
	if !ok {
		return
	}

	orderID := uint64(order.OrderIndex)
	log.Printf("lighter-private: order event: id=%d status=%s market=%d", orderID, order.Status, order.MarketIndex)

	switch order.Status {
	case "open":
		// Order created
		initialSize, _ := strconv.ParseFloat(order.InitialBaseAmount, 64)
		lp.eventBuffer.PushOrderCreated(uint8(ExchangeLighter), symID, orderID, initialSize, order.IsAsk)

	case "canceled":
		// Order canceled
		lp.eventBuffer.PushOrderCanceled(uint8(ExchangeLighter), symID, orderID)

	case "filled":
		// Order fully filled (handled by trade events)
		// No action needed here
	}
}

func (lp *LighterPrivate) processTrade(trade *lighterTrade) {
	symID, ok := lp.mktMap[trade.MarketID]
	if !ok {
		return
	}

	log.Printf("lighter-private: trade event: id=%d market=%d price=%s size=%s maker_ask=%v",
		trade.TradeID, trade.MarketID, trade.Price, trade.Size, trade.IsMakerAsk)

	// Determine which order ID belongs to this account
	// TODO: Need to track which orders are ours
	// For now, we'll process both ask and bid
	var orderID uint64
	if trade.IsMakerAsk {
		orderID = uint64(trade.AskID)
	} else {
		orderID = uint64(trade.BidID)
	}

	fillPrice, _ := strconv.ParseFloat(trade.Price, 64)
	fillSize, _ := strconv.ParseFloat(trade.Size, 64)

	// Calculate fee (Lighter uses basis points)
	var feePaid float64
	if trade.IsMakerAsk {
		feePaid = float64(trade.MakerFee) / 10000.0 * fillPrice * fillSize
	} else {
		feePaid = float64(trade.TakerFee) / 10000.0 * fillPrice * fillSize
	}

	// TODO: Get remaining size from order state
	// For now, assume 0 (fully filled)
	remainingSize := 0.0

	lp.eventBuffer.PushOrderFilled(
		uint8(ExchangeLighter),
		symID,
		orderID,
		fillPrice,
		fillSize,
		remainingSize,
		feePaid,
		trade.IsMakerAsk,
	)
}

// GetAccountIndex returns the account index for testing
func (lp *LighterPrivate) GetAccountIndex() int64 {
	return lp.auth.GetAccountIndex()
}

// GetAPIKeyIndex returns the API key index for testing
func (lp *LighterPrivate) GetAPIKeyIndex() uint8 {
	return lp.auth.GetAPIKeyIndex()
}

// Start is an alias for Run for consistency
func (lp *LighterPrivate) Start(ctx context.Context) error {
	return lp.Run(ctx)
}
