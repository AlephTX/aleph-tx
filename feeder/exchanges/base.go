package exchanges

import (
	"context"
	"log"
	"math/rand"
	"time"

	"nhooyr.io/websocket"
)

// Exchange defines the interface for all feed handlers.
type Exchange interface {
	Run(ctx context.Context) error
}

// ConnectFunc represents the actual websocket or REST connection loop.
type ConnectFunc func(ctx context.Context) error

// RunConnectionLoop is a utility that handles the infinite reconnect/backoff loop
// for feeder exchanges, so individual exchanges don't have to duplicate this logic.
// Uses exponential backoff with jitter: 1s -> 2s -> 4s -> 8s -> 16s (max).
// Includes circuit breaker: after 10 consecutive failures, pause 60s before resetting.
func RunConnectionLoop(ctx context.Context, name string, connect ConnectFunc) error {
	backoff := 1 * time.Second
	const maxBackoff = 16 * time.Second
	consecutiveFailures := 0
	const maxConsecutiveFailures = 10

	for {
		// Circuit breaker: pause after too many consecutive failures
		if consecutiveFailures >= maxConsecutiveFailures {
			log.Printf("%s: circuit breaker open (>=%d failures), pausing 60s...", name, maxConsecutiveFailures)
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(60 * time.Second):
				log.Printf("%s: circuit breaker reset, resuming...", name)
				consecutiveFailures = 0
				backoff = 1 * time.Second
			}
		}

		if err := connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}

			consecutiveFailures++
			log.Printf("%s: disconnected (%v), reconnecting in %v... (failures: %d)", name, err, backoff, consecutiveFailures)

			// Add ±25% jitter to backoff
			jitter := time.Duration(rand.Float64() * 0.5 * float64(backoff))
			sleepDuration := backoff + jitter - time.Duration(0.25*float64(backoff))

			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(sleepDuration):
				// Exponential backoff with cap
				backoff *= 2
				if backoff > maxBackoff {
					backoff = maxBackoff
				}
			}
		} else {
			// Reset backoff and failure counter on successful connection
			consecutiveFailures = 0
			backoff = 1 * time.Second
		}
	}
}

func startWebSocketKeepalive(
	parent context.Context,
	name string,
	c *websocket.Conn,
	interval time.Duration,
) func() {
	done := make(chan struct{})

	go func() {
		ticker := time.NewTicker(interval)
		defer ticker.Stop()

		for {
			select {
			case <-parent.Done():
				return
			case <-done:
				return
			case <-ticker.C:
				pingCtx, cancel := context.WithTimeout(parent, 5*time.Second)
				err := c.Ping(pingCtx)
				cancel()
				if err != nil && parent.Err() == nil {
					log.Printf("%s: keepalive ping failed: %v", name, err)
				}
			}
		}
	}()

	return func() {
		close(done)
	}
}
