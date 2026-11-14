"""Precise public capture clock validation; no exchange clock calibration."""
import re


def precise_available_ms(row):
    stamp = row['capture_timing']
    keys = ('sequence', 'connection_epoch', 'receipt_wall_ns', 'receipt_monotonic_ns',
            'available_wall_ns', 'available_monotonic_ns')
    if any(type(stamp.get(k)) is not int or stamp[k] < 0 for k in keys):
        raise ValueError('invalid precise capture clock fields')
    rw, aw = stamp['receipt_wall_ns'], stamp['available_wall_ns']
    rm, am = stamp['receipt_monotonic_ns'], stamp['available_monotonic_ns']
    if (not re.fullmatch('[0-9a-f]{32}', stamp.get('run_id', '')) or not stamp['sequence']
            or rw <= 0 or aw < rw or am < rm
            or rw // 1_000_000 != row['received_at_ms']
            or aw // 1_000_000 != row['available_at_ms']
            or abs((aw - rw) - (am - rm)) > 1_000_000
            or (row['source'] == 'collector') != (stamp['connection_epoch'] == 0)):
        raise ValueError('inconsistent precise capture clocks')
    # Rounding DOWN can make an observation available before it was received.
    return (aw + 999_999) // 1_000_000


class ClockAudit:
    def __init__(self):
        self.run_id = None
        self.sequence = 0
        self.last_wall = 0
        self.last_mono = 0
        self.origin = None
        self.epochs = {}

    def observe(self, row):
        at = precise_available_ms(row)
        t = row['capture_timing']
        if self.run_id is None:
            self.run_id = t['run_id']
            self.origin = t['available_wall_ns'] - t['available_monotonic_ns']
        key = row['source'], row.get('market')
        if (t['run_id'] != self.run_id or t['sequence'] != self.sequence + 1
                or t['available_wall_ns'] < self.last_wall
                or t['available_monotonic_ns'] < self.last_mono
                or abs(t['available_wall_ns'] - t['available_monotonic_ns'] - self.origin) > 1_000_000
                or t['connection_epoch'] < self.epochs.get(key, 0)):
            raise ValueError('capture identity, sequence, epoch or clock continuity failed')
        self.sequence = t['sequence']
        self.last_wall = t['available_wall_ns']
        self.last_mono = t['available_monotonic_ns']
        self.epochs[key] = t['connection_epoch']
        return at
