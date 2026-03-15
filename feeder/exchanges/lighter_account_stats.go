package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/shm"
	"nhooyr.io/websocket"
)

// LighterAccountStats connects to Lighter WebSocket for account statistics
type LighterAccountStats struct {
	cfg         config.ExchangeConfig
	auth        *LighterAuth
	statsWriter *shm.AccountStatsWriter
	position    float64 // Cached position from private stream
}

// NewLighterAccountStats creates a new account stats client
func NewLighterAccountStats(cfg config.ExchangeConfig, statsWriter *shm.AccountStatsWriter) (*LighterAccountStats, error) {
	auth, err := LoadLighterAuthFromEnv()
	if err != nil {
		return nil, fmt.Errorf("failed to load Lighter auth: %w", err)
	}

	return &LighterAccountStats{
		cfg:        cfg,
		auth:       auth,
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

	// Subscribe to user_stats channel with authentication
	sub := fmt.Sprintf(
		`{"type":"subscribe","channel":"user_stats/%d","auth":"%s"}`,
		accountID,
		authToken,
	)
	if err := c.Write(ctx, websocket.MessageText, []byte(sub)); err != nil {
		return fmt.Errorf("subscribe user_stats: %w", err)
	}
	log.Printf("lighter-account-stats: subscribed to user_stats/%d with auth", accountID)

	// Fetch initial stats via REST API (as fallback)
	go las.fetchInitialStats(ctx)

	// Start periodic polling as fallback (every 30 seconds)
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()

	// Create a done channel to signal goroutine cleanup
	done := make(chan struct{})
	defer close(done)

	go func() {
		for {
			select {
			case <-ctx.Done():
				return
			case <-done:
				return
			case <-ticker.C:
				las.fetchStatsREST(ctx)
			}
		}
	}()

	// Read loop
	for {
		_, data, err := c.Read(ctx)
		if err != nil {
			return err
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

	log.Printf("lighter-account-stats: collateral=$%.2f portfolio=$%.2f leverage=%.2fx available=$%.2f margin=%.1f%% buying_power=$%.2f position=%.4f",
		collateral, portfolioValue, leverage, availableBalance, marginUsage*100, buyingPower, las.position)

	// Write to shared memory for Rust to read (including position)
	timestampNs := uint64(time.Now().UnixNano())
	las.statsWriter.WriteStatsWithPosition(
		collateral,
		portfolioValue,
		leverage,
		availableBalance,
		marginUsage,
		buyingPower,
		las.position,
		timestampNs,
	)
}

// SetPosition updates the cached position (called from private stream)
func (las *LighterAccountStats) SetPosition(position float64) {
	las.position = position
}

func (las *LighterAccountStats) fetchInitialStats(ctx context.Context) {
	time.Sleep(1 * time.Second) // Wait for subscription to settle
	las.fetchStatsREST(ctx)
}

func (las *LighterAccountStats) fetchStatsREST(ctx context.Context) {
	accountID := las.auth.GetAccountIndex()

	// Try the account endpoint
	url := fmt.Sprintf("https://mainnet.zklighter.elliot.ai/api/v1/accounts/%d", accountID)

	req, err := http.NewRequestWithContext(ctx, "GET", url, nil)
	if err != nil {
		return
	}
	req.Header.Set("User-Agent", "AlephTX/5.0")
	req.Header.Set("Accept", "application/json")

	client := &http.Client{Timeout: 5 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		log.Printf("lighter-account-stats: REST fetch failed: %v", err)
		return
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		body, _ := io.ReadAll(resp.Body)
		log.Printf("lighter-account-stats: REST returned %d: %s", resp.StatusCode, string(body))
		return
	}

	var result struct {
		Code int `json:"code"`
		Data struct {
			Collateral       string `json:"collateral"`
			PortfolioValue   string `json:"portfolio_value"`
			Leverage         string `json:"leverage"`
			AvailableBalance string `json:"available_balance"`
			MarginUsage      string `json:"margin_usage"`
			BuyingPower      string `json:"buying_power"`
		} `json:"data"`
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return
	}

	if err := json.Unmarshal(body, &result); err != nil {
		log.Printf("lighter-account-stats: REST parse failed: %v (body: %s)", err, string(body))
		return
	}

	if result.Code != 0 {
		log.Printf("lighter-account-stats: REST returned code %d", result.Code)
		return
	}

	// Convert to WebSocket format and process
	stats := &lighterUserStats{
		Type:    "update/user_stats",
		Channel: fmt.Sprintf("user_stats/%d", accountID),
	}
	stats.Stats.Collateral = result.Data.Collateral
	stats.Stats.PortfolioValue = result.Data.PortfolioValue
	stats.Stats.Leverage = result.Data.Leverage
	stats.Stats.AvailableBalance = result.Data.AvailableBalance
	stats.Stats.MarginUsage = result.Data.MarginUsage
	stats.Stats.BuyingPower = result.Data.BuyingPower

	las.processStats(stats)
}
