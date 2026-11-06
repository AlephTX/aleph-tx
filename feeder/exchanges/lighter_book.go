package exchanges

import (
	"fmt"
	"math"
	"sort"
	"strconv"

	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/tidwall/gjson"
)

// lighterBook reconstructs the full price-level book. Lighter update/order_book
// messages are deltas: omitted levels remain, and size=0 deletes a level.
// See the official SDK's WsClient.update_order_book_state/update_orders:
// https://github.com/elliottech/lighter-python/blob/main/lighter/ws_client.py
// Each side stays sorted so extracting L1-L5 costs O(5), with no hot-path sort.
type lighterBook struct {
	bids, asks  []shm.PriceLevel
	initialized bool
	offset      int64
	hasOffset   bool
}

func validBookLevel(level gjson.Result) (shm.PriceLevel, error) {
	price, err := strconv.ParseFloat(level.Get("price").String(), 64)
	if err != nil || price <= 0 || math.IsNaN(price) || math.IsInf(price, 0) {
		return shm.PriceLevel{}, fmt.Errorf("invalid book price")
	}
	size, err := strconv.ParseFloat(level.Get("size").String(), 64)
	if err != nil || size < 0 || math.IsNaN(size) || math.IsInf(size, 0) {
		return shm.PriceLevel{}, fmt.Errorf("invalid book size")
	}
	return shm.PriceLevel{Price: price, Size: size}, nil
}

func (b *lighterBook) apply(book gjson.Result, snapshot bool) (bool, error) {
	if !snapshot && !b.initialized {
		return false, fmt.Errorf("book delta before snapshot")
	}
	offset := book.Get("offset")
	if !snapshot && offset.Exists() && b.hasOffset && offset.Int() <= b.offset {
		return false, nil // Duplicate or reordered frame; never roll back the book.
	}
	bids, asks := book.Get("bids"), book.Get("asks")
	// Validate the whole message before changing either side.
	for _, side := range []gjson.Result{bids, asks} {
		if !side.IsArray() {
			return false, fmt.Errorf("book side must be an array")
		}
		var parseErr error
		side.ForEach(func(_, level gjson.Result) bool {
			_, parseErr = validBookLevel(level)
			return parseErr == nil
		})
		if parseErr != nil {
			return false, parseErr
		}
	}
	if snapshot {
		b.bids, b.asks = b.bids[:0], b.asks[:0]
	}
	b.bids = updateBookSide(b.bids, bids, true)
	b.asks = updateBookSide(b.asks, asks, false)
	b.initialized = true
	b.hasOffset = offset.Exists()
	b.offset = offset.Int()
	if len(b.bids) > 0 && len(b.asks) > 0 && b.bids[0].Price >= b.asks[0].Price {
		b.initialized = false
		return false, fmt.Errorf("crossed reconstructed book")
	}
	return true, nil
}

func updateBookSide(levels []shm.PriceLevel, updates gjson.Result, descending bool) []shm.PriceLevel {
	updates.ForEach(func(_, raw gjson.Result) bool {
		level, _ := validBookLevel(raw) // Entire frame was validated before mutation.
		i := sort.Search(len(levels), func(i int) bool {
			if descending {
				return levels[i].Price <= level.Price
			}
			return levels[i].Price >= level.Price
		})
		if i < len(levels) && levels[i].Price == level.Price {
			if level.Size == 0 {
				copy(levels[i:], levels[i+1:])
				levels = levels[:len(levels)-1]
			} else {
				levels[i] = level
			}
		} else if level.Size > 0 {
			levels = append(levels, shm.PriceLevel{})
			copy(levels[i+1:], levels[i:])
			levels[i] = level
		}
		return true
	})
	return levels
}

func (b *lighterBook) depth() (bids, asks [shm.DepthLevels]shm.PriceLevel) {
	copy(bids[:], b.bids)
	copy(asks[:], b.asks)
	return
}
