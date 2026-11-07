package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"log"
	"math"
	"strconv"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/config"
	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
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

	stopKeepalive := wsutil.StartKeepalive(ctx, "lighter-account-stats", c, 15*time.Second, nil)
	defer stopKeepalive()

	refreshCtx, cancelRefresh := context.WithCancel(ctx)
	var refreshDone chan struct{}
	defer func() {
		cancelRefresh()
		if refreshDone != nil {
			<-refreshDone
		}
	}()

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
			if refreshDone == nil && guard.pending == 0 {
				refreshDone = make(chan struct{})
				go func() {
					defer close(refreshDone)
					las.refreshEquity(refreshCtx, accountID)
				}()
			}
		}
	}
}

func (las *LighterAccountStats) refreshEquity(ctx context.Context, account int64) {
	endpoint, err := lighterAccountEndpoint(las.cfg.WSURL, account)
	if err != nil {
		return
	}
	ticker := time.NewTicker(positionPollInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case now := <-ticker.C:
			if !las.statsWriter.EquityNeedsRefresh(uint64(now.UnixNano()), uint64(5*time.Second)) {
				continue
			}
			startedAt := uint64(time.Now().UnixNano())
			snapshot, err := requestLighterAccount(ctx, endpoint, account)
			var stats lighterEquityStats
			if err == nil {
				stats, err = snapshot.equityStats()
			}
			if ctx.Err() != nil {
				return
			}
			if err != nil {
				log.Printf("lighter-account-stats: equity refresh failed: %v", err)
				continue
			}
			// WriteStats ignores a result that started before a newer WS update.
			las.statsWriter.WriteStats(stats.collateral, stats.equity, stats.leverage, stats.available, stats.marginUsage, 0, startedAt)
		}
	}
}

type lighterEquityStats struct {
	collateral, equity, available, leverage, marginUsage float64
}

func (snapshot lighterAccountSnapshot) equityStats() (lighterEquityStats, error) {
	var values [3]float64
	for i, value := range [...]string{snapshot.Collateral, snapshot.TotalAssetValue, snapshot.AvailableBalance} {
		parsed, parseErr := strconv.ParseFloat(value, 64)
		if parseErr != nil || math.IsNaN(parsed) || math.IsInf(parsed, 0) {
			return lighterEquityStats{}, fmt.Errorf("missing or invalid Lighter account equity fields")
		}
		values[i] = parsed
	}
	collateral, equity, available := values[0], values[1], values[2]
	if equity <= 0 {
		return lighterEquityStats{}, fmt.Errorf("non-positive Lighter account equity")
	}
	var grossNotional float64
	for _, position := range snapshot.Positions {
		value, parseErr := strconv.ParseFloat(position.Value, 64)
		if parseErr != nil || math.IsNaN(value) || math.IsInf(value, 0) {
			return lighterEquityStats{}, fmt.Errorf("missing or invalid Lighter position value")
		}
		grossNotional += math.Abs(value)
	}
	return lighterEquityStats{
		collateral:  collateral,
		equity:      equity,
		available:   available,
		leverage:    grossNotional / equity,
		marginUsage: 1 - math.Min(1, math.Max(0, available/equity)),
	}, nil
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
