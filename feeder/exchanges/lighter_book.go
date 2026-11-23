package exchanges

import (
	"errors"
	"fmt"
	"math"
	"strconv"

	"github.com/AlephTX/aleph-tx/feeder/shm"
	"github.com/tidwall/gjson"
)

const maxLighterBookLevels = 10_000

var (
	errInvalidBookPrice    = errors.New("invalid book price")
	errInvalidBookSize     = errors.New("invalid book size")
	errBookSideNotArray    = errors.New("book side must be an array")
	errBookDeltaBeforeSnap = errors.New("book delta before snapshot")
	errInvalidBookNonce    = errors.New("invalid book nonce")
	errInvalidBeginNonce   = errors.New("invalid book begin_nonce")
	errInvalidLastUpdated  = errors.New("invalid book last_updated_at")
	errCrossedBook         = errors.New("crossed reconstructed book")
)

// lighterBook reconstructs the full price-level book. Lighter update/order_book
// messages are deltas: omitted levels remain, and size=0 deletes a level.
// See the official SDK's WsClient.update_order_book_state/update_orders:
// https://github.com/elliottech/lighter-python/blob/main/lighter/ws_client.py
// Each side stays sorted so extracting L1-L5 costs O(5), with no hot-path sort.
type lighterBook struct {
	bids, asks       []shm.PriceLevel
	initialized      bool
	offset           int64
	hasOffset        bool
	nonce            int64
	hasNonce         bool
	lastUpdatedAt    int64
	hasLastUpdatedAt bool
}

type lighterBookMeta struct {
	hasNonce         bool
	nonce            int64
	nonceRaw         []byte
	hasBeginNonce    bool
	beginNonce       int64
	beginNonceRaw    []byte
	hasOffset        bool
	offset           int64
	hasLastUpdatedAt bool
	lastUpdatedAt    int64
}

type lighterParsedBook struct {
	meta    lighterBookMeta
	hasBids bool
	hasAsks bool
	bids    []shm.PriceLevel
	asks    []shm.PriceLevel
}

func validBookLevelPriceSize(price, size float64) error {
	if price <= 0 || math.IsNaN(price) || math.IsInf(price, 0) {
		return errInvalidBookPrice
	}
	if size < 0 || math.IsNaN(size) || math.IsInf(size, 0) {
		return errInvalidBookSize
	}
	return nil
}

func validBookLevelBytes(priceBytes, sizeBytes []byte) (shm.PriceLevel, error) {
	price, err := parseFloatBytes(priceBytes)
	if err != nil {
		return shm.PriceLevel{}, errInvalidBookPrice
	}
	size, err := parseFloatBytes(sizeBytes)
	if err != nil {
		return shm.PriceLevel{}, errInvalidBookSize
	}
	if err := validBookLevelPriceSize(price, size); err != nil {
		return shm.PriceLevel{}, err
	}
	return shm.PriceLevel{Price: price, Size: size}, nil
}

func validBookLevel(level gjson.Result) (shm.PriceLevel, error) {
	price, err := strconv.ParseFloat(level.Get("price").String(), 64)
	if err != nil {
		return shm.PriceLevel{}, errInvalidBookPrice
	}
	size, err := strconv.ParseFloat(level.Get("size").String(), 64)
	if err != nil {
		return shm.PriceLevel{}, errInvalidBookSize
	}
	if err := validBookLevelPriceSize(price, size); err != nil {
		return shm.PriceLevel{}, err
	}
	return shm.PriceLevel{Price: price, Size: size}, nil
}

func (b *lighterBook) validateHeader(meta lighterBookMeta, snapshot bool) (skip bool, err error) {
	if !snapshot && !b.initialized {
		return false, errBookDeltaBeforeSnap
	}
	if meta.hasNonce && meta.nonce <= 0 {
		return false, errInvalidBookNonce
	}
	if meta.hasBeginNonce {
		if snapshot {
			if meta.beginNonce < 0 {
				return false, errInvalidBeginNonce
			}
		} else if meta.beginNonce <= 0 {
			return false, errInvalidBeginNonce
		}
	}
	if meta.hasLastUpdatedAt {
		if snapshot {
			if meta.lastUpdatedAt < 0 {
				return false, errInvalidLastUpdated
			}
		} else if meta.lastUpdatedAt <= 0 {
			return false, errInvalidLastUpdated
		}
	}
	if !snapshot {
		if meta.hasNonce && b.hasNonce && meta.nonce <= b.nonce {
			return true, nil // Duplicate or reordered frame; never roll back the book.
		}
		if meta.hasOffset && b.hasOffset && meta.offset <= b.offset {
			return true, nil // Duplicate or reordered frame; never roll back the book.
		}
		if b.hasNonce || meta.hasBeginNonce {
			if !b.hasNonce || !meta.hasNonce || !meta.hasBeginNonce || meta.beginNonce != b.nonce {
				prev := b.nonce
				*b = lighterBook{}
				return false, fmt.Errorf("book nonce gap: expected begin_nonce=%d, got begin_nonce=%s nonce=%s", prev, meta.beginNonceRaw, meta.nonceRaw)
			}
		}
		if meta.hasLastUpdatedAt && b.hasLastUpdatedAt && meta.lastUpdatedAt < b.lastUpdatedAt {
			prev := b.lastUpdatedAt
			*b = lighterBook{}
			return false, fmt.Errorf("book timestamp regression: %d < %d", meta.lastUpdatedAt, prev)
		}
	}
	return false, nil
}

func (b *lighterBook) finishApply(meta lighterBookMeta, snapshot bool) (bool, error) {
	if len(b.bids) > maxLighterBookLevels || len(b.asks) > maxLighterBookLevels {
		*b = lighterBook{}
		return false, fmt.Errorf("reconstructed book exceeds %d levels", maxLighterBookLevels)
	}
	if len(b.bids) > 0 && len(b.asks) > 0 && b.bids[0].Price >= b.asks[0].Price {
		*b = lighterBook{}
		return false, errCrossedBook
	}
	b.initialized = true
	b.hasOffset = meta.hasOffset
	b.offset = meta.offset
	b.hasNonce = meta.hasNonce
	b.nonce = meta.nonce
	if snapshot {
		b.hasLastUpdatedAt = meta.hasLastUpdatedAt && meta.lastUpdatedAt > 0
		b.lastUpdatedAt = meta.lastUpdatedAt
	} else if meta.hasLastUpdatedAt {
		b.hasLastUpdatedAt = true
		b.lastUpdatedAt = meta.lastUpdatedAt
	}
	return true, nil
}

func (b *lighterBook) applyParsed(parsed *lighterParsedBook, snapshot bool) (bool, error) {
	if parsed == nil {
		return false, errBookSideNotArray
	}
	skip, err := b.validateHeader(parsed.meta, snapshot)
	if err != nil || skip {
		return false, err
	}
	if !parsed.hasBids || !parsed.hasAsks {
		return false, errBookSideNotArray
	}
	// Validate every level before mutating either side.
	for _, lvl := range parsed.bids {
		if err := validBookLevelPriceSize(lvl.Price, lvl.Size); err != nil {
			return false, err
		}
	}
	for _, lvl := range parsed.asks {
		if err := validBookLevelPriceSize(lvl.Price, lvl.Size); err != nil {
			return false, err
		}
	}
	if snapshot {
		b.bids, b.asks = b.bids[:0], b.asks[:0]
	}
	for _, lvl := range parsed.bids {
		b.bids = applyBookLevel(b.bids, lvl, true)
	}
	for _, lvl := range parsed.asks {
		b.asks = applyBookLevel(b.asks, lvl, false)
	}
	return b.finishApply(parsed.meta, snapshot)
}

func (b *lighterBook) apply(book gjson.Result, snapshot bool) (bool, error) {
	nonce := book.Get("nonce")
	beginNonce := book.Get("begin_nonce")
	lastUpdatedAt := book.Get("last_updated_at")
	offset := book.Get("offset")
	meta := lighterBookMeta{
		hasNonce:         nonce.Exists(),
		nonce:            nonce.Int(),
		nonceRaw:         []byte(nonce.Raw),
		hasBeginNonce:    beginNonce.Exists(),
		beginNonce:       beginNonce.Int(),
		beginNonceRaw:    []byte(beginNonce.Raw),
		hasOffset:        offset.Exists(),
		offset:           offset.Int(),
		hasLastUpdatedAt: lastUpdatedAt.Exists(),
		lastUpdatedAt:    lastUpdatedAt.Int(),
	}
	skip, err := b.validateHeader(meta, snapshot)
	if err != nil || skip {
		return false, err
	}
	bids, asks := book.Get("bids"), book.Get("asks")
	// Validate the whole message before changing either side.
	for _, side := range []gjson.Result{bids, asks} {
		if !side.IsArray() {
			return false, errBookSideNotArray
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
	return b.finishApply(meta, snapshot)
}

func searchBookLevel(levels []shm.PriceLevel, price float64, descending bool) int {
	lo, hi := 0, len(levels)
	for lo < hi {
		mid := int(uint(lo+hi) >> 1)
		if descending {
			if levels[mid].Price <= price {
				hi = mid
			} else {
				lo = mid + 1
			}
		} else {
			if levels[mid].Price >= price {
				hi = mid
			} else {
				lo = mid + 1
			}
		}
	}
	return lo
}

func applyBookLevel(levels []shm.PriceLevel, level shm.PriceLevel, descending bool) []shm.PriceLevel {
	i := searchBookLevel(levels, level.Price, descending)
	if i < len(levels) && levels[i].Price == level.Price {
		if level.Size == 0 {
			copy(levels[i:], levels[i+1:])
			return levels[:len(levels)-1]
		}
		levels[i] = level
		return levels
	}
	if level.Size > 0 {
		levels = append(levels, shm.PriceLevel{})
		copy(levels[i+1:], levels[i:])
		levels[i] = level
	}
	return levels
}

func updateBookSide(levels []shm.PriceLevel, updates gjson.Result, descending bool) []shm.PriceLevel {
	updates.ForEach(func(_, raw gjson.Result) bool {
		level, _ := validBookLevel(raw) // Entire frame was validated before mutation.
		levels = applyBookLevel(levels, level, descending)
		return true
	})
	return levels
}

func (b *lighterBook) depth() (bids, asks [shm.DepthLevels]shm.PriceLevel) {
	copy(bids[:], b.bids)
	copy(asks[:], b.asks)
	return
}
