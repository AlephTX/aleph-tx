package wsutil

import (
	"context"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"nhooyr.io/websocket"
)

func TestKeepaliveSendsApplicationHeartbeatAndClosesStalledPeer(t *testing.T) {
	ctx, cancel := context.WithTimeout(t.Context(), 10*time.Second)
	defer cancel()
	seen := make(chan string, 1)
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		conn, err := websocket.Accept(w, r, nil)
		if err != nil {
			t.Errorf("accept WebSocket: %v", err)
			return
		}
		defer conn.CloseNow()
		_, payload, err := conn.Read(ctx)
		if err != nil {
			t.Errorf("read application heartbeat: %v", err)
			return
		}
		seen <- string(payload)
		// Leave the socket open with no reader so the transport ping gets no pong.
		<-ctx.Done()
	}))
	defer server.Close()
	conn, _, err := websocket.Dial(ctx, "ws"+strings.TrimPrefix(server.URL, "http"), nil)
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	const heartbeat = `{"method":"ping"}`
	stop := StartKeepalive(ctx, "test", conn, time.Millisecond, []byte(heartbeat))
	defer stop()
	if _, _, err := conn.Read(ctx); err == nil || ctx.Err() != nil {
		t.Fatalf("keepalive did not wake the reader before its deadline: %v", err)
	}
	select {
	case got := <-seen:
		if got != heartbeat {
			t.Fatalf("heartbeat = %q, want %q", got, heartbeat)
		}
	default:
		t.Fatal("peer never received the application heartbeat")
	}
}
