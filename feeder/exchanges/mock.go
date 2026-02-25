package exchanges

import (
	"context"
	"math"
	"math/rand"
	"time"

	"github.com/AlephTX/aleph-tx/feeder/shm"
)

// MockFeeder generates realistic BBO data for exchanges we can't reach.
// Prices track a random walk around a base price with realistic spreads.
type MockFeeder struct {
	matrix    *shm.Matrix
	exchangeID uint8
	name      string
}

func NewMockFeeder(matrix *shm.Matrix, exchangeID uint8, name string) *MockFeeder {
	return &MockFeeder{matrix: matrix, exchangeID: exchangeID, name: name}
}

func (m *MockFeeder) Run(ctx context.Context) {
	// Base prices — will drift with random walk
	btcMid := 63100.0
	ethMid := 1825.0

	ticker := time.NewTicker(100 * time.Millisecond) // 10 updates/sec
	defer ticker.Stop()

	rng := rand.New(rand.NewSource(time.Now().UnixNano() + int64(m.exchangeID)*1000))

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			tsNs := uint64(time.Now().UnixNano())

			// Random walk: ±0.01% per tick
			btcMid += btcMid * (rng.Float64() - 0.5) * 0.0002
			ethMid += ethMid * (rng.Float64() - 0.5) * 0.0002

			// Realistic spread: BTC ~$1, ETH ~$0.10
			btcSpread := 0.5 + rng.Float64()*1.0
			ethSpread := 0.05 + rng.Float64()*0.10

			btcBid := math.Round((btcMid-btcSpread/2)*100) / 100
			btcAsk := math.Round((btcMid+btcSpread/2)*100) / 100
			ethBid := math.Round((ethMid-ethSpread/2)*100) / 100
			ethAsk := math.Round((ethMid+ethSpread/2)*100) / 100

			// Random sizes
			btcBidSz := 0.1 + rng.Float64()*2.0
			btcAskSz := 0.1 + rng.Float64()*2.0
			ethBidSz := 1.0 + rng.Float64()*20.0
			ethAskSz := 1.0 + rng.Float64()*20.0

			// Write to shared matrix (triggers version increment)
			m.matrix.WriteBBO(m.exchangeID, SymbolBTCPERP, tsNs,
				btcBid, btcBidSz, btcAsk, btcAskSz)
			m.matrix.WriteBBO(m.exchangeID, SymbolETHPERP, tsNs,
				ethBid, ethBidSz, ethAsk, ethAskSz)
		}
	}
}
