"""Convert shared public archives into receipt-timed research observations."""
from decimal import Decimal
import json

from archive_public_capture import records


def trade_events(archive, venue, market, leg, *, precise_times=False):
    """Yield maker replay events from acknowledged public trade subscriptions."""
    sources = {'lighter': 'lighter_trades', 'lighter_robinhood': 'lighter_robinhood_trades',
               'extended': 'extended_trades', 'hyperliquid': 'hyperliquid_trades',
               'entropy': 'hyperliquid_trades', 'trade_xyz': 'hyperliquid_trades'}
    source = sources[venue]
    online_since = None
    seen = {}
    for raw in records(archive, source=source):
        row = json.loads(raw)
        if row['source'] != source or row.get('market') not in (None, market):
            continue
        at, event = row['available_at_ms'], row['event']
        if precise_times:
            from capture_timing import precise_available_ms
            at = precise_available_ms(row)
        if event in ('disconnect', 'clock_refresh'):
            online_since = None
            yield {'kind': 'trade_feed', 'at_ms': at, 'leg': leg, 'online': False}
            continue
        if event not in ('message', 'subscribed'):
            continue
        data = row['payload']
        if source == 'hyperliquid_trades':
            subscribed = event == 'subscribed' and data == {'type': 'trades', 'coin': market}
            trades = [(t['tid'], t['time'], t['px'], t['sz'], {'A': 'sell', 'B': 'buy'}[t['side']])
                      for t in data.get('data', []) if t['coin'] == market]
        elif source == 'extended_trades':
            subscribed = data['seq'] == 1
            trades = [(t['i'], t['T'], t['p'], t['q'], t['S'].lower()) for t in data['data']
                      if t['m'] == market and t['tT'] == 'TRADE']
        else:
            if data['channel'].replace('/', ':') != f'trade:{market}':
                continue
            subscribed = data['type'] == 'subscribed/trade'
            trades = [(t['trade_id'], t['timestamp'], t['price'], t['size'],
                       'buy' if t['is_maker_ask'] else 'sell') for t in data.get('trades') or []
                      if str(t['market_id']) == market and type(t['is_maker_ask']) is bool]
        if subscribed:
            online_since = row['received_at_ms']
            yield {'kind': 'trade_feed', 'at_ms': at, 'leg': leg, 'online': True}
        for identifier, stamp, price, size, side in trades:
            if type(stamp) is not int or stamp <= 0 or side not in ('buy', 'sell'):
                raise ValueError('invalid public trade fields')
            if online_since is None:
                raise ValueError('public trade received without a valid subscription')
            key = (stamp, identifier) if source == 'hyperliquid_trades' else identifier
            value = (stamp, str(price), str(size), side)
            if key in seen:
                if seen[key] != value:
                    raise ValueError('conflicting duplicate public trade')
                continue
            seen[key] = value
            # Lighter and Extended mark subscription snapshots on the wire.
            # Keep the conservative cutoff for unmarked Hyperliquid trade batches.
            if not subscribed and (source != 'hyperliquid_trades' or stamp >= online_since):
                yield {'kind': 'trade', 'at_ms': at, 'leg': leg, 'source_ms': stamp,
                       'aggressor': side, 'price': str(price), 'contracts': str(size)}


def funding_samples(archive):
    """Expose published market rates with their original availability, in bps."""
    hour = 3_600_000
    for raw in records(archive, source='hyperliquid_funding'):
        row = json.loads(raw)
        if row['source'] != 'hyperliquid_funding' or row['event'] != 'message':
            continue
        market = row['market']
        rates = []
        for point in row['payload']['data']:
            stamp = point['time']
            slot = stamp // hour * hour
            rate = Decimal(point['fundingRate']) * 10_000
            if (point['coin'] != market or not 0 <= stamp - slot <= 10_000
                    or stamp > row['received_at_ms'] or not rate.is_finite()):
                raise ValueError('invalid Hyperliquid hourly funding evidence')
            rates.append({'slot_ms': slot, 'source_time_ms': stamp, 'rate_bps': str(rate)})
        yield {'venue': 'hyperliquid', 'market': market, 'available_at_ms': row['available_at_ms'], 'rates': rates}
