package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/coder/websocket"
	"github.com/coder/websocket/wsjson"
)

const lighterClockProbeTimeout = 10 * time.Second

// Domain identity is part of the recording source, never inferred from market ID.
func isLighterBookSource(source string) bool {
	return source == "lighter" || source == "lighter_robinhood"
}

func validateLighterClockOptions(lighter, rh string, clockRefresh time.Duration, staggerRefresh, clockProbe bool) error {
	if clockRefresh != 0 && ((lighter == "" && rh == "") || clockRefresh < 30*time.Second || clockRefresh > 9*time.Minute) {
		return errors.New("clock refresh needs Lighter markets and an interval from 30s to 9m")
	}
	if staggerRefresh && (clockRefresh == 0 || lighter == "" || rh == "") {
		return errors.New("-stagger-lighter-clock-refresh requires Core and Robinhood markets and -lighter-clock-refresh")
	}
	if clockProbe && (clockRefresh <= 0 || (lighter == "" && rh == "")) {
		return errors.New("-lighter-clock-probe requires Lighter markets and -lighter-clock-refresh")
	}
	return nil
}

func enableLighterClockProbes(sources []subscription, clockRefresh time.Duration) ([]subscription, error) {
	if clockRefresh <= 0 {
		return nil, errors.New("-lighter-clock-probe requires -lighter-clock-refresh > 0")
	}
	var probes []subscription
	for i := range sources {
		s := &sources[i]
		if !isLighterBookSource(s.source) || s.clockProbe {
			continue
		}
		if len(s.coins) == 0 {
			return nil, fmt.Errorf("missing Lighter markets for %s clock probe", s.source)
		}
		id, err := strconv.ParseUint(s.coins[0], 10, 16)
		if err != nil {
			return nil, fmt.Errorf("lighter market ID %q: %w", s.coins[0], err)
		}
		// Keep the primary orderbook WebSocket connected continuously without planned disconnects.
		s.clockRefresh = 0
		s.firstClockRefresh = time.Time{}
		probes = append(probes, subscription{
			source:       s.source,
			url:          s.url,
			requests:     []any{lighterSubscription{Type: "subscribe", Channel: fmt.Sprintf("market_stats/%d", id)}},
			clockRefresh: clockRefresh,
			clockProbe:   true,
		})
	}
	if len(probes) == 0 {
		return nil, errors.New("-lighter-clock-probe requires Lighter markets")
	}
	return append(sources, probes...), nil
}

func validClockBracket(sent, received time.Time) bool {
	wallElapsed := received.UnixMilli() - sent.UnixMilli()
	monotonicElapsed := received.Sub(sent).Milliseconds()
	return wallElapsed >= 0 && wallElapsed >= monotonicElapsed-1 && wallElapsed <= monotonicElapsed+1
}

func runLighterClockProbeLoop(ctx context.Context, source subscription, emit func(record) error) {
	var epoch uint64
	for ctx.Err() == nil {
		deadline := source.nextClockRefresh(time.Now())
		if deadline.IsZero() {
			return
		}
		timer := time.NewTimer(max(0, time.Until(deadline)))
		select {
		case <-ctx.Done():
			timer.Stop()
			return
		case <-timer.C:
		}
		epoch++
		if err := probeLighterClockOnce(ctx, source, epoch, emit); err != nil {
			if ctx.Err() != nil {
				return
			}
			log.Printf("%s clock_probe: %v", source.source, err)
		}
	}
}

func probeLighterClockOnce(ctx context.Context, source subscription, epoch uint64, emit func(record) error) error {
	if len(source.requests) == 0 {
		return errors.New("missing clock probe subscription request")
	}
	sub, ok := source.requests[0].(lighterSubscription)
	if !ok {
		return errors.New("invalid clock probe subscription request")
	}
	probeCtx, cancelProbe := context.WithTimeout(ctx, lighterClockProbeTimeout)
	defer cancelProbe()
	dialCtx, cancelDial := context.WithTimeout(probeCtx, 8*time.Second)
	conn, _, err := websocket.Dial(dialCtx, source.url, &websocket.DialOptions{
		HTTPHeader: http.Header{"User-Agent": {"AlephTX-public-capture/1.0"}},
	})
	cancelDial()
	if err != nil {
		return fmt.Errorf("dial: %w", err)
	}
	defer conn.CloseNow()
	conn.SetReadLimit(1 << 20)

	sent := consistentNow()
	if err := wsjson.Write(probeCtx, conn, sub); err != nil {
		return fmt.Errorf("subscribe: %w", err)
	}

	for {
		_, raw, err := conn.Read(probeCtx)
		received := consistentNow()
		if err != nil {
			return fmt.Errorf("read: %w", err)
		}
		var header struct {
			Type    string `json:"type"`
			Channel string `json:"channel"`
		}
		if err := json.Unmarshal(raw, &header); err != nil {
			return fmt.Errorf("decode: %w", err)
		}
		if header.Type == "ping" {
			if err := wsjson.Write(probeCtx, conn, map[string]string{"type": "pong"}); err != nil {
				return fmt.Errorf("pong: %w", err)
			}
			continue
		}
		if header.Type == "error" || header.Channel == "error" {
			return errors.New("clock probe subscription rejected")
		}
		if header.Type != "subscribed/market_stats" ||
			strings.Replace(header.Channel, ":", "/", 1) != sub.Channel {
			continue
		}
		row := record{
			epoch:      epoch,
			Source:     source.source,
			Event:      "clock_probe",
			ReceivedAt: received.UnixMilli(),
			received:   received,
			Payload:    raw,
		}
		if validClockBracket(sent, received) {
			row.RequestSentAt = sent.UnixMilli()
		}
		if err := emit(row); err != nil {
			return err
		}
		_ = conn.Close(websocket.StatusNormalClosure, "")
		return nil
	}
}

func staggerLighterClockRefresh(sources []subscription, start time.Time) {
	for i := range sources {
		s := &sources[i]
		if s.clockRefresh <= 0 {
			continue
		}
		switch s.source {
		case "lighter":
			s.firstClockRefresh = start.Add(s.clockRefresh)
		case "lighter_robinhood":
			s.firstClockRefresh = start.Add(s.clockRefresh / 2)
		}
	}
}

// Retain the original phase after a disconnect, even if retries missed a refresh.
func (s subscription) nextClockRefresh(now time.Time) time.Time {
	if s.clockRefresh <= 0 {
		return time.Time{}
	}
	if s.firstClockRefresh.IsZero() {
		return now.Add(s.clockRefresh)
	}
	if now.Before(s.firstClockRefresh) {
		return s.firstClockRefresh
	}
	return now.Add(s.clockRefresh - now.Sub(s.firstClockRefresh)%s.clockRefresh)
}

func robinhoodSubscriptions(markets string, trades bool, clock time.Duration) ([]subscription, error) {
	if markets == "" {
		return nil, nil
	}
	sources, err := subscriptions(markets, "", "", "", "", "", "", 0, clock, false)
	if err != nil {
		return nil, err
	}
	for i := range sources {
		sources[i].source = "lighter_robinhood"
		sources[i].url = "wss://api.rh.lighter.xyz/stream"
	}
	if trades {
		tape := subscription{source: "lighter_robinhood_trades", url: "wss://api.rh.lighter.xyz/stream"}
		for _, request := range sources[0].requests {
			sub := request.(lighterSubscription)
			if len(sub.Channel) > len("order_book/") && sub.Channel[:len("order_book/")] == "order_book/" {
				tape.requests = append(tape.requests, lighterSubscription{Type: "subscribe", Channel: "trade/" + sub.Channel[len("order_book/"):]})
			}
		}
		sources = append(sources, tape)
	}
	return sources, nil
}
