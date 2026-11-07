// Package wsutil provides the shared WebSocket connection lifecycle.
package wsutil

import (
	"context"
	"log"
	"time"

	"nhooyr.io/websocket"
)

// StartKeepalive probes the peer without refreshing market data. An optional
// application heartbeat is sent before each transport ping. Stop cancels and
// joins the worker, including any outstanding write or pong wait.
func StartKeepalive(parent context.Context, name string, conn *websocket.Conn, interval time.Duration, heartbeat []byte) func() {
	ctx, cancel := context.WithCancel(parent)
	done := make(chan struct{})
	go func() {
		defer close(done)
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				pingCtx, stop := context.WithTimeout(ctx, 5*time.Second)
				var err error
				if len(heartbeat) > 0 {
					err = conn.Write(pingCtx, websocket.MessageText, heartbeat)
				}
				if err == nil {
					err = conn.Ping(pingCtx)
				}
				stop()
				if err != nil {
					if ctx.Err() == nil {
						log.Printf("%s: keepalive failed: %v", name, err)
						// Wake the read loop so its reconnect policy can recover.
						_ = conn.CloseNow()
					}
					return
				}
			}
		}
	}()
	return func() {
		cancel()
		<-done
	}
}
