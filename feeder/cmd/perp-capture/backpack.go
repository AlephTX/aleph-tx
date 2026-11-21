package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"strings"
	"time"
)

const (
	backpackStream   = "wss://ws.backpack.exchange"
	backpackDepthURL = "https://api.backpack.exchange/api/v1/depth"
)

// Each Backpack perpetual has its own connection, depth sequence and reconnect
// epoch. Depth messages are deltas; every connection archives one REST depth
// snapshot after subscribing so an offline compiler can rebuild the book from
// lastUpdateId and the first delta whose U..u range covers lastUpdateId+1.
func backpackSubscriptions(raw string) ([]subscription, error) {
	markets, err := symbols(raw)
	if err != nil {
		return nil, err
	}
	var feeds []subscription
	for _, market := range markets {
		if !strings.HasSuffix(market, "_USDC_PERP") || strings.IndexFunc(market, func(r rune) bool {
			return !('A' <= r && r <= 'Z' || '0' <= r && r <= '9' || r == '_' || r == '.')
		}) >= 0 {
			return nil, fmt.Errorf("Backpack requires uppercase USDC perpetual symbols, got %q", market)
		}
		streams := []string{"depth." + market, "trade." + market, "markPrice." + market}
		feeds = append(feeds, subscription{
			source: "backpack", market: market, url: backpackStream, snapshotURL: backpackDepthURL,
			requests: []any{map[string]any{"method": "SUBSCRIBE", "params": streams}},
		})
	}
	return feeds, nil
}

// backpackSnapshot archives the public REST depth for the connection that just
// subscribed. Deltas that arrive meanwhile wait in the socket buffer.
func backpackSnapshot(ctx context.Context, source subscription, emit func(record) error) error {
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	endpoint := source.snapshotURL + "?" + url.Values{"symbol": {source.market}}.Encode()
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, endpoint, nil)
	if err != nil {
		return err
	}
	request.Header.Set("User-Agent", "AlephTX-public-capture/1.0")
	client := http.Client{CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	defer client.CloseIdleConnections()
	sent := consistentNow()
	response, err := client.Do(request)
	if err != nil {
		return fmt.Errorf("Backpack depth snapshot: %w", err)
	}
	body, readErr := io.ReadAll(io.LimitReader(response.Body, (4<<20)+1))
	received := consistentNow()
	if err := errors.Join(readErr, response.Body.Close()); err != nil {
		return err
	}
	if response.StatusCode != http.StatusOK || len(body) > 4<<20 {
		return fmt.Errorf("Backpack depth snapshot HTTP %d", response.StatusCode)
	}
	var book struct {
		LastUpdateID json.Number `json:"lastUpdateId"`
		Timestamp    uint64      `json:"timestamp"`
		Bids, Asks   [][2]string
	}
	if err := json.Unmarshal(body, &book); err != nil || book.Timestamp == 0 ||
		len(book.Bids) == 0 || len(book.Asks) == 0 {
		return errors.New("invalid Backpack depth snapshot")
	}
	if id, err := book.LastUpdateID.Int64(); err != nil || id <= 0 {
		return errors.New("invalid Backpack depth snapshot update ID")
	}
	payload, err := json.Marshal(snapshotPayload{Market: source.market, Data: body})
	if err != nil {
		return err
	}
	return emit(record{Source: "backpack_snapshot", Market: source.market, Event: "message",
		RequestSentAt: sent.UnixMilli(), ReceivedAt: received.UnixMilli(), received: received, Payload: payload})
}

// backpackPublication validates identity and depth continuity, and returns the
// archived payload. Trade order IDs are dropped; exact decimals are kept.
func backpackPublication(raw []byte, source subscription, previous uint64) ([]byte, uint64, error) {
	var msg struct {
		Stream string                     `json:"stream"`
		Data   map[string]json.RawMessage `json:"data"`
		Error  json.RawMessage            `json:"error"`
	}
	if err := json.Unmarshal(raw, &msg); err != nil {
		return nil, previous, errors.New("invalid Backpack publication")
	}
	if len(msg.Error) > 0 && string(msg.Error) != "null" {
		return nil, previous, errors.New("Backpack subscription rejected")
	}
	kind, market, ok := strings.Cut(msg.Stream, ".")
	var event, symbol string
	if !ok || market != source.market || json.Unmarshal(msg.Data["e"], &event) != nil || event != kind ||
		json.Unmarshal(msg.Data["s"], &symbol) != nil || symbol != source.market {
		return nil, previous, errors.New("invalid Backpack stream identity")
	}
	var eventTime, engineTime uint64
	if json.Unmarshal(msg.Data["E"], &eventTime) != nil || eventTime == 0 ||
		json.Unmarshal(msg.Data["T"], &engineTime) != nil || engineTime == 0 {
		return nil, previous, errors.New("missing Backpack event times")
	}
	switch kind {
	case "depth":
		var first, last uint64
		if json.Unmarshal(msg.Data["U"], &first) != nil || json.Unmarshal(msg.Data["u"], &last) != nil ||
			first == 0 || last < first {
			return nil, previous, errors.New("invalid Backpack depth update IDs")
		}
		if previous != 0 && first != previous+1 {
			return nil, previous, fmt.Errorf("Backpack depth sequence gap on %s", source.market)
		}
		return raw, last, nil
	case "markPrice":
		return raw, previous, nil
	case "trade":
		public := make(map[string]json.RawMessage)
		for _, key := range []string{"e", "E", "T", "s", "p", "q", "m", "t"} {
			value, exists := msg.Data[key]
			if !exists {
				return nil, previous, fmt.Errorf("Backpack trade missing %s", key)
			}
			public[key] = value
		}
		clean, err := json.Marshal(struct {
			Stream string                     `json:"stream"`
			Data   map[string]json.RawMessage `json:"data"`
		}{msg.Stream, public})
		return clean, previous, err
	default:
		return nil, previous, errors.New("unexpected Backpack stream")
	}
}
