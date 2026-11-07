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

var lighterPositionClient = &http.Client{
	Timeout: 3 * time.Second,
	CheckRedirect: func(*http.Request, []*http.Request) error {
		return http.ErrUseLastResponse
	},
}

// refreshPosition runs only while the authenticated account_market subscription
// is acknowledged. A quiet account can provide its actual position through
// REST even when the private WebSocket does not emit another position update.
func (lp *LighterPrivate) refreshPosition(ctx context.Context) {
	endpoint, err := url.Parse(lp.cfg.WSURL)
	if err != nil || endpoint.Scheme != "wss" {
		return
	}
	account := lp.auth.GetAccountIndex()
	query := url.Values{"by": {"index"}, "value": {strconv.FormatInt(account, 10)}}
	endpoint = &url.URL{Scheme: "https", Host: endpoint.Host, Path: "/api/v1/account", RawQuery: query.Encode()}
	ticker := time.NewTicker(positionPollInterval)
	defer ticker.Stop()
	for {
		at := time.Now()
		if lp.statsWriter.PositionNeedsRefresh(uint64(at.UnixNano()), uint64(privatePositionFreshness)) {
			for market, symbol := range lp.mktMap {
				position, err := requestLighterPosition(ctx, endpoint.String(), account, market, symbol)
				if err != nil {
					if ctx.Err() == nil {
						log.Printf("lighter-private: position refresh failed: %v", err)
					}
					break
				}
				if ctx.Err() == nil {
					// Use request start, so a newer WebSocket observation always wins.
					lp.statsWriter.WritePosition(position, uint64(at.UnixNano()))
				}
			}
		}
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

func requestLighterPosition(ctx context.Context, endpoint string, account int64, market int, symbol uint16) (float64, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return 0, err
	}
	resp, err := lighterPositionClient.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return 0, fmt.Errorf("account HTTP %d", resp.StatusCode)
	}
	return parseLighterPosition(io.LimitReader(resp.Body, 2<<20), account, market, symbol)
}

func parseLighterPosition(body io.Reader, account int64, market int, symbol uint16) (float64, error) {
	type position struct {
		Market *int   `json:"market_id"`
		Symbol string `json:"symbol"`
		Size   string `json:"position"`
		Sign   int    `json:"sign"`
	}
	var response struct {
		Code     int `json:"code"`
		Accounts []struct {
			Index     *int64     `json:"index"`
			Positions []position `json:"positions"`
		} `json:"accounts"`
	}
	if err := json.NewDecoder(body).Decode(&response); err != nil {
		return 0, err
	}
	if response.Code != 200 || len(response.Accounts) != 1 ||
		response.Accounts[0].Index == nil || *response.Accounts[0].Index != account ||
		response.Accounts[0].Positions == nil {
		return 0, fmt.Errorf("account position response is missing or mismatched")
	}
	var signed float64
	found := false
	for _, position := range response.Accounts[0].Positions {
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
