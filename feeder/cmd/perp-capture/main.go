// perp-capture records public WebSocket publications and REST snapshots with their receipt times.
package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"slices"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/internal/wsutil"
	"github.com/tidwall/gjson"
	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

type record struct {
	Source        string          `json:"source"`
	Event         string          `json:"event"`
	ReceivedAt    int64           `json:"received_at_ms"`
	AvailableAt   int64           `json:"available_at_ms"`
	RequestSentAt int64           `json:"request_sent_at_ms,omitempty"`
	Market        string          `json:"market,omitempty"`
	Payload       json.RawMessage `json:"payload,omitempty"`
}

type lighterSubscription struct {
	Type    string `json:"type"`
	Channel string `json:"channel"`
}

type subscription struct {
	source, url  string
	market       string
	requests     []any
	coins        []string
	interval     time.Duration
	clockRefresh time.Duration
}

var errClockRefresh = errors.New("refresh Lighter subscription clock evidence")

func symbols(raw string) ([]string, error) {
	if raw == "" {
		return nil, nil
	}
	items := strings.Split(raw, ",")
	if len(items) > 8 {
		return nil, fmt.Errorf("at most eight markets per source")
	}
	seen := make(map[string]bool)
	for _, item := range items {
		if len(item) == 0 || len(item) > 40 || strings.ContainsAny(item, " \t\n") || seen[item] {
			return nil, fmt.Errorf("invalid or duplicate market %q", item)
		}
		seen[item] = true
	}
	return items, nil
}

func perpetualSymbols(raw string) ([]string, error) {
	coins, err := symbols(raw)
	if err != nil {
		return nil, err
	}
	for _, coin := range coins {
		if strings.IndexFunc(coin, func(r rune) bool {
			return !('A' <= r && r <= 'Z' || 'a' <= r && r <= 'z' ||
				'0' <= r && r <= '9' || r == '-' || r == '_')
		}) >= 0 {
			return nil, fmt.Errorf("invalid perpetual market %q", coin)
		}
	}
	return coins, nil
}

func subscriptions(lighter, hl, coinbase, arcus, bulk, arcusWS, bulkWS string, snapshotInterval, clockRefresh time.Duration, hlFastBook bool) ([]subscription, error) {
	sources := []subscription{
		{source: "lighter", url: "wss://mainnet.zklighter.elliot.ai/stream", clockRefresh: clockRefresh},
		{source: "hyperliquid", url: "wss://api.hyperliquid.xyz/ws"},
		{source: "coinbase", url: "wss://ws-feed.exchange.coinbase.com"},
	}
	for i, raw := range []string{lighter, hl, coinbase} {
		items, err := symbols(raw)
		if err != nil {
			return nil, err
		}
		if len(items) == 0 {
			continue
		}
		source := &sources[i]
		source.coins = items
		for _, item := range items {
			switch source.source {
			case "lighter":
				id, err := strconv.ParseUint(item, 10, 16)
				if err != nil {
					return nil, fmt.Errorf("lighter market ID %q: %w", item, err)
				}
				for _, channel := range []string{"order_book", "market_stats"} {
					source.requests = append(source.requests, lighterSubscription{
						Type: "subscribe", Channel: fmt.Sprintf("%s/%d", channel, id),
					})
				}
			case "hyperliquid":
				for _, channel := range []string{"l2Book", "activeAssetCtx"} {
					request := map[string]any{"type": channel, "coin": item}
					if channel == "l2Book" && hlFastBook {
						request["fast"] = true
					}
					source.requests = append(source.requests, map[string]any{
						"method": "subscribe", "subscription": request,
					})
				}
			case "coinbase":
				source.requests = []any{map[string]any{
					"type": "subscribe", "product_ids": items, "channels": []string{"ticker"},
				}}
			}
		}
	}
	var enabled []subscription
	for _, source := range sources {
		if len(source.requests) != 0 {
			enabled = append(enabled, source)
		}
	}
	if snapshotInterval != 0 {
		coins, err := symbols(hl)
		if err != nil {
			return nil, err
		}
		if len(coins) == 0 || len(coins) > 4 || snapshotInterval < time.Second {
			return nil, errors.New("Hyperliquid snapshots need one to four coins and at least a 1s interval")
		}
		enabled = append(enabled, subscription{
			source: "hyperliquid_snapshot", url: "https://api.hyperliquid.xyz/info",
			coins: coins, interval: snapshotInterval,
		})
	}
	for _, rest := range []struct{ markets, source, url string }{
		{arcus, "arcus_snapshot", "https://api.arcus.xyz/v1/l2OrderBook"},
		{bulk, "bulk_snapshot", "https://mainnet-api1.bulk.trade/api/v1/l2book"},
	} {
		coins, err := perpetualSymbols(rest.markets)
		if err != nil {
			return nil, err
		}
		if len(coins) > 4 {
			return nil, fmt.Errorf("%s supports at most four markets", rest.source)
		}
		if len(coins) > 0 {
			enabled = append(enabled, subscription{
				source: rest.source, url: rest.url, coins: coins, interval: time.Second,
			})
		}
	}
	for _, ws := range []struct{ markets, name, url string }{
		{arcusWS, "arcus", "wss://api.arcus.xyz/v1/ws"},
		{bulkWS, "bulk", "wss://mainnet-ws1.bulk.trade"},
	} {
		coins, err := perpetualSymbols(ws.markets)
		if err != nil {
			return nil, err
		}
		if len(coins) == 0 {
			continue
		}
		source := subscription{source: ws.name, url: ws.url, coins: coins}
		if ws.name == "arcus" {
			for _, coin := range coins {
				source.requests = append(source.requests, map[string]any{
					"type": "subscribe", "channel": "l2Orderbook", "id": coin, "nLevels": 20,
				})
			}
		} else {
			books := make([]map[string]any, 0, len(coins))
			for _, coin := range coins {
				books = append(books, map[string]any{
					"type": "l2Snapshot", "symbol": coin, "nlevels": 20,
				})
			}
			source.requests = []any{map[string]any{"method": "subscribe", "subscription": books}}
		}
		enabled = append(enabled, source)
	}
	return enabled, nil
}

func pollSnapshots(ctx context.Context, source subscription, emit func(record) error) error {
	client := &http.Client{
		Timeout:       5 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	defer client.CloseIdleConnections()
	ctx, cancel := context.WithCancelCause(ctx)
	defer cancel(nil)
	var workers sync.WaitGroup
	for _, coin := range source.coins {
		workers.Go(func() {
			cancel(pollSnapshotCoin(ctx, source, coin, client, emit))
		})
	}
	workers.Wait()
	return context.Cause(ctx)
}

func snapshotRequest(ctx context.Context, source subscription, coin string, sent int64) (*http.Request, error) {
	switch source.source {
	case "hyperliquid_funding":
		return hyperliquidFundingRequest(ctx, source.url, coin, sent)
	case "hyperliquid_snapshot":
		query, err := json.Marshal(map[string]string{"type": "l2Book", "coin": coin})
		if err != nil {
			return nil, err
		}
		req, err := http.NewRequestWithContext(ctx, http.MethodPost, source.url, bytes.NewReader(query))
		if err == nil {
			req.Header.Set("Content-Type", "application/json")
		}
		return req, err
	case "arcus_snapshot":
		endpoint := source.url + "/" + url.PathEscape(coin) + "?nLevels=20"
		return http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	case "bulk_snapshot":
		query := url.Values{"type": {"l2book"}, "coin": {coin}, "nlevels": {"20"}}
		return http.NewRequestWithContext(ctx, http.MethodGet, source.url+"?"+query.Encode(), nil)
	default:
		return nil, fmt.Errorf("unsupported snapshot source %q", source.source)
	}
}

type snapshotPayload struct {
	Channel string          `json:"channel,omitempty"`
	Market  string          `json:"market,omitempty"`
	Data    json.RawMessage `json:"data"`
}

func pollSnapshotCoin(ctx context.Context, source subscription, coin string, client *http.Client, emit func(record) error) error {
	ticker := time.NewTicker(source.interval)
	defer ticker.Stop()
	for {
		sent := time.Now().UnixMilli()
		request, err := snapshotRequest(ctx, source, coin, sent)
		if err != nil {
			return err
		}
		response, err := client.Do(request)
		if err != nil {
			return fmt.Errorf("snapshot %s: %w", coin, err)
		}
		body, readErr := io.ReadAll(io.LimitReader(response.Body, (1<<20)+1))
		received := time.Now().UnixMilli()
		closeErr := response.Body.Close()
		if err := errors.Join(readErr, closeErr); err != nil {
			return err
		}
		if response.StatusCode != http.StatusOK || len(body) > 1<<20 {
			return fmt.Errorf("invalid %s for %s: HTTP %d", source.source, coin, response.StatusCode)
		}
		if source.source == "hyperliquid_funding" {
			if err := emitHyperliquidFunding(source, coin, sent, received, body, emit); err != nil {
				return err
			}
		} else {
			snapshot := snapshotPayload{Data: body}
			if source.source == "hyperliquid_snapshot" {
				snapshot.Channel = "l2Book"
			} else {
				// Arcus snapshots do not echo the symbol. Archive the requested market.
				snapshot.Market = coin
			}
			payload, err := json.Marshal(snapshot)
			if err != nil {
				return err
			}
			if err := emit(record{Source: source.source, Event: "message", ReceivedAt: received, Payload: payload}); err != nil {
				return err
			}
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-ticker.C:
		}
	}
}

func connect(ctx context.Context, source subscription, emit func(record) error) error {
	dialCtx, cancel := context.WithTimeout(ctx, 8*time.Second)
	conn, _, err := websocket.Dial(dialCtx, source.url, &websocket.DialOptions{
		HTTPHeader: http.Header{"User-Agent": {"AlephTX-public-capture/1.0"}},
	})
	cancel()
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.CloseNow()
	conn.SetReadLimit(1 << 20)
	if source.isHyperliquidWS() {
		if err := emit(record{Source: source.source, Market: source.market, Event: "connected", ReceivedAt: time.Now().UnixMilli()}); err != nil {
			return err
		}
	}
	subscriptionSends := make(map[string]time.Time)
	for _, request := range source.requests {
		if sub, ok := request.(lighterSubscription); ok {
			subscriptionSends[sub.Channel] = time.Now()
		}
		if err := wsjson.Write(ctx, conn, request); err != nil {
			return fmt.Errorf("subscribe: %w", err)
		}
	}
	var heartbeat []byte
	if source.isHyperliquidWS() {
		heartbeat = []byte(`{"method":"ping"}`)
	}
	stopKeepalive := wsutil.StartKeepalive(ctx, source.source, conn, 15*time.Second, heartbeat)
	defer stopKeepalive()

	nonces := make(map[string]uint64)
	var extendedSequence uint64
	acknowledged := make(map[string]bool)
	for {
		readCtx, cancelRead := ctx, func() {}
		if source.source != "extended_trades" && source.source != "hyperliquid_trades" {
			readCtx, cancelRead = context.WithTimeout(ctx, 30*time.Second)
		}
		// An inactive market can have no trades while the transport is healthy.
		// The keepalive closes an unresponsive connection and wakes this read.
		_, raw, err := conn.Read(readCtx)
		received := time.Now()
		cancelRead()
		if err != nil {
			return fmt.Errorf("read: %w", err)
		}
		var header struct {
			Type    string `json:"type"`
			Channel string `json:"channel"`
			Book    *struct {
				Nonce      uint64 `json:"nonce"`
				BeginNonce uint64 `json:"begin_nonce"`
			} `json:"order_book"`
		}
		if err := json.Unmarshal(raw, &header); err != nil {
			return fmt.Errorf("decode: %w", err)
		}
		if header.Type == "ping" {
			if err := wsjson.Write(ctx, conn, map[string]string{"type": "pong"}); err != nil {
				return fmt.Errorf("pong: %w", err)
			}
			continue
		}
		if header.Type == "error" || header.Channel == "error" {
			return errors.New("public subscription rejected")
		}
		switch source.source {
		case "hyperliquid", "hyperliquid_trades":
			publication, err := hyperliquidPublication(source, header.Channel, raw, acknowledged)
			if err != nil {
				return err
			}
			if publication.Event != "" {
				publication.ReceivedAt = received.UnixMilli()
				if err := emit(publication); err != nil {
					return err
				}
			}
			continue
		case "extended", "extended_trades":
			next, err := extendedPublication(raw, source, extendedSequence)
			if err != nil {
				return err
			}
			extendedSequence = next
		case "lighter_trades", "lighter_robinhood_trades":
			if header.Type != "subscribed/trade" && header.Type != "update/trade" {
				continue
			}
			raw, err = publicLighterTrades(raw)
			if err != nil {
				return err
			}
		case "arcus":
			if header.Channel != "l2Orderbook" || !slices.Contains(source.coins, gjson.GetBytes(raw, "id").String()) ||
				(header.Type != "subscribed" && header.Type != "channel_data") {
				continue
			}
		case "bulk":
			// The server can append depth parameters to the topic. The
			// snapshot carries the native market identity independently.
			if header.Type != "l2Snapshot" ||
				!slices.Contains(source.coins, gjson.GetBytes(raw, "data.book.symbol").String()) {
				continue
			}
		default:
			switch header.Type {
			case "subscribed/order_book", "update/order_book", "subscribed/market_stats", "update/market_stats", "ticker":
			default:
				if header.Channel != "l2Book" && header.Channel != "activeAssetCtx" {
					continue
				}
			}
		}
		row := record{Source: source.source, Market: source.market, Event: "message", ReceivedAt: received.UnixMilli(), Payload: raw}
		if isLighterBookSource(source.source) && strings.HasPrefix(header.Type, "subscribed/") {
			channel := strings.Replace(header.Channel, ":", "/", 1)
			if sent, ok := subscriptionSends[channel]; ok {
				delete(subscriptionSends, channel)
				// A clock step between send and receive invalidates the bracket.
				wallElapsed := received.UnixMilli() - sent.UnixMilli()
				monotonicElapsed := received.Sub(sent).Milliseconds()
				if wallElapsed >= 0 && wallElapsed >= monotonicElapsed-1 && wallElapsed <= monotonicElapsed+1 {
					row.RequestSentAt = sent.UnixMilli()
				}
			}
		}
		if err := emit(row); err != nil {
			return err
		}
		// Record the gap before reconnecting so the compiler invalidates the
		// previous book. A new connection starts with a fresh snapshot.
		if isLighterBookSource(source.source) && (header.Type == "subscribed/order_book" || header.Type == "update/order_book") {
			if header.Book == nil || header.Book.Nonce == 0 {
				return errors.New("missing Lighter book nonce")
			}
			prior := nonces[header.Channel]
			if header.Type == "subscribed/order_book" {
				nonces[header.Channel] = header.Book.Nonce
			} else if header.Book.Nonce > prior {
				if prior == 0 || header.Book.BeginNonce != prior {
					return fmt.Errorf("Lighter book nonce gap on %s", header.Channel)
				}
				nonces[header.Channel] = header.Book.Nonce
			}
		}
	}
}

func runSource(ctx context.Context, source subscription, emit func(record) error) error {
	if source.interval > 0 {
		return pollSnapshots(ctx, source, emit)
	}
	if source.clockRefresh == 0 {
		return connect(ctx, source, emit)
	}
	ctx, cancel := context.WithTimeoutCause(ctx, source.clockRefresh, errClockRefresh)
	defer cancel()
	err := connect(ctx, source, emit)
	if errors.Is(context.Cause(ctx), errClockRefresh) {
		return errClockRefresh
	}
	return err
}

func capture(ctx context.Context, sources []subscription, out syncWriter) error {
	ctx, cancel := context.WithCancelCause(ctx)
	defer cancel(nil)
	queue := make(chan record, 4096)
	emit := func(row record) error {
		select {
		case queue <- row:
			return nil
		case <-ctx.Done():
			return context.Cause(ctx)
		default:
			err := errors.New("capture queue full, recording incomplete")
			cancel(err)
			return err
		}
	}
	var workers sync.WaitGroup
	for _, source := range sources {
		workers.Go(func() {
			backoff := time.Second
			for ctx.Err() == nil {
				start := time.Now()
				err := runSource(ctx, source, emit)
				if ctx.Err() != nil {
					return
				}
				log.Printf("%s: %v", source.source, err)
				event := "disconnect"
				if errors.Is(err, errClockRefresh) {
					event = "clock_refresh"
				}
				if emit(record{Source: source.source, Market: source.market, Event: event, ReceivedAt: time.Now().UnixMilli()}) != nil {
					return
				}
				if event == "clock_refresh" {
					backoff = time.Second
					continue
				}
				if time.Since(start) > 30*time.Second {
					backoff = time.Second
				}
				timer := time.NewTimer(backoff)
				select {
				case <-ctx.Done():
					timer.Stop()
					return
				case <-timer.C:
					backoff = min(15*time.Second, 2*backoff)
				}
			}
		})
	}
	go func() { workers.Wait(); close(queue) }()

	writer := bufio.NewWriterSize(out, 256*1024)
	encoder := json.NewEncoder(writer)
	var last int64
	counts := make(map[string]int)
	write := func(row record) error {
		row.AvailableAt = time.Now().UnixMilli()
		if row.AvailableAt < last || row.AvailableAt < row.ReceivedAt {
			return errors.New("wall clock moved backwards during capture")
		}
		last = row.AvailableAt
		return encoder.Encode(row)
	}
	for row := range queue {
		if err := write(row); err != nil {
			cancel(err)
			return err
		}
		if row.Event == "message" || row.Source == "hyperliquid_trades" && row.Event == "subscribed" {
			counts[row.Source+"\x00"+row.Market]++
		}
	}
	if err := context.Cause(ctx); err != nil &&
		!errors.Is(err, context.DeadlineExceeded) && !errors.Is(err, context.Canceled) {
		return err
	}
	for _, source := range sources {
		market := source.market
		if source.source == "hyperliquid" || source.source == "hyperliquid_funding" {
			for _, coin := range source.coins {
				if counts[source.source+"\x00"+coin] == 0 {
					return fmt.Errorf("no publications from %s %s", source.source, coin)
				}
			}
			continue
		}
		if counts[source.source+"\x00"+market] == 0 {
			return fmt.Errorf("no publications from %s %s", source.source, source.market)
		}
	}
	if err := write(record{Source: "collector", Event: "end", ReceivedAt: time.Now().UnixMilli()}); err != nil {
		return err
	}
	if err := writer.Flush(); err != nil {
		return err
	}
	if err := out.Sync(); err != nil {
		return err
	}
	log.Printf("recorded publications %v", counts)
	return nil
}

func run() error {
	lighter := flag.String("lighter", "", "comma-separated Lighter market IDs")
	rh := flag.String("lighter-robinhood", "", "comma-separated Robinhood Chain Lighter market IDs (independent domain)")
	rhTrades := flag.Bool("lighter-robinhood-trades", false, "record Robinhood Chain Lighter public trades")
	lighterTrades := flag.Bool("lighter-trades", false, "record Lighter public trades on a separate connection")
	extended := flag.String("extended", "", "comma-separated active Extended CLOB perpetuals for public depth and trades")
	hl := flag.String("hyperliquid", "", "comma-separated Hyperliquid core or HIP-3 coins")
	hlTrades := flag.Bool("hl-trades", false, "record Hyperliquid public trades and subscription acknowledgments")
	hlFunding := flag.Duration("hl-funding-interval", 0, "poll public Hyperliquid funding history (minimum 1m)")
	coinbase := flag.String("coinbase", "", "comma-separated Coinbase spot products")
	arcus := flag.String("arcus", "", "comma-separated Arcus perpetuals for 1s REST snapshots")
	bulk := flag.String("bulk", "", "comma-separated BULK perpetuals for 1s REST snapshots")
	arcusWS := flag.String("arcus-ws", "", "comma-separated Arcus perpetuals for WebSocket L2 books")
	bulkWS := flag.String("bulk-ws", "", "comma-separated BULK perpetuals for WebSocket L2 snapshots")
	snapshotInterval := flag.Duration("hl-snapshot-interval", 0, "optional Hyperliquid REST L2 interval")
	hlFastBook := flag.Bool("hl-fast-book", false, "request Hyperliquid's fast five-level WebSocket book")
	clockRefresh := flag.Duration("lighter-clock-refresh", 0, "renew the Lighter connection and clock evidence periodically")
	duration := flag.Duration("duration", time.Minute, "recording duration")
	path := flag.String("output", "", "new output NDJSON file")
	compressed := flag.Bool("gzip", false, "stream gzip-compressed NDJSON to the output file")
	flag.Parse()
	if *path == "" || *duration < time.Second || *duration > 24*time.Hour {
		return errors.New("provide -output and a duration from 1s to 24h")
	}
	if *clockRefresh != 0 && ((*lighter == "" && *rh == "") || *clockRefresh < 30*time.Second || *clockRefresh > 9*time.Minute) {
		return errors.New("clock refresh needs Lighter markets and an interval from 30s to 9m")
	}
	if *hlFastBook && *hl == "" {
		return errors.New("fast books need Hyperliquid markets")
	}
	sources, err := subscriptions(*lighter, *hl, *coinbase, *arcus, *bulk, *arcusWS, *bulkWS, *snapshotInterval, *clockRefresh, *hlFastBook)
	if err != nil {
		return err
	}
	hlSources, err := hyperliquidExtraSources(*hl, *hlTrades, *hlFunding)
	if err != nil {
		return err
	}
	sources = append(sources, hlSources...)
	rhSources, err := robinhoodSubscriptions(*rh, *rhTrades, *clockRefresh)
	if err != nil {
		return err
	}
	if *rhTrades && *rh == "" {
		return errors.New("Robinhood trades require Robinhood market IDs")
	}
	sources = append(sources, rhSources...)
	if *lighterTrades {
		source := subscription{source: "lighter_trades", url: "wss://mainnet.zklighter.elliot.ai/stream"}
		for _, raw := range strings.Split(*lighter, ",") {
			id, err := strconv.ParseUint(raw, 10, 16)
			if err != nil {
				return errors.New("Lighter trades require Lighter market IDs")
			}
			source.requests = append(source.requests, lighterSubscription{Type: "subscribe", Channel: fmt.Sprintf("trade/%d", id)})
		}
		sources = append(sources, source)
	}
	if *extended != "" {
		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		extra, err := extendedSubscriptions(ctx, *extended)
		cancel()
		if err != nil {
			return err
		}
		sources = append(sources, extra...)
	}
	if len(sources) == 0 {
		return errors.New("configure at least one public market")
	}
	out, err := openCaptureOutput(*path, *compressed)
	if err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ctx, cancel := context.WithTimeout(ctx, *duration)
	defer cancel()
	return errors.Join(capture(ctx, sources, out), out.Close())
}

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}
