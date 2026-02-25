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
func RunConnectionLoop(ctx context.Context, name string, connect ConnectFunc) error {
	for {
		if err := connect(ctx); err != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			log.Printf("%s: disconnected (%v), reconnecting in 3s...", name, err)
			
			select {
			case <-ctx.Done():
				return ctx.Err()
			case <-time.After(3 * time.Second):
			}
		}
	}
}
