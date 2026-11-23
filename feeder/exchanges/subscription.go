package exchanges

import (
	"bytes"
	"context"
	"fmt"
	"strings"
	"time"
	"unsafe"

	"github.com/tidwall/gjson"
)

const subscriptionTimeout = 15 * time.Second

var subscriptionErrorTypeBytes = []byte("error")

func unsafeString(b []byte) string {
	if len(b) == 0 {
		return ""
	}
	return unsafe.String(unsafe.SliceData(b), len(b))
}

func channelBytesEqualNormalized(raw []byte, normalized string) bool {
	if len(raw) != len(normalized) {
		return false
	}
	for i := 0; i < len(raw); i++ {
		c := raw[i]
		if c == ':' {
			c = '/'
		}
		if c != normalized[i] {
			return false
		}
	}
	return true
}

// subscriptionGuard bounds the complete handshake, including arbitrary pings
// or unrelated messages. Transport keepalives never acknowledge a subscription
// and never refresh account/position timestamps.
type subscriptionGuard struct {
	kind           string
	subscribedKind string
	channels       map[string]bool
	pending        int
	ctx            context.Context
	cancel         context.CancelFunc
}

func newSubscriptionGuard(parent context.Context, kind string, channels []string, timeout time.Duration) *subscriptionGuard {
	ctx, cancel := context.WithTimeout(parent, timeout)
	g := &subscriptionGuard{
		kind:           kind,
		subscribedKind: "subscribed/" + kind,
		channels:       make(map[string]bool, len(channels)),
		ctx:            ctx,
		cancel:         cancel,
	}
	for _, channel := range channels {
		g.channels[normalizeChannel(channel)] = false
	}
	g.pending = len(g.channels)
	return g
}

func normalizeChannel(channel string) string { return strings.ReplaceAll(channel, ":", "/") }

func (g *subscriptionGuard) readContext(parent context.Context) context.Context {
	if g.pending != 0 {
		return g.ctx
	}
	return parent
}

func (g *subscriptionGuard) matchesSubscribedKind(kind []byte) bool {
	if g.subscribedKind != "" {
		return len(kind) == len(g.subscribedKind) && unsafeString(kind) == g.subscribedKind
	}
	const prefix = "subscribed/"
	return len(kind) == len(prefix)+len(g.kind) &&
		unsafeString(kind[:len(prefix)]) == prefix &&
		unsafeString(kind[len(prefix):]) == g.kind
}

func (g *subscriptionGuard) observeFields(kind, channel []byte, hasNonNullError bool, errCode []byte) error {
	if g.pending == 0 && !hasNonNullError && !bytes.Equal(kind, subscriptionErrorTypeBytes) {
		return nil
	}
	if hasNonNullError || bytes.Equal(kind, subscriptionErrorTypeBytes) {
		// Do not log the raw frame: some servers echo authenticated requests.
		return fmt.Errorf("%s subscription/server error (code=%s)", g.kind, string(errCode))
	}
	if g.pending > 0 && g.matchesSubscribedKind(kind) {
		for ch, confirmed := range g.channels {
			if !confirmed && channelBytesEqualNormalized(channel, ch) {
				g.channels[ch] = true
				g.pending--
				if g.pending == 0 {
					g.cancel()
				}
				break
			}
		}
	}
	return nil
}

func (g *subscriptionGuard) observe(message gjson.Result) error {
	kind := message.Get("type").String()
	serverError := message.Get("error")
	if kind == "error" || (serverError.Exists() && serverError.Type != gjson.Null) {
		// Do not log the raw frame: some servers echo authenticated requests.
		return fmt.Errorf("%s subscription/server error (code=%s)", g.kind, message.Get("code").String())
	}
	channel := normalizeChannel(message.Get("channel").String())
	if confirmed, known := g.channels[channel]; known && !confirmed && kind == "subscribed/"+g.kind {
		g.channels[channel] = true
		g.pending--
		if g.pending == 0 {
			g.cancel()
		}
	}
	return nil
}

func (g *subscriptionGuard) acceptsData(message gjson.Result) bool {
	kind := message.Get("type").String()
	_, known := g.channels[normalizeChannel(message.Get("channel").String())]
	return known && (kind == "subscribed/"+g.kind || kind == "update/"+g.kind)
}
