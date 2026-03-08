package exchanges

import (
	"context"
	"log"
	"time"
)

// Exchange defines the interface for all feed handlers.
type Exchange interface {
	Run(ctx context.Context) error
}

// ConnectFunc represents the actual websocket or REST connection loop.
type ConnectFunc func(ctx context.Context) error

// RunConnectionLoop is a utility that handles the infinite reconnect/backoff loop
// for feeder exchanges, so individual exchanges don't have to duplicate this logic.
// Uses exponential backoff: 1s -> 2s -> 4s -> 8s -> 16s (max).
func RunConnectionLoop(ctx context.Context, name string, connect ConnectFunc) error {
	backoff := 1 * time.Second
	const maxBackoff = 16 * time.Second

	for {
		if err := connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("%s: disconnected (%v), reconnecting in %v...", name, err, backoff)

			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(backoff):
				// Exponential backoff with cap
				backoff *= 2
				if backoff > maxBackoff {
					backoff = maxBackoff
				}
			}
		} else {
			// Reset backoff on successful connection
			backoff = 1 * time.Second
		}
	}
}
