package main

import "time"

// Domain identity is part of the recording source, never inferred from market ID.
func isLighterBookSource(source string) bool {
	return source == "lighter" || source == "lighter_robinhood"
}

func staggerLighterClockRefresh(sources []subscription, start time.Time) {
	for i := range sources {
		s := &sources[i]
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
