package exchanges

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"math"
	"net/http"
	"net/url"
	"strconv"
	"time"
)

const positionPollInterval = 2 * time.Second
const privatePositionFreshness = 5 * time.Second

var lighterAccountClient = &http.Client{
	Timeout: 3 * time.Second,
	CheckRedirect: func(*http.Request, []*http.Request) error {
		return http.ErrUseLastResponse
	},
}

type lighterAccountPosition struct {
	Market *int   `json:"market_id"`
	Symbol string `json:"symbol"`
	Size   string `json:"position"`
	Sign   int    `json:"sign"`
	Value  string `json:"position_value"`
}

type lighterAccountSnapshot struct {
	Index            *int64                   `json:"index"`
	Positions        []lighterAccountPosition `json:"positions"`
	Collateral       string                   `json:"collateral"`
	TotalAssetValue  string                   `json:"total_asset_value"`
	AvailableBalance string                   `json:"available_balance"`
}

func lighterAccountEndpoint(wsURL string, account int64) (string, error) {
	ws, err := url.Parse(wsURL)
	if err != nil || ws.Scheme != "wss" || ws.Host == "" {
		return "", fmt.Errorf("invalid Lighter WebSocket URL for account refresh")
	}
	query := url.Values{"by": {"index"}, "value": {strconv.FormatInt(account, 10)}}
	endpoint := url.URL{Scheme: "https", Host: ws.Host, Path: "/api/v1/account", RawQuery: query.Encode()}
	return endpoint.String(), nil
}

// refreshPosition runs only while the authenticated account_market subscription
// is acknowledged. A quiet account can provide its actual position through
// REST even when the private WebSocket does not emit another position update.
func (lp *LighterPrivate) refreshPosition(ctx context.Context) {
	account := lp.auth.GetAccountIndex()
	endpoint, err := lighterAccountEndpoint(lp.cfg.WSURL, account)
	if err != nil {
		return
	}
	ticker := time.NewTicker(positionPollInterval)
	defer ticker.Stop()
	for {
		at := time.Now()
		if lp.statsWriter.PositionNeedsRefresh(uint64(at.UnixNano()), uint64(privatePositionFreshness)) {
			snapshot, err := requestLighterAccount(ctx, endpoint, account)
			if err == nil {
				for market, symbol := range lp.mktMap {
					var position float64
					position, err = snapshot.signedPosition(market, symbol)
					if err != nil || ctx.Err() != nil {
						break
					}
					// Use request start, so a newer WebSocket observation always wins.
					lp.statsWriter.WritePosition(position, uint64(at.UnixNano()))
				}
			}
			if err != nil && ctx.Err() == nil {
				log.Printf("lighter-private: position refresh failed: %v", err)
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

func requestLighterAccount(ctx context.Context, endpoint string, account int64) (lighterAccountSnapshot, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return lighterAccountSnapshot{}, err
	}
	resp, err := lighterAccountClient.Do(req)
	if err != nil {
		return lighterAccountSnapshot{}, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return lighterAccountSnapshot{}, fmt.Errorf("account HTTP %d", resp.StatusCode)
	}
	return decodeLighterAccount(io.LimitReader(resp.Body, 2<<20), account)
}

func decodeLighterAccount(body io.Reader, account int64) (lighterAccountSnapshot, error) {
	var response struct {
		Code     int                      `json:"code"`
		Accounts []lighterAccountSnapshot `json:"accounts"`
	}
	if err := json.NewDecoder(body).Decode(&response); err != nil {
		return lighterAccountSnapshot{}, err
	}
	if response.Code != 200 || len(response.Accounts) != 1 ||
		response.Accounts[0].Index == nil || *response.Accounts[0].Index != account ||
		response.Accounts[0].Positions == nil {
		return lighterAccountSnapshot{}, fmt.Errorf("Lighter account response is missing or mismatched")
	}
	return response.Accounts[0], nil
}

func (snapshot lighterAccountSnapshot) signedPosition(market int, symbol uint16) (float64, error) {
	var signed float64
	found := false
	for _, position := range snapshot.Positions {
		if position.Market == nil {
			return 0, fmt.Errorf("position is missing market ID")
		}
		if *position.Market != market {
			continue
		}
		size, err := strconv.ParseFloat(position.Size, 64)
		if found || SymbolNameToID[position.Symbol] != symbol || err != nil ||
			size < 0 || math.IsNaN(size) || math.IsInf(size, 0) ||
			(position.Sign != -1 && position.Sign != 1 && !(position.Sign == 0 && size == 0)) {
			return 0, fmt.Errorf("invalid position for the configured market")
		}
		found = true
		signed = float64(position.Sign) * size
	}
	return signed, nil
}
