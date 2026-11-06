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
	"github.com/tidwall/gjson"
	"nhooyr.io/websocket"
)

// LighterAccountStats connects to Lighter WebSocket for account statistics
type LighterAccountStats struct {
	cfg         config.ExchangeConfig
	auth        *LighterAuth
	statsWriter *shm.AccountStatsWriter
}

// NewLighterAccountStats creates a new account stats client
func NewLighterAccountStats(cfg config.ExchangeConfig, statsWriter *shm.AccountStatsWriter) (*LighterAccountStats, error) {
	auth, err := LoadLighterAuthFromEnv()
	if err != nil {
		return nil, fmt.Errorf("failed to load Lighter auth: %w", err)
	}

	return &LighterAccountStats{
		cfg:         cfg,
		auth:        auth,
		statsWriter: statsWriter,
	}, nil
}

// lighterUserStats matches the WebSocket response
type lighterUserStats struct {
	Type    string `json:"type"`
	Channel string `json:"channel"`
	Stats   struct {
		Collateral       string `json:"collateral"`
		PortfolioValue   string `json:"portfolio_value"`
		Leverage         string `json:"leverage"`
		AvailableBalance string `json:"available_balance"`
		MarginUsage      string `json:"margin_usage"`
		BuyingPower      string `json:"buying_power"`
	} `json:"stats"`
}

func (las *LighterAccountStats) Run(ctx context.Context) error {
	return RunConnectionLoop(ctx, "lighter-account-stats", las.connect)
}

func (las *LighterAccountStats) connect(ctx context.Context) error {
	c, _, err := websocket.Dial(ctx, las.cfg.WSURL, nil)
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer c.CloseNow()
	c.SetReadLimit(1 << 20)

	log.Printf("lighter-account-stats: connected to %s", las.cfg.WSURL)

	// Generate authentication token (valid for 10 minutes)
	authToken, err := las.auth.CreateAuthToken()
	if err != nil {
		return fmt.Errorf("failed to create auth token: %w", err)
	}

	accountID := las.auth.GetAccountIndex()

	guard := newSubscriptionGuard(ctx, "user_stats", []string{fmt.Sprintf("user_stats/%d", accountID)}, subscriptionTimeout)
	defer guard.cancel()
	// Subscribe to user_stats channel with authentication
	sub := fmt.Sprintf(
		`{"type":"subscribe","channel":"user_stats/%d","auth":"%s"}`,
		accountID,
		authToken,
	)
	if err := c.Write(guard.ctx, websocket.MessageText, []byte(sub)); err != nil {
		return fmt.Errorf("subscribe user_stats: %w", err)
	}
	log.Printf("lighter-account-stats: subscribed to user_stats/%d with auth", accountID)

	stopKeepalive := startWebSocketKeepalive(ctx, "lighter-account-stats", c, 15*time.Second)
	defer stopKeepalive()

	// The previous REST fallback used an invalid account endpoint and produced
	// persistent 403s. For Lighter, the authenticated user_stats stream is the
	// authoritative source here, so keep the path websocket-only until a valid
	// REST account endpoint is implemented.

	// Read loop
	for {
		_, data, err := c.Read(guard.readContext(ctx))
		if err != nil {
			return err
		}

		message := gjson.ParseBytes(data)
		if err := guard.observe(message); err != nil {
			return err
		}
		if message.Get("type").String() == "ping" {
			if err := c.Write(ctx, websocket.MessageText, []byte(`{"type":"pong"}`)); err != nil {
				return fmt.Errorf("write app pong: %w", err)
			}
			continue
		}

		if !guard.acceptsData(message) {
			continue
		}
		var stats lighterUserStats
		if err := json.Unmarshal(data, &stats); err != nil {
			log.Printf("lighter-account-stats: failed to parse message: %v", err)
			continue
		}

		log.Printf("lighter-account-stats: parsed message type=%s channel=%s", stats.Type, stats.Channel)

		if stats.Type == "update/user_stats" || stats.Type == "subscribed/user_stats" {
			las.processStats(&stats)
		}
	}
}

func (las *LighterAccountStats) processStats(stats *lighterUserStats) {
	collateral, err := strconv.ParseFloat(stats.Stats.Collateral, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse collateral: %v", err)
		return
	}
	portfolioValue, err := strconv.ParseFloat(stats.Stats.PortfolioValue, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse portfolio_value: %v", err)
		return
	}
	leverage, err := strconv.ParseFloat(stats.Stats.Leverage, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse leverage: %v", err)
		return
	}
	availableBalance, err := strconv.ParseFloat(stats.Stats.AvailableBalance, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse available_balance: %v", err)
		return
	}
	marginUsage, err := strconv.ParseFloat(stats.Stats.MarginUsage, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse margin_usage: %v", err)
		return
	}
	buyingPower, err := strconv.ParseFloat(stats.Stats.BuyingPower, 64)
	if err != nil {
		log.Printf("lighter-account-stats: failed to parse buying_power: %v", err)
		return
	}

	log.Printf("lighter-account-stats: collateral=$%.2f portfolio=$%.2f leverage=%.2fx available=$%.2f margin=%.1f%% buying_power=$%.2f",
		collateral, portfolioValue, leverage, availableBalance, marginUsage*100, buyingPower)

	// Write to shared memory for Rust to read (including position)
	timestampNs := uint64(time.Now().UnixNano())
	las.statsWriter.WriteStats(
		collateral,
		portfolioValue,
		leverage,
		availableBalance,
		marginUsage,
		buyingPower,
		timestampNs,
	)
}
