// perp-capture records public WebSocket publications with their receipt times.
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
	"os"
	"os/signal"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"nhooyr.io/websocket"
	"nhooyr.io/websocket/wsjson"
)

type record struct {
	Source      string          `json:"source"`
	Event       string          `json:"event"`
	ReceivedAt  int64           `json:"received_at_ms"`
	AvailableAt int64           `json:"available_at_ms"`
	Payload     json.RawMessage `json:"payload,omitempty"`
}

type subscription struct {
	source, url string
	requests    []any
	coins       []string
	interval    time.Duration
}

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

func subscriptions(lighter, hl, coinbase string, snapshotInterval time.Duration) ([]subscription, error) {
	sources := []subscription{
		{source: "lighter", url: "wss://mainnet.zklighter.elliot.ai/stream"},
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
		for _, item := range items {
			switch source.source {
			case "lighter":
				id, err := strconv.ParseUint(item, 10, 16)
				if err != nil {
					return nil, fmt.Errorf("lighter market ID %q: %w", item, err)
				}
				for _, channel := range []string{"order_book", "market_stats"} {
					source.requests = append(source.requests, map[string]string{
						"type": "subscribe", "channel": fmt.Sprintf("%s/%d", channel, id),
					})
				}
			case "hyperliquid":
				for _, channel := range []string{"l2Book", "activeAssetCtx"} {
					source.requests = append(source.requests, map[string]any{
						"method": "subscribe", "subscription": map[string]string{"type": channel, "coin": item},
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
	if len(enabled) == 0 {
		return nil, errors.New("configure at least one public market")
	}
	return enabled, nil
}

func pollSnapshots(ctx context.Context, source subscription, emit func(record) error) error {
	client := &http.Client{
		Timeout:       5 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	defer client.CloseIdleConnections()
	ticker := time.NewTicker(source.interval)
	defer ticker.Stop()
	for {
		for _, coin := range source.coins {
			query, err := json.Marshal(map[string]string{"type": "l2Book", "coin": coin})
			if err != nil {
				return err
			}
			request, err := http.NewRequestWithContext(ctx, http.MethodPost, source.url, bytes.NewReader(query))
			if err != nil {
				return err
			}
			request.Header.Set("Content-Type", "application/json")
			response, err := client.Do(request)
			if err != nil {
				return err
			}
			body, readErr := io.ReadAll(io.LimitReader(response.Body, (1<<20)+1))
			received := time.Now().UnixMilli()
			closeErr := response.Body.Close()
			if err := errors.Join(readErr, closeErr); err != nil {
				return err
			}
			if response.StatusCode != http.StatusOK || len(body) > 1<<20 {
				return fmt.Errorf("invalid Hyperliquid snapshot response: HTTP %d", response.StatusCode)
			}
			payload, err := json.Marshal(struct {
				Channel string          `json:"channel"`
				Data    json.RawMessage `json:"data"`
			}{"l2Book", body})
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
	conn, _, err := websocket.Dial(dialCtx, source.url, nil)
	cancel()
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.CloseNow()
	conn.SetReadLimit(1 << 20)
	for _, request := range source.requests {
		if err := wsjson.Write(ctx, conn, request); err != nil {
			return fmt.Errorf("subscribe: %w", err)
		}
	}
	nonces := make(map[string]uint64)
	for {
		readCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
		_, raw, err := conn.Read(readCtx)
		received := time.Now().UnixMilli()
		cancel()
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
			return fmt.Errorf("subscription rejected: %.240s", raw)
		}
		switch header.Type {
		case "subscribed/order_book", "update/order_book", "subscribed/market_stats", "update/market_stats", "ticker":
		default:
			if header.Channel != "l2Book" && header.Channel != "activeAssetCtx" {
				continue
			}
		}
		if err := emit(record{Source: source.source, Event: "message", ReceivedAt: received, Payload: raw}); err != nil {
			return err
		}
		// Record the gap before reconnecting so the compiler invalidates the
		// previous book. A new connection starts with a fresh snapshot.
		if source.source == "lighter" && (header.Type == "subscribed/order_book" || header.Type == "update/order_book") {
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

func capture(ctx context.Context, sources []subscription, out *os.File) error {
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
				var err error
				if source.interval > 0 {
					err = pollSnapshots(ctx, source, emit)
				} else {
					err = connect(ctx, source, emit)
				}
				if ctx.Err() != nil {
					return
				}
				log.Printf("%s: %v", source.source, err)
				if emit(record{Source: source.source, Event: "disconnect", ReceivedAt: time.Now().UnixMilli()}) != nil {
					return
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
		if row.Event == "message" {
			counts[row.Source]++
		}
	}
	if err := context.Cause(ctx); err != nil &&
		!errors.Is(err, context.DeadlineExceeded) && !errors.Is(err, context.Canceled) {
		return err
	}
	for _, source := range sources {
		if counts[source.source] == 0 {
			return fmt.Errorf("no publications from %s", source.source)
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
	hl := flag.String("hyperliquid", "", "comma-separated Hyperliquid core or HIP-3 coins")
	coinbase := flag.String("coinbase", "", "comma-separated Coinbase spot products")
	snapshotInterval := flag.Duration("hl-snapshot-interval", 0, "optional Hyperliquid REST L2 interval")
	duration := flag.Duration("duration", time.Minute, "recording duration")
	path := flag.String("output", "", "new output NDJSON file")
	flag.Parse()
	if *path == "" || *duration < time.Second || *duration > 24*time.Hour {
		return errors.New("provide -output and a duration from 1s to 24h")
	}
	sources, err := subscriptions(*lighter, *hl, *coinbase, *snapshotInterval)
	if err != nil {
		return err
	}
	out, err := os.OpenFile(*path, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
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
