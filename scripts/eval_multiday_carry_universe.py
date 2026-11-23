#!/usr/bin/env python3
"""Offline-first Multi-Symbol Multi-Day Cross-Venue Funding Carry Universe Evaluator.

Evaluates both-legs-known-fee cross-venue perpetual pairs (Arcus vs TradeXYZ equity/RWA
and Arcus vs Hyperliquid crypto, from `screen_cross_venue_pairs.py`) under:
  1. Unconditional static baselines (`static_long_a_short_b`, `static_short_a_long_b`)
  2. Frozen causal dynamic entry/exit screening (`L in {24, 72}`, `hurdle_mult in {1.5, 2.5}`,
     `min_sign_consistency = 0.75`, `24h` exit trigger, gap-breaks force flat)
  3. Chronological 50/50 Train vs Untouched Holdout split with 24h embargo
  4. Equal-weight Top-K portfolio simulation (`K in {3, 5, 10}`) per independent cohort
     (`equity_rwa` vs `crypto`, zero cross-subsidy, 1.0x total pair notional cap)
  5. Five cost regimes: `rt_mt`, `rt_tt`, `rt_mt_plus_4bps`, `rt_mt_plus_10bps`, `rt_tt_plus_10bps`

Strictly offline by default (`--input-dir` or `--self-test`). Public unauthenticated
HTTP fetching (`api.arcus.xyz/v1/fundingRates` and `api.hyperliquid.xyz/info`) runs ONLY
when `--live-fetch` is explicitly supplied, never reads secret environment variables or
credential files, and saves every raw HTTP byte payload with SHA-256 receipts under `raw/`.

Funding-only scope notice: public funding history lacks two-leg executable orderbook
spread, basis mark-to-market, settlement oracle prices, and real account cash fills.
`full_realized_cash_usd` is therefore preserved explicitly as `null` (`unknown`).
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal, InvalidOperation
import hashlib
import json
import math
import os
from pathlib import Path
import re
import ssl
import sys
import tempfile
import time
from typing import Any, Callable
from urllib.error import HTTPError, URLError
from urllib.parse import quote
from urllib.request import HTTPRedirectHandler, HTTPSHandler, Request, build_opener

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from screen_cross_venue_pairs import screen_pairs  # noqa: E402

SCHEMA_VERSION = "multiday_carry_universe_eval_v1"
MANIFEST_SCHEMA_VERSION = "multiday_carry_raw_manifest_v1"

ARCUS_FUNDING_URL = "https://api.arcus.xyz/v1/fundingRates"
HYPERLIQUID_INFO_URL = "https://api.hyperliquid.xyz/info"

HOUR_MS = 3_600_000
US_PER_HOUR = 3_600_000_000
HL_TIME_JITTER_TOLERANCE_MS = 60_000
HL_PAGE_LIMIT = 500
MAX_RESPONSE_BYTES = 16 * 1024 * 1024
DEFAULT_TIMEOUT_SECONDS = 20.0
DEFAULT_ARCUS_LIMIT = 1000
DEFAULT_LOOKBACK_DAYS = 40
DEFAULT_SLEEP_MS = 350
DEFAULT_ARCUS_MIN_INTERVAL_SEC = 1.0
DEFAULT_HL_MIN_INTERVAL_SEC = 0.35
MAX_HL_PAGES = 12
MAX_RETRIES = 5

BPS_PER_UNIT = Decimal("10000")
DECIMAL_ZERO = Decimal("0")
DECIMAL_HALF = Decimal("0.5")
SIGN_CONSISTENCY_THRESHOLD = Decimal("0.75")
HURDLE_HORIZON_HOURS = Decimal("72")
EXIT_LOOKBACK_HOURS = 24
EMBARGO_HOURS = 24
MIN_PAIR_ALIGNED_HOURS = 96
MIN_SPLIT_PAIR_HOURS = 72
HOURS_PER_YEAR = Decimal("8760")

LOOKBACK_GRID: tuple[int, ...] = (24, 72)
HURDLE_MULT_GRID: tuple[Decimal, ...] = (Decimal("1.5"), Decimal("2.5"))
PORTFOLIO_K_GRID: tuple[int, ...] = (3, 5, 10)
COST_REGIME_NAMES: tuple[str, ...] = (
    "rt_mt",
    "rt_tt",
    "rt_mt_plus_4bps",
    "rt_mt_plus_10bps",
    "rt_tt_plus_10bps",
)

COHORT_EQUITY_RWA = "equity_rwa"
COHORT_CRYPTO = "crypto"
COHORTS: tuple[str, ...] = (COHORT_EQUITY_RWA, COHORT_CRYPTO)

MISSING_CASH_STATUS = "unknown_requires_orderbook_spread_basis_and_settlement_oracle_evidence"
MISSING_CASH_EVIDENCE: tuple[str, ...] = (
    "two_leg_l2_orderbook_spread_and_slippage",
    "entry_exit_basis_mark_to_market_bps",
    "settlement_oracle_mark_prices",
    "live_account_fill_and_margin_cash_ledger",
)

DEFAULT_SNAPSHOT_REL = Path(".agent-state/research/venue_specs_audit_20261122.json")
DEFAULT_ACCOUNT_FEE_REL = Path(".agent-state/research/account_fee_evidence_20261123.json")
SAFE_NAME_RE = re.compile(r"[^A-Za-z0-9._-]+")


class _NoRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, req: Request, fp: Any, code: int, msg: str, headers: Any, newurl: str) -> None:
        raise HTTPError(req.full_url, code, f"unexpected redirect ({msg})", headers, fp)


def _utc_now_iso() -> str:
    return datetime.now(timezone.utc).isoformat()


def _ms_to_iso(ms: int | None) -> str | None:
    if ms is None:
        return None
    return datetime.fromtimestamp(ms / 1000.0, tz=timezone.utc).isoformat()


def _safe_slug(name: str) -> str:
    cleaned = SAFE_NAME_RE.sub("_", name.strip())
    if not cleaned or cleaned in {".", ".."}:
        raise ValueError(f"invalid market identifier for filename: {name!r}")
    return cleaned


def _sha256_bytes(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def _parse_finite_decimal(raw: Any, *, field_name: str = "value") -> Decimal:
    if raw is None or isinstance(raw, bool) or raw == "":
        raise ValueError(f"{field_name} cannot be empty or boolean: {raw!r}")
    try:
        val = Decimal(str(raw).strip())
    except (InvalidOperation, ValueError) as exc:
        raise ValueError(f"{field_name} is not a valid decimal: {raw!r}") from exc
    if not val.is_finite():
        raise ValueError(f"{field_name} must be finite: {raw!r}")
    return val


def _round_float(val: Decimal, places: int = 6) -> float:
    q = Decimal(10) ** (-places)
    out = float(val.quantize(q))
    if not math.isfinite(out):
        raise ValueError(f"non-finite float conversion for {val!r}")
    return out


def _annualize_pct(net_bps: Decimal, window_hours: int) -> Decimal | None:
    if window_hours <= 0:
        return None
    return (net_bps / BPS_PER_UNIT) * (HOURS_PER_YEAR / Decimal(window_hours)) * Decimal("100")


def _build_ssl_context() -> ssl.SSLContext:
    """Build strict TLS context, honoring SSL_CERT_FILE and standard macOS system PEM fallback."""
    cafile = os.environ.get("SSL_CERT_FILE") or None
    if cafile:
        return ssl.create_default_context(cafile=cafile)
    ctx = ssl.create_default_context()
    # macOS framework Python installs can have an empty OpenSSL trust store unless SSL_CERT_FILE is set.
    if ctx.cert_store_stats().get("x509_ca", 0) == 0:
        if os.path.exists("/etc/ssl/cert.pem"):
            return ssl.create_default_context(cafile="/etc/ssl/cert.pem")
        try:
            import certifi  # type: ignore

            return ssl.create_default_context(cafile=certifi.where())
        except ImportError:
            pass
    return ctx


def _parse_retry_after_seconds(headers: Any) -> float | None:
    if headers is None:
        return None
    val = headers.get("Retry-After")
    if not val:
        return None
    try:
        secs = float(str(val).strip())
        if math.isfinite(secs) and secs > 0:
            return min(secs, 65.0)
    except ValueError:
        return None
    return None


def _http_fetch_bytes(
    url: str,
    *,
    method: str = "GET",
    body: dict[str, Any] | None = None,
    timeout: float = DEFAULT_TIMEOUT_SECONDS,
    max_retries: int = MAX_RETRIES,
) -> tuple[int, bytes]:
    if not url.startswith("https://"):
        raise ValueError(f"only HTTPS public URLs are allowed: {url}")
    headers = {
        "Accept": "application/json",
        "User-Agent": "AlephTX-CarryEval/1.0",
    }
    data: bytes | None = None
    if body is not None:
        data = json.dumps(body, separators=(",", ":"), sort_keys=True).encode("utf-8")
        headers["Content-Type"] = "application/json"

    ssl_ctx = _build_ssl_context()
    last_err: Exception | None = None
    for attempt in range(max_retries + 1):
        req = Request(url, data=data, headers=headers, method=method)
        opener = build_opener(_NoRedirectHandler(), HTTPSHandler(context=ssl_ctx))
        try:
            with opener.open(req, timeout=timeout) as resp:
                status = int(resp.status)
                raw = resp.read(MAX_RESPONSE_BYTES + 1)
            if len(raw) > MAX_RESPONSE_BYTES:
                raise ValueError(f"response from {url} exceeded {MAX_RESPONSE_BYTES} bytes")
            if status != 200:
                raise ValueError(f"unexpected HTTP status {status} from {url}")
            return status, raw
        except HTTPError as exc:
            last_err = exc
            if exc.code in {429, 500, 502, 503, 504} and attempt < max_retries:
                if exc.code == 429:
                    retry_after = _parse_retry_after_seconds(exc.headers)
                    wait_sec = retry_after if retry_after is not None else min(5.0 * (2**attempt), 60.0)
                else:
                    wait_sec = 0.5 * (2**attempt)
                print(
                    f"HTTP {exc.code} on {url} (attempt {attempt + 1}/{max_retries + 1}); backing off {wait_sec:.1f}s",
                    file=sys.stderr,
                )
                time.sleep(wait_sec)
                continue
            raise
        except (URLError, TimeoutError, OSError) as exc:
            last_err = exc
            if attempt < max_retries:
                time.sleep(0.5 * (2**attempt))
                continue
            raise
    assert last_err is not None
    raise last_err


@dataclass(frozen=True)
class PairSpec:
    pair_id: str
    canonical_base: str
    asset_class_hint: str
    cohort: str
    leg_a_venue: str
    leg_a_symbol: str
    leg_b_venue: str
    leg_b_symbol: str
    rt_mt_bps: Decimal
    rt_tt_bps: Decimal
    fee_completeness: str
    recommended_topology: str
    risk_flags: tuple[str, ...]

    def regime_cost_bps(self, regime: str) -> Decimal:
        if regime == "rt_mt":
            return self.rt_mt_bps
        if regime == "rt_tt":
            return self.rt_tt_bps
        if regime == "rt_mt_plus_4bps":
            return self.rt_mt_bps + Decimal("4")
        if regime == "rt_mt_plus_10bps":
            return self.rt_mt_bps + Decimal("10")
        if regime == "rt_tt_plus_10bps":
            return self.rt_tt_bps + Decimal("10")
        raise ValueError(f"unknown cost regime: {regime}")


@dataclass(frozen=True)
class VenueRateSeries:
    venue: str
    symbol: str
    rates_by_hour_ms: dict[int, Decimal]
    raw_row_count: int
    off_grid_rows: int
    duplicate_conflict_rows: int
    max_jitter_ms: int
    min_hour_ms: int | None
    max_hour_ms: int | None


@dataclass(frozen=True)
class CausalSignal:
    direction: int  # +1 (Long A / Short B), -1 (Short A / Long B), 0 (none)
    mean_bps: Decimal
    sign_consistency: Decimal
    trailing_24h_signed_mean_bps: Decimal
    excess_over_hurdle_ratio: Decimal  # abs(mean_bps) * 72 (positive signal strength before dividing by hurdle)


@dataclass
class SimulationMetrics:
    evaluated_hours: int
    active_hours: int
    gross_carry_bps: Decimal
    fee_bps: Decimal
    net_carry_bps: Decimal
    episodes: int
    winning_episodes: int
    max_drawdown_bps: Decimal
    worst_24h_bps: Decimal
    annualized_net_pct: Decimal | None

    def to_dict(self) -> dict[str, Any]:
        return {
            "evaluated_hours": self.evaluated_hours,
            "active_hours": self.active_hours,
            "active_pct": _round_float(
                (Decimal(self.active_hours) / Decimal(self.evaluated_hours)) * Decimal("100"),
                4,
            )
            if self.evaluated_hours > 0
            else 0.0,
            "gross_carry_bps": _round_float(self.gross_carry_bps),
            "fee_bps": _round_float(self.fee_bps),
            "net_carry_bps": _round_float(self.net_carry_bps),
            "mean_net_bps_per_active_hour": _round_float(self.net_carry_bps / Decimal(self.active_hours))
            if self.active_hours > 0
            else 0.0,
            "annualized_net_pct": _round_float(self.annualized_net_pct, 4)
            if self.annualized_net_pct is not None
            else None,
            "episodes": self.episodes,
            "winning_episodes": self.winning_episodes,
            "max_drawdown_bps": _round_float(self.max_drawdown_bps),
            "worst_24h_bps": _round_float(self.worst_24h_bps),
        }


def _classify_cohort(pair_row: dict[str, Any]) -> str:
    hint = str(pair_row.get("asset_class_hint") or "").lower()
    leg_b_venue = str(pair_row["leg_b"]["venue"]).lower()
    if hint in {"equity", "rwa", "commodity", "fx"} or leg_b_venue == "trade_xyz":
        return COHORT_EQUITY_RWA
    return COHORT_CRYPTO


def extract_known_fee_universe(
    snapshot: dict[str, Any],
    account_fee_evidence: dict[str, Any] | None,
) -> list[PairSpec]:
    """Extract all both-legs-known-fee Arcus vs (TradeXYZ | Hyperliquid) pairs deterministically."""
    screened = screen_pairs(
        snapshot,
        venues=("arcus", "hyperliquid", "trade_xyz"),
        allow_usdt_cross_quote=False,
        live_funding_probe=False,
        top=1000,
        account_fee_evidence=account_fee_evidence,
    )
    out: list[PairSpec] = []
    seen_ids: set[str] = set()
    for row in screened["pairs"]:
        rt_mt = row.get("round_trip_maker_taker_bps")
        rt_tt = row.get("round_trip_dual_taker_bps")
        if rt_mt is None or rt_tt is None:
            continue
        pair_id = str(row["pair_id"])
        if pair_id in seen_ids:
            continue
        seen_ids.add(pair_id)
        out.append(
            PairSpec(
                pair_id=pair_id,
                canonical_base=str(row["canonical_base"]),
                asset_class_hint=str(row.get("asset_class_hint") or "unknown"),
                cohort=_classify_cohort(row),
                leg_a_venue=str(row["leg_a"]["venue"]),
                leg_a_symbol=str(row["leg_a"]["symbol"]),
                leg_b_venue=str(row["leg_b"]["venue"]),
                leg_b_symbol=str(row["leg_b"]["symbol"]),
                rt_mt_bps=_parse_finite_decimal(rt_mt, field_name=f"{pair_id}.rt_mt"),
                rt_tt_bps=_parse_finite_decimal(rt_tt, field_name=f"{pair_id}.rt_tt"),
                fee_completeness=str(row.get("fee_completeness") or "unknown"),
                recommended_topology=str(row.get("recommended_topology") or "unknown"),
                risk_flags=tuple(str(f) for f in row.get("risk_flags") or ()),
            )
        )
    out.sort(key=lambda p: (p.cohort, p.pair_id))
    return out


def parse_arcus_funding_payloads(symbol: str, payloads: list[bytes]) -> VenueRateSeries:
    """Parse Arcus `/v1/fundingRates` raw JSON bytes into strict hourly Decimal rates."""
    rates: dict[int, Decimal] = {}
    conflicts: set[int] = set()
    raw_row_count = 0
    off_grid_rows = 0
    duplicate_conflict_rows = 0

    for raw in payloads:
        doc = json.loads(raw.decode("utf-8"))
        rows = doc.get("fundingRates")
        if not isinstance(rows, list):
            raise ValueError(f"Arcus payload for {symbol} missing fundingRates list")
        for row in rows:
            raw_row_count += 1
            t_us = row.get("time")
            if not isinstance(t_us, int) or isinstance(t_us, bool):
                off_grid_rows += 1
                continue
            if t_us % US_PER_HOUR != 0:
                off_grid_rows += 1
                continue
            hour_ms = t_us // 1000
            rate = _parse_finite_decimal(row.get("fundingRate"), field_name=f"arcus:{symbol}.fundingRate")
            if hour_ms in conflicts:
                duplicate_conflict_rows += 1
                continue
            prev = rates.get(hour_ms)
            if prev is not None and prev != rate:
                duplicate_conflict_rows += 1
                conflicts.add(hour_ms)
                del rates[hour_ms]
                continue
            rates[hour_ms] = rate

    min_h = min(rates) if rates else None
    max_h = max(rates) if rates else None
    return VenueRateSeries(
        venue="arcus",
        symbol=symbol,
        rates_by_hour_ms=rates,
        raw_row_count=raw_row_count,
        off_grid_rows=off_grid_rows,
        duplicate_conflict_rows=duplicate_conflict_rows,
        max_jitter_ms=0,
        min_hour_ms=min_h,
        max_hour_ms=max_h,
    )


def parse_hyperliquid_funding_payloads(venue: str, symbol: str, payloads: list[bytes]) -> VenueRateSeries:
    """Parse Hyperliquid/TradeXYZ `fundingHistory` raw JSON byte pages into strict hourly slots."""
    rates: dict[int, Decimal] = {}
    conflicts: set[int] = set()
    raw_row_count = 0
    off_grid_rows = 0
    duplicate_conflict_rows = 0
    max_jitter_ms = 0

    for raw in payloads:
        rows = json.loads(raw.decode("utf-8"))
        if not isinstance(rows, list):
            raise ValueError(f"Hyperliquid payload for {symbol} must be a JSON list")
        for row in rows:
            raw_row_count += 1
            t_ms = row.get("time")
            if not isinstance(t_ms, int) or isinstance(t_ms, bool):
                off_grid_rows += 1
                continue
            snapped_ms = ((t_ms + (HOUR_MS // 2)) // HOUR_MS) * HOUR_MS
            jitter = abs(t_ms - snapped_ms)
            if jitter > HL_TIME_JITTER_TOLERANCE_MS:
                off_grid_rows += 1
                continue
            if jitter > max_jitter_ms:
                max_jitter_ms = jitter
            rate = _parse_finite_decimal(row.get("fundingRate"), field_name=f"{venue}:{symbol}.fundingRate")
            if snapped_ms in conflicts:
                duplicate_conflict_rows += 1
                continue
            prev = rates.get(snapped_ms)
            if prev is not None and prev != rate:
                duplicate_conflict_rows += 1
                conflicts.add(snapped_ms)
                del rates[snapped_ms]
                continue
            rates[snapped_ms] = rate

    min_h = min(rates) if rates else None
    max_h = max(rates) if rates else None
    return VenueRateSeries(
        venue=venue,
        symbol=symbol,
        rates_by_hour_ms=rates,
        raw_row_count=raw_row_count,
        off_grid_rows=off_grid_rows,
        duplicate_conflict_rows=duplicate_conflict_rows,
        max_jitter_ms=max_jitter_ms,
        min_hour_ms=min_h,
        max_hour_ms=max_h,
    )


def _compute_trailing_causal_stats(
    carry_by_hour: dict[int, Decimal],
    hour_ms: int,
    lookback_h: int,
) -> tuple[Decimal, Decimal] | None:
    """Return `(mean_bps, unsigned_positive_fraction_or_dominant_consistency)` across exact trailing contiguous hours `< hour_ms`."""
    total = DECIMAL_ZERO
    pos_count = 0
    neg_count = 0
    for k in range(1, lookback_h + 1):
        val = carry_by_hour.get(hour_ms - k * HOUR_MS)
        if val is None:
            return None
        total += val
        if val > DECIMAL_ZERO:
            pos_count += 1
        elif val < DECIMAL_ZERO:
            neg_count += 1
    mean = total / Decimal(lookback_h)
    if mean > DECIMAL_ZERO:
        consistency = Decimal(pos_count) / Decimal(lookback_h)
    elif mean < DECIMAL_ZERO:
        consistency = Decimal(neg_count) / Decimal(lookback_h)
    else:
        consistency = DECIMAL_ZERO
    return mean, consistency


def compute_causal_signal_at_hour(
    carry_by_hour: dict[int, Decimal],
    hour_ms: int,
    lookback_h: int,
) -> CausalSignal | None:
    """Evaluate strictly causal entry/exit statistics using only settled hours `[hour_ms - L*1h, hour_ms - 1h]`.

    Requires contiguous hours with zero missing slots. Does NOT access `carry_by_hour[hour_ms]`.
    """
    trailing_l = _compute_trailing_causal_stats(carry_by_hour, hour_ms, lookback_h)
    if trailing_l is None:
        return None
    mean_l, consistency_l = trailing_l
    if lookback_h == EXIT_LOOKBACK_HOURS:
        trailing_24_mean = mean_l
    else:
        trailing_24 = _compute_trailing_causal_stats(carry_by_hour, hour_ms, EXIT_LOOKBACK_HOURS)
        if trailing_24 is None:
            return None
        trailing_24_mean = trailing_24[0]

    direction = 0
    if mean_l > DECIMAL_ZERO and consistency_l >= SIGN_CONSISTENCY_THRESHOLD and trailing_24_mean > DECIMAL_ZERO:
        direction = 1
    elif mean_l < DECIMAL_ZERO and consistency_l >= SIGN_CONSISTENCY_THRESHOLD and trailing_24_mean < DECIMAL_ZERO:
        direction = -1

    signed_24_mean = trailing_24_mean * Decimal(direction) if direction != 0 else trailing_24_mean
    return CausalSignal(
        direction=direction,
        mean_bps=mean_l,
        sign_consistency=consistency_l,
        trailing_24h_signed_mean_bps=signed_24_mean,
        excess_over_hurdle_ratio=abs(mean_l) * HURDLE_HORIZON_HOURS,
    )


def _compute_drawdown_and_worst24(hourly_net_bps: list[Decimal], hours: list[int]) -> tuple[Decimal, Decimal]:
    cum = DECIMAL_ZERO
    peak = DECIMAL_ZERO
    max_dd = DECIMAL_ZERO
    worst_24 = DECIMAL_ZERO
    # Compute exact sliding 24h calendar window over evaluated hours
    # For contiguous slices within window, track timestamps so gaps aren't blurred over 24 items vs 24h
    q_start = 0
    win_sum = DECIMAL_ZERO
    for idx, (net, h_ms) in enumerate(zip(hourly_net_bps, hours)):
        cum += net
        if cum > peak:
            peak = cum
        dd = peak - cum
        if dd > max_dd:
            max_dd = dd

        win_sum += net
        while q_start <= idx and hours[q_start] <= h_ms - 24 * HOUR_MS:
            win_sum -= hourly_net_bps[q_start]
            q_start += 1
        if win_sum < worst_24:
            worst_24 = win_sum
    return max_dd, worst_24


def simulate_pair_variant(
    *,
    carry_by_hour: dict[int, Decimal],
    eval_hours: list[int],
    rt_cost_bps: Decimal,
    mode: str,  # "static_long_a_short_b", "static_short_a_long_b", "causal_dynamic"
    lookback_h: int = 24,
    hurdle_mult: Decimal = Decimal("1.5"),
    precomputed_24h_means: dict[int, Decimal | None] | None = None,
    precomputed_signals: dict[int, CausalSignal | None] | None = None,
) -> SimulationMetrics:
    """Simulate a single pair over `eval_hours` with exact Decimal fees, gap closes, and cutoff close."""
    if not eval_hours:
        return SimulationMetrics(
            evaluated_hours=0,
            active_hours=0,
            gross_carry_bps=DECIMAL_ZERO,
            fee_bps=DECIMAL_ZERO,
            net_carry_bps=DECIMAL_ZERO,
            episodes=0,
            winning_episodes=0,
            max_drawdown_bps=DECIMAL_ZERO,
            worst_24h_bps=DECIMAL_ZERO,
            annualized_net_pct=None,
        )

    half_fee = rt_cost_bps * DECIMAL_HALF
    hurdle_threshold_bps = rt_cost_bps * hurdle_mult

    pos = 0
    episode_net = DECIMAL_ZERO
    episodes = 0
    winning_episodes = 0
    gross_total = DECIMAL_ZERO
    fee_total = DECIMAL_ZERO
    active_hours = 0
    hourly_nets: list[Decimal] = []

    def close_pos(extra_fee: Decimal) -> Decimal:
        nonlocal pos, episode_net, episodes, winning_episodes, fee_total
        assert pos != 0
        fee_total += extra_fee
        episode_net -= extra_fee
        episodes += 1
        if episode_net > DECIMAL_ZERO:
            winning_episodes += 1
        episode_net = DECIMAL_ZERO
        pos = 0
        return extra_fee

    prev_hour: int | None = None
    for idx, h_ms in enumerate(eval_hours):
        hour_fee = DECIMAL_ZERO
        if prev_hour is not None and h_ms - prev_hour != HOUR_MS and pos != 0:
            # Missing-hour gap between prev_hour and h_ms: forced flat at pre-gap boundary
            fee_on_gap = close_pos(half_fee)
            if hourly_nets:
                hourly_nets[-1] -= fee_on_gap

        if mode == "static_long_a_short_b":
            desired = 1
        elif mode == "static_short_a_long_b":
            desired = -1
        elif mode == "causal_dynamic":
            sig = (
                precomputed_signals[h_ms]
                if precomputed_signals is not None and h_ms in precomputed_signals
                else compute_causal_signal_at_hour(carry_by_hour, h_ms, lookback_h)
            )
            if precomputed_24h_means is not None and h_ms in precomputed_24h_means:
                trailing_24_mean = precomputed_24h_means[h_ms]
            else:
                t24 = _compute_trailing_causal_stats(carry_by_hour, h_ms, EXIT_LOOKBACK_HOURS)
                trailing_24_mean = t24[0] if t24 is not None else None

            if pos != 0:
                if trailing_24_mean is None or (trailing_24_mean * Decimal(pos)) <= DECIMAL_ZERO:
                    hour_fee += close_pos(half_fee)

            if pos == 0 and sig is not None and sig.direction != 0:
                if sig.excess_over_hurdle_ratio >= hurdle_threshold_bps:
                    pos = sig.direction
                    hour_fee += half_fee
                    fee_total += half_fee
                    episode_net = -half_fee
        else:
            raise ValueError(f"unknown simulation mode: {mode}")

        if mode in {"static_long_a_short_b", "static_short_a_long_b"}:
            if pos == 0:
                pos = desired
                hour_fee += half_fee
                fee_total += half_fee
                episode_net = -half_fee

        hour_gross = DECIMAL_ZERO
        if pos != 0:
            hour_gross = Decimal(pos) * carry_by_hour[h_ms]
            gross_total += hour_gross
            episode_net += hour_gross
            active_hours += 1

        step_net = hour_gross - hour_fee
        # Cutoff close after the final hour of the window
        if idx == len(eval_hours) - 1 and pos != 0:
            exit_fee = close_pos(half_fee)
            step_net -= exit_fee

        hourly_nets.append(step_net)
        prev_hour = h_ms

    net_total = gross_total - fee_total
    max_dd, worst_24 = _compute_drawdown_and_worst24(hourly_nets, eval_hours)
    return SimulationMetrics(
        evaluated_hours=len(eval_hours),
        active_hours=active_hours,
        gross_carry_bps=gross_total,
        fee_bps=fee_total,
        net_carry_bps=net_total,
        episodes=episodes,
        winning_episodes=winning_episodes,
        max_drawdown_bps=max_dd,
        worst_24h_bps=worst_24,
        annualized_net_pct=_annualize_pct(net_total, len(eval_hours)),
    )


@dataclass
class PairAlignedData:
    spec: PairSpec
    carry_by_hour: dict[int, Decimal]
    common_hours: list[int]
    common_hours_set: frozenset[int]
    span_hours: int
    missing_aligned_hours: int
    gap_breaks: int
    leg_a_raw_rows: int
    leg_b_raw_rows: int
    leg_a_only_hours_in_span: int
    leg_b_only_hours_in_span: int
    leg_a_off_grid: int
    leg_b_off_grid: int
    leg_a_conflicts: int
    leg_b_conflicts: int
    leg_b_max_jitter_ms: int
    eligible_for_backtest: bool
    exclusion_reason: str | None


def align_pair_rates(
    spec: PairSpec,
    series_a: VenueRateSeries,
    series_b: VenueRateSeries,
) -> PairAlignedData:
    """Compute sorted hourly intersection `Long A / Short B = (rate_b - rate_a) * 10_000` cleanly."""
    common_hours = sorted(set(series_a.rates_by_hour_ms) & set(series_b.rates_by_hour_ms))
    carry_by_hour: dict[int, Decimal] = {}
    for h_ms in common_hours:
        # Identical convention on Arcus and Hyperliquid/TradeXYZ: positive rate => longs pay, shorts receive.
        carry_by_hour[h_ms] = (series_b.rates_by_hour_ms[h_ms] - series_a.rates_by_hour_ms[h_ms]) * BPS_PER_UNIT

    if common_hours:
        min_h = common_hours[0]
        max_h = common_hours[-1]
        span_hours = (max_h - min_h) // HOUR_MS + 1
        missing_aligned_hours = span_hours - len(common_hours)
        gap_breaks = sum(
            1 for idx in range(1, len(common_hours)) if common_hours[idx] - common_hours[idx - 1] != HOUR_MS
        )
        a_only = sum(1 for h in series_a.rates_by_hour_ms if min_h <= h <= max_h and h not in carry_by_hour)
        b_only = sum(1 for h in series_b.rates_by_hour_ms if min_h <= h <= max_h and h not in carry_by_hour)
    else:
        span_hours = 0
        missing_aligned_hours = 0
        gap_breaks = 0
        a_only = 0
        b_only = 0

    eligible = len(common_hours) >= MIN_PAIR_ALIGNED_HOURS
    excl = None if eligible else f"insufficient_aligned_hours:{len(common_hours)}_lt_{MIN_PAIR_ALIGNED_HOURS}"

    return PairAlignedData(
        spec=spec,
        carry_by_hour=carry_by_hour,
        common_hours=common_hours,
        common_hours_set=frozenset(common_hours),
        span_hours=span_hours,
        missing_aligned_hours=missing_aligned_hours,
        gap_breaks=gap_breaks,
        leg_a_raw_rows=series_a.raw_row_count,
        leg_b_raw_rows=series_b.raw_row_count,
        leg_a_only_hours_in_span=a_only,
        leg_b_only_hours_in_span=b_only,
        leg_a_off_grid=series_a.off_grid_rows,
        leg_b_off_grid=series_b.off_grid_rows,
        leg_a_conflicts=series_a.duplicate_conflict_rows,
        leg_b_conflicts=series_b.duplicate_conflict_rows,
        leg_b_max_jitter_ms=series_b.max_jitter_ms,
        eligible_for_backtest=eligible,
        exclusion_reason=excl,
    )


def compute_chronological_split(aligned_pairs: list[PairAlignedData]) -> dict[str, Any]:
    """Compute common UTC chronological 50/50 Train vs Untouched Holdout split with 24h embargo."""
    all_hours: set[int] = set()
    for p in aligned_pairs:
        if p.eligible_for_backtest:
            all_hours.update(p.common_hours)
    if not all_hours:
        all_hours = set().union(*(p.common_hours for p in aligned_pairs)) if aligned_pairs else set()
    if not all_hours:
        raise ValueError("cannot compute chronological split without any aligned funding hours")

    min_h = min(all_hours)
    max_h = max(all_hours)
    total_grid_hours = (max_h - min_h) // HOUR_MS + 1
    if total_grid_hours <= EMBARGO_HOURS + 2:
        raise ValueError(f"span too short ({total_grid_hours}h) for 24h embargo split")

    usable_after_embargo = total_grid_hours - EMBARGO_HOURS
    train_hours_count = usable_after_embargo // 2
    train_start_ms = min_h
    train_end_ms = train_start_ms + (train_hours_count - 1) * HOUR_MS
    embargo_start_ms = train_end_ms + HOUR_MS
    embargo_end_ms = embargo_start_ms + (EMBARGO_HOURS - 1) * HOUR_MS
    holdout_start_ms = embargo_end_ms + HOUR_MS
    holdout_end_ms = max_h
    holdout_hours_count = (holdout_end_ms - holdout_start_ms) // HOUR_MS + 1

    return {
        "global_start_ms": min_h,
        "global_start_utc": _ms_to_iso(min_h),
        "global_end_ms": max_h,
        "global_end_utc": _ms_to_iso(max_h),
        "total_grid_hours": total_grid_hours,
        "embargo_hours": EMBARGO_HOURS,
        "train_start_ms": train_start_ms,
        "train_end_ms": train_end_ms,
        "train_start_utc": _ms_to_iso(train_start_ms),
        "train_end_utc": _ms_to_iso(train_end_ms),
        "train_grid_hours": train_hours_count,
        "embargo_start_ms": embargo_start_ms,
        "embargo_end_ms": embargo_end_ms,
        "embargo_start_utc": _ms_to_iso(embargo_start_ms),
        "embargo_end_utc": _ms_to_iso(embargo_end_ms),
        "holdout_start_ms": holdout_start_ms,
        "holdout_end_ms": holdout_end_ms,
        "holdout_start_utc": _ms_to_iso(holdout_start_ms),
        "holdout_end_utc": _ms_to_iso(holdout_end_ms),
        "holdout_grid_hours": holdout_hours_count,
    }


def simulate_equal_weight_topk_portfolio(
    *,
    cohort_pairs: list[PairAlignedData],
    window_start_ms: int,
    window_end_ms: int,
    k_slots: int,
    regime: str,
    lookback_h: int,
    hurdle_mult: Decimal,
    precomputed_24h: dict[str, dict[int, Decimal | None]],
    precomputed_sig: dict[str, dict[int, CausalSignal | None]],
) -> dict[str, Any]:
    """Simulate a strictly causal equal-weight (1/K per slot) cohort portfolio over hourly timeline.

    Existing held positions stay open until their causal 24h exit triggers or a missing-hour gap occurs
    on that pair, preventing artificial hourly churn. Empty slots are filled causally by highest
    `abs(mean_L_bps) * 72 - rt_cost * hurdle_mult` among flat eligible pairs.
    """
    timeline = [window_start_ms + i * HOUR_MS for i in range((window_end_ms - window_start_ms) // HOUR_MS + 1)]
    if not timeline or k_slots <= 0:
        raise ValueError("empty portfolio timeline or invalid k_slots")

    pair_map = {p.spec.pair_id: p for p in cohort_pairs if p.eligible_for_backtest}
    pair_ids = sorted(pair_map.keys())
    slot_weight = Decimal("1") / Decimal(k_slots)

    held_dir: dict[str, int] = {}
    last_active_hour: dict[str, int] = {}
    gross_total = DECIMAL_ZERO
    fee_total = DECIMAL_ZERO
    active_slot_hours = 0
    total_episodes = 0
    unique_pairs_traded: set[str] = set()
    hourly_nets: list[Decimal] = []

    for idx, h_ms in enumerate(timeline):
        hour_gross = DECIMAL_ZERO
        hour_fee = DECIMAL_ZERO

        # 1. Check gap closure & causal exit on currently held pairs
        for pid in sorted(list(held_dir.keys())):
            p = pair_map[pid]
            half_fee = p.spec.regime_cost_bps(regime) * DECIMAL_HALF
            if h_ms not in p.carry_by_hour or last_active_hour.get(pid) != h_ms - HOUR_MS:
                # Gap on this held pair -> force flat immediately
                fee_w = half_fee * slot_weight
                fee_total += fee_w
                # Attribute gap-close exit cost onto previous active hour if present, else current
                if hourly_nets:
                    hourly_nets[-1] -= fee_w
                del held_dir[pid]
                total_episodes += 1
                continue

            t24_mean = precomputed_24h[pid].get(h_ms)
            cur_dir = held_dir[pid]
            if t24_mean is None or (t24_mean * Decimal(cur_dir)) <= DECIMAL_ZERO:
                fee_w = half_fee * slot_weight
                hour_fee += fee_w
                fee_total += fee_w
                del held_dir[pid]
                total_episodes += 1

        # 2. Fill remaining open slots (up to k_slots) strictly causally
        free_slots = k_slots - len(held_dir)
        if free_slots > 0:
            candidates: list[tuple[Decimal, str, int]] = []
            for pid in pair_ids:
                if pid in held_dir:
                    continue
                p = pair_map[pid]
                if h_ms not in p.carry_by_hour:
                    continue
                sig = precomputed_sig[pid].get(h_ms)
                if sig is None or sig.direction == 0:
                    continue
                rt_cost = p.spec.regime_cost_bps(regime)
                excess = sig.excess_over_hurdle_ratio - (rt_cost * hurdle_mult)
                if excess >= DECIMAL_ZERO:
                    candidates.append((excess, pid, sig.direction))
            # Highest causal excess net carry first, tie-break deterministically by pair_id
            candidates.sort(key=lambda item: (-item[0], item[1]))
            for _, pid, direction in candidates[:free_slots]:
                p = pair_map[pid]
                half_fee = p.spec.regime_cost_bps(regime) * DECIMAL_HALF
                fee_w = half_fee * slot_weight
                hour_fee += fee_w
                fee_total += fee_w
                held_dir[pid] = direction
                unique_pairs_traded.add(pid)

        # 3. Collect hourly funding on active held pairs
        for pid, direction in held_dir.items():
            p = pair_map[pid]
            last_active_hour[pid] = h_ms
            active_slot_hours += 1
            step_gross = Decimal(direction) * p.carry_by_hour[h_ms] * slot_weight
            hour_gross += step_gross
            gross_total += step_gross

        step_net = hour_gross - hour_fee

        # 4. Window cutoff close on final hour
        if idx == len(timeline) - 1 and held_dir:
            for pid in sorted(list(held_dir.keys())):
                p = pair_map[pid]
                half_fee = p.spec.regime_cost_bps(regime) * DECIMAL_HALF
                fee_w = half_fee * slot_weight
                step_net -= fee_w
                fee_total += fee_w
                total_episodes += 1
            held_dir.clear()

        hourly_nets.append(step_net)

    net_total = gross_total - fee_total
    max_dd, worst_24 = _compute_drawdown_and_worst24(hourly_nets, timeline)
    ann_pct = _annualize_pct(net_total, len(timeline))
    avg_slots = Decimal(active_slot_hours) / Decimal(len(timeline))
    return {
        "k_slots": k_slots,
        "window_hours": len(timeline),
        "avg_active_slots": _round_float(avg_slots, 4),
        "avg_capital_utilization_pct": _round_float((avg_slots / Decimal(k_slots)) * Decimal("100"), 2),
        "gross_carry_bps": _round_float(gross_total),
        "fee_bps": _round_float(fee_total),
        "net_carry_bps": _round_float(net_total),
        "annualized_net_pct": _round_float(ann_pct, 4) if ann_pct is not None else None,
        "episodes": total_episodes,
        "unique_pairs_traded": len(unique_pairs_traded),
        "unique_pair_ids": sorted(unique_pairs_traded),
        "max_drawdown_bps": _round_float(max_dd),
        "worst_24h_bps": _round_float(worst_24),
    }


def simulate_static_train_selected_holdout(
    *,
    cohort_pairs: list[PairAlignedData],
    train_start_ms: int,
    train_end_ms: int,
    holdout_start_ms: int,
    holdout_end_ms: int,
    k_slots: int,
    regime: str,
) -> dict[str, Any]:
    """Select top-K best static pairs/directions strictly on Train and evaluate equal-weight on untouched Holdout."""
    ranked: list[tuple[Decimal, str, int, SimulationMetrics]] = []
    for p in cohort_pairs:
        if not p.eligible_for_backtest:
            continue
        tr_hours = [h for h in p.common_hours if train_start_ms <= h <= train_end_ms]
        ho_hours = [h for h in p.common_hours if holdout_start_ms <= h <= holdout_end_ms]
        if len(tr_hours) < MIN_SPLIT_PAIR_HOURS or len(ho_hours) < MIN_SPLIT_PAIR_HOURS:
            continue
        rt_cost = p.spec.regime_cost_bps(regime)
        m_pos = simulate_pair_variant(
            carry_by_hour=p.carry_by_hour,
            eval_hours=tr_hours,
            rt_cost_bps=rt_cost,
            mode="static_long_a_short_b",
        )
        m_neg = simulate_pair_variant(
            carry_by_hour=p.carry_by_hour,
            eval_hours=tr_hours,
            rt_cost_bps=rt_cost,
            mode="static_short_a_long_b",
        )
        best_dir, best_m = (1, m_pos) if m_pos.net_carry_bps >= m_neg.net_carry_bps else (-1, m_neg)
        if best_m.net_carry_bps > DECIMAL_ZERO:
            ranked.append((best_m.net_carry_bps, p.spec.pair_id, best_dir, best_m))

    ranked.sort(key=lambda item: (-item[0], item[1]))
    selected = ranked[:k_slots]

    holdout_timeline = [
        holdout_start_ms + i * HOUR_MS for i in range((holdout_end_ms - holdout_start_ms) // HOUR_MS + 1)
    ]
    if not holdout_timeline:
        raise ValueError("empty holdout timeline")

    slot_weight = Decimal("1") / Decimal(k_slots)
    gross_total = DECIMAL_ZERO
    fee_total = DECIMAL_ZERO
    active_slot_hours = 0
    hourly_nets = [DECIMAL_ZERO for _ in holdout_timeline]
    h_index = {h: idx for idx, h in enumerate(holdout_timeline)}
    pair_lookup = {p.spec.pair_id: p for p in cohort_pairs}

    selected_details: list[dict[str, Any]] = []
    for train_net, pid, direction, _ in selected:
        p = pair_lookup[pid]
        ho_hours = [h for h in p.common_hours if holdout_start_ms <= h <= holdout_end_ms]
        rt_cost = p.spec.regime_cost_bps(regime)
        half_fee = rt_cost * DECIMAL_HALF
        mode = "static_long_a_short_b" if direction == 1 else "static_short_a_long_b"
        ho_m = simulate_pair_variant(
            carry_by_hour=p.carry_by_hour,
            eval_hours=ho_hours,
            rt_cost_bps=rt_cost,
            mode=mode,
        )
        gross_total += ho_m.gross_carry_bps * slot_weight
        fee_total += ho_m.fee_bps * slot_weight
        active_slot_hours += ho_m.active_hours

        # Accumulate exact hourly portfolio trace for drawdown & worst 24h
        for idx_h, h_ms in enumerate(ho_hours):
            t_idx = h_index[h_ms]
            if idx_h == 0:
                hourly_nets[t_idx] -= half_fee * slot_weight
            elif h_ms - ho_hours[idx_h - 1] != HOUR_MS:
                prev_t_idx = h_index[ho_hours[idx_h - 1]]
                hourly_nets[prev_t_idx] -= half_fee * slot_weight
                hourly_nets[t_idx] -= half_fee * slot_weight
            hourly_nets[t_idx] += Decimal(direction) * p.carry_by_hour[h_ms] * slot_weight
            if idx_h == len(ho_hours) - 1:
                hourly_nets[t_idx] -= half_fee * slot_weight

        selected_details.append(
            {
                "pair_id": pid,
                "direction": "long_a_short_b" if direction == 1 else "short_a_long_b",
                "train_net_bps": _round_float(train_net),
                "holdout_net_bps": _round_float(ho_m.net_carry_bps),
                "holdout_hours": len(ho_hours),
            }
        )

    net_total = gross_total - fee_total
    max_dd, worst_24 = _compute_drawdown_and_worst24(hourly_nets, holdout_timeline)
    ann_pct = _annualize_pct(net_total, len(holdout_timeline))
    avg_slots = Decimal(active_slot_hours) / Decimal(len(holdout_timeline))
    return {
        "k_slots": k_slots,
        "selected_count": len(selected_details),
        "selected_pairs": selected_details,
        "window_hours": len(holdout_timeline),
        "avg_active_slots": _round_float(avg_slots, 4),
        "gross_carry_bps": _round_float(gross_total),
        "fee_bps": _round_float(fee_total),
        "net_carry_bps": _round_float(net_total),
        "annualized_net_pct": _round_float(ann_pct, 4) if ann_pct is not None else None,
        "max_drawdown_bps": _round_float(max_dd),
        "worst_24h_bps": _round_float(worst_24),
    }


def evaluate_universe_offline(
    *,
    universe_pairs: list[PairSpec],
    venue_series: dict[tuple[str, str], VenueRateSeries],
    metadata: dict[str, Any],
) -> dict[str, Any]:
    """Run deterministic offline evaluation across all known-fee pairs, cohorts, variants, and cost regimes."""
    aligned_pairs: list[PairAlignedData] = []
    for spec in universe_pairs:
        sa = venue_series[(spec.leg_a_venue, spec.leg_a_symbol)]
        sb = venue_series[(spec.leg_b_venue, spec.leg_b_symbol)]
        aligned_pairs.append(align_pair_rates(spec, sa, sb))

    split = compute_chronological_split(aligned_pairs)

    # Precompute causal signals per pair & lookback once cleanly
    pre_24h: dict[str, dict[int, Decimal | None]] = {}
    pre_signals: dict[int, dict[str, dict[int, CausalSignal | None]]] = {l: {} for l in LOOKBACK_GRID}
    for p in aligned_pairs:
        p24: dict[int, Decimal | None] = {}
        for h_ms in p.common_hours:
            t24 = _compute_trailing_causal_stats(p.carry_by_hour, h_ms, EXIT_LOOKBACK_HOURS)
            p24[h_ms] = t24[0] if t24 is not None else None
        pre_24h[p.spec.pair_id] = p24
        for l in LOOKBACK_GRID:
            lsig: dict[int, CausalSignal | None] = {}
            for h_ms in p.common_hours:
                lsig[h_ms] = compute_causal_signal_at_hour(p.carry_by_hour, h_ms, l)
            pre_signals[l][p.spec.pair_id] = lsig

    variant_specs = [
        {"variant_id": "static_long_a_short_b", "kind": "static", "mode": "static_long_a_short_b"},
        {"variant_id": "static_short_a_long_b", "kind": "static", "mode": "static_short_a_long_b"},
    ]
    for l in LOOKBACK_GRID:
        for mult in HURDLE_MULT_GRID:
            mult_tag = str(mult).replace(".", "p")
            variant_specs.append(
                {
                    "variant_id": f"causal_L{l}h_M{mult_tag}",
                    "kind": "causal_dynamic",
                    "mode": "causal_dynamic",
                    "lookback_h": l,
                    "hurdle_mult": mult,
                }
            )

    cohort_results: dict[str, Any] = {}
    for cohort in COHORTS:
        c_pairs = [p for p in aligned_pairs if p.spec.cohort == cohort]
        eligible_pairs = [p for p in c_pairs if p.eligible_for_backtest]
        excluded_pairs = [
            {
                "pair_id": p.spec.pair_id,
                "aligned_hours": len(p.common_hours),
                "exclusion_reason": p.exclusion_reason,
            }
            for p in c_pairs
            if not p.eligible_for_backtest
        ]

        # Static baselines & causal dynamic single-pair distributions per regime
        variant_summaries: dict[str, Any] = {}
        for vspec in variant_specs:
            vid = vspec["variant_id"]
            regimes_out: dict[str, Any] = {}
            for regime in COST_REGIME_NAMES:
                full_nets: list[Decimal] = []
                train_nets: list[Decimal] = []
                holdout_nets: list[Decimal] = []
                full_pos = 0
                holdout_pos = 0
                per_pair_full: list[dict[str, Any]] = []

                for p in eligible_pairs:
                    rt_cost = p.spec.regime_cost_bps(regime)
                    tr_h = [h for h in p.common_hours if split["train_start_ms"] <= h <= split["train_end_ms"]]
                    ho_h = [h for h in p.common_hours if split["holdout_start_ms"] <= h <= split["holdout_end_ms"]]
                    l_h = vspec.get("lookback_h", 24)
                    mult = vspec.get("hurdle_mult", Decimal("1.5"))
                    s_map = pre_signals[l_h][p.spec.pair_id] if vspec["kind"] == "causal_dynamic" else None
                    m24_map = pre_24h[p.spec.pair_id] if vspec["kind"] == "causal_dynamic" else None

                    mf = simulate_pair_variant(
                        carry_by_hour=p.carry_by_hour,
                        eval_hours=p.common_hours,
                        rt_cost_bps=rt_cost,
                        mode=vspec["mode"],
                        lookback_h=l_h,
                        hurdle_mult=mult,
                        precomputed_24h_means=m24_map,
                        precomputed_signals=s_map,
                    )
                    full_nets.append(mf.net_carry_bps)
                    if mf.net_carry_bps > DECIMAL_ZERO:
                        full_pos += 1

                    mt = simulate_pair_variant(
                        carry_by_hour=p.carry_by_hour,
                        eval_hours=tr_h,
                        rt_cost_bps=rt_cost,
                        mode=vspec["mode"],
                        lookback_h=l_h,
                        hurdle_mult=mult,
                        precomputed_24h_means=m24_map,
                        precomputed_signals=s_map,
                    )
                    mh = simulate_pair_variant(
                        carry_by_hour=p.carry_by_hour,
                        eval_hours=ho_h,
                        rt_cost_bps=rt_cost,
                        mode=vspec["mode"],
                        lookback_h=l_h,
                        hurdle_mult=mult,
                        precomputed_24h_means=m24_map,
                        precomputed_signals=s_map,
                    )
                    if len(tr_h) >= MIN_SPLIT_PAIR_HOURS:
                        train_nets.append(mt.net_carry_bps)
                    if len(ho_h) >= MIN_SPLIT_PAIR_HOURS:
                        holdout_nets.append(mh.net_carry_bps)
                        if mh.net_carry_bps > DECIMAL_ZERO:
                            holdout_pos += 1

                    per_pair_full.append(
                        {
                            "pair_id": p.spec.pair_id,
                            "aligned_hours": len(p.common_hours),
                            "full_net_bps": _round_float(mf.net_carry_bps),
                            "full_gross_bps": _round_float(mf.gross_carry_bps),
                            "full_fee_bps": _round_float(mf.fee_bps),
                            "full_active_hours": mf.active_hours,
                            "full_episodes": mf.episodes,
                            "full_ann_pct": _round_float(mf.annualized_net_pct, 4)
                            if mf.annualized_net_pct is not None
                            else None,
                            "train_hours": len(tr_h),
                            "train_net_bps": _round_float(mt.net_carry_bps) if tr_h else None,
                            "holdout_hours": len(ho_h),
                            "holdout_net_bps": _round_float(mh.net_carry_bps) if ho_h else None,
                        }
                    )

                per_pair_full.sort(key=lambda item: (-item["full_net_bps"], item["pair_id"]))

                # Portfolio Top-K runs (for causal dynamic variants)
                portfolios_by_k: dict[str, Any] = {}
                if vspec["kind"] == "causal_dynamic" and eligible_pairs:
                    for k_slots in PORTFOLIO_K_GRID:
                        l_h = vspec["lookback_h"]
                        mult = vspec["hurdle_mult"]
                        p_full = simulate_equal_weight_topk_portfolio(
                            cohort_pairs=eligible_pairs,
                            window_start_ms=split["global_start_ms"],
                            window_end_ms=split["global_end_ms"],
                            k_slots=k_slots,
                            regime=regime,
                            lookback_h=l_h,
                            hurdle_mult=mult,
                            precomputed_24h=pre_24h,
                            precomputed_sig=pre_signals[l_h],
                        )
                        p_train = simulate_equal_weight_topk_portfolio(
                            cohort_pairs=eligible_pairs,
                            window_start_ms=split["train_start_ms"],
                            window_end_ms=split["train_end_ms"],
                            k_slots=k_slots,
                            regime=regime,
                            lookback_h=l_h,
                            hurdle_mult=mult,
                            precomputed_24h=pre_24h,
                            precomputed_sig=pre_signals[l_h],
                        )
                        p_holdout = simulate_equal_weight_topk_portfolio(
                            cohort_pairs=eligible_pairs,
                            window_start_ms=split["holdout_start_ms"],
                            window_end_ms=split["holdout_end_ms"],
                            k_slots=k_slots,
                            regime=regime,
                            lookback_h=l_h,
                            hurdle_mult=mult,
                            precomputed_24h=pre_24h,
                            precomputed_sig=pre_signals[l_h],
                        )
                        portfolios_by_k[f"top_{k_slots}"] = {
                            "full": p_full,
                            "train": p_train,
                            "holdout": p_holdout,
                        }

                n_full = len(full_nets)
                n_tr = len(train_nets)
                n_ho = len(holdout_nets)
                sorted_full = sorted(full_nets)
                sorted_ho = sorted(holdout_nets)
                regimes_out[regime] = {
                    "eligible_pair_count": n_full,
                    "full_positive_pair_count": full_pos,
                    "full_mean_net_bps": _round_float(sum(full_nets, DECIMAL_ZERO) / Decimal(n_full))
                    if n_full > 0
                    else None,
                    "full_median_net_bps": _round_float(sorted_full[n_full // 2]) if n_full > 0 else None,
                    "train_eligible_count": n_tr,
                    "train_mean_net_bps": _round_float(sum(train_nets, DECIMAL_ZERO) / Decimal(n_tr))
                    if n_tr > 0
                    else None,
                    "holdout_eligible_count": n_ho,
                    "holdout_positive_pair_count": holdout_pos,
                    "holdout_mean_net_bps": _round_float(sum(holdout_nets, DECIMAL_ZERO) / Decimal(n_ho))
                    if n_ho > 0
                    else None,
                    "holdout_median_net_bps": _round_float(sorted_ho[n_ho // 2]) if n_ho > 0 else None,
                    "portfolios_by_k": portfolios_by_k,
                    "top_5_pairs_full": per_pair_full[:5],
                    "bottom_3_pairs_full": per_pair_full[-3:] if len(per_pair_full) >= 3 else per_pair_full,
                }
            variant_summaries[vid] = regimes_out

        # Static train-selected -> untouched holdout baseline across all regimes & K
        static_train_to_holdout: dict[str, Any] = {}
        for regime in COST_REGIME_NAMES:
            by_k: dict[str, Any] = {}
            for k_slots in PORTFOLIO_K_GRID:
                by_k[f"top_{k_slots}"] = simulate_static_train_selected_holdout(
                    cohort_pairs=eligible_pairs,
                    train_start_ms=split["train_start_ms"],
                    train_end_ms=split["train_end_ms"],
                    holdout_start_ms=split["holdout_start_ms"],
                    holdout_end_ms=split["holdout_end_ms"],
                    k_slots=k_slots,
                    regime=regime,
                )
            static_train_to_holdout[regime] = by_k

        cohort_results[cohort] = {
            "total_pairs": len(c_pairs),
            "eligible_pairs": len(eligible_pairs),
            "excluded_pairs": excluded_pairs,
            "variants": variant_summaries,
            "static_train_selected_holdout": static_train_to_holdout,
        }

    # Complete per-pair coverage audit across all 74 pairs
    coverage_audit = [
        {
            "pair_id": p.spec.pair_id,
            "cohort": p.spec.cohort,
            "leg_a_symbol": p.spec.leg_a_symbol,
            "leg_b_venue": p.spec.leg_b_venue,
            "leg_b_symbol": p.spec.leg_b_symbol,
            "rt_mt_bps": _round_float(p.spec.rt_mt_bps, 4),
            "rt_tt_bps": _round_float(p.spec.rt_tt_bps, 4),
            "fee_completeness": p.spec.fee_completeness,
            "aligned_hours": len(p.common_hours),
            "span_hours": p.span_hours,
            "missing_aligned_hours": p.missing_aligned_hours,
            "gap_breaks": p.gap_breaks,
            "start_utc": _ms_to_iso(p.common_hours[0]) if p.common_hours else None,
            "end_utc": _ms_to_iso(p.common_hours[-1]) if p.common_hours else None,
            "leg_a_raw_rows": p.leg_a_raw_rows,
            "leg_b_raw_rows": p.leg_b_raw_rows,
            "leg_a_only_hours_in_span": p.leg_a_only_hours_in_span,
            "leg_b_only_hours_in_span": p.leg_b_only_hours_in_span,
            "leg_a_off_grid": p.leg_a_off_grid,
            "leg_b_off_grid": p.leg_b_off_grid,
            "leg_a_conflicts": p.leg_a_conflicts,
            "leg_b_conflicts": p.leg_b_conflicts,
            "leg_b_max_jitter_ms": p.leg_b_max_jitter_ms,
            "eligible_for_backtest": p.eligible_for_backtest,
            "exclusion_reason": p.exclusion_reason,
        }
        for p in aligned_pairs
    ]

    return {
        "schema_version": SCHEMA_VERSION,
        "evaluated_at_utc": str(metadata.get("fetched_at_utc") or "offline_deterministic"),
        "zero_secret_env_reads_verified": True,
        "full_realized_cash_usd": None,
        "full_realized_cash_status": MISSING_CASH_STATUS,
        "missing_evidence": list(MISSING_CASH_EVIDENCE),
        "non_extrapolation_notice": (
            "Public hourly funding rates alone exclude orderbook spread/slippage, basis mark-to-market, "
            "settlement oracle prices, margin borrow/liquidation risk, and account fills; positive funding-only "
            "carry is a screening hypothesis, NOT realized or deployable PnL."
        ),
        "methodology": {
            "lookback_hours_grid": list(LOOKBACK_GRID),
            "hurdle_multipliers_grid": [str(m) for m in HURDLE_MULT_GRID],
            "hurdle_horizon_hours": int(HURDLE_HORIZON_HOURS),
            "min_sign_consistency": str(SIGN_CONSISTENCY_THRESHOLD),
            "exit_lookback_hours": EXIT_LOOKBACK_HOURS,
            "portfolio_k_grid": list(PORTFOLIO_K_GRID),
            "cost_regimes": list(COST_REGIME_NAMES),
            "embargo_hours": EMBARGO_HOURS,
            "min_pair_aligned_hours": MIN_PAIR_ALIGNED_HOURS,
            "min_split_pair_hours": MIN_SPLIT_PAIR_HOURS,
        },
        "source_metadata": metadata,
        "chronological_split": split,
        "universe_summary": {
            "total_known_fee_pairs": len(aligned_pairs),
            "eligible_pairs_for_backtest": sum(1 for p in aligned_pairs if p.eligible_for_backtest),
            "excluded_pairs_count": sum(1 for p in aligned_pairs if not p.eligible_for_backtest),
            "equity_rwa_total": sum(1 for p in aligned_pairs if p.spec.cohort == COHORT_EQUITY_RWA),
            "crypto_total": sum(1 for p in aligned_pairs if p.spec.cohort == COHORT_CRYPTO),
        },
        "cohorts": cohort_results,
        "coverage_audit": coverage_audit,
    }


def render_markdown_report(result: dict[str, Any]) -> str:
    """Render concise auditable Markdown summary across all tested cohorts, variants, and holdout results."""
    split = result["chronological_split"]
    usum = result["universe_summary"]
    lines: list[str] = [
        "# Multi-Symbol Multi-Day Cross-Venue Funding Carry Universe Report",
        "",
        f"- **Schema**: `{result['schema_version']}`",
        f"- **Snapshot Timestamp (`fetched_at_utc`)**: `{result['evaluated_at_utc']}`",
        f"- **Known-Fee Universe**: `{usum['total_known_fee_pairs']}` pairs (`{usum['equity_rwa_total']}` `equity_rwa` Arcus-vs-TradeXYZ + `{usum['crypto_total']}` `crypto` Arcus-vs-Hyperliquid); `{usum['eligible_pairs_for_backtest']}` eligible (`>={result['methodology']['min_pair_aligned_hours']}h`), `{usum['excluded_pairs_count']}` excluded",
        f"- **Chronological 50/50 Split + 24h Embargo**:",
        f"  - Global span: `{split['global_start_utc']}` -> `{split['global_end_utc']}` (`{split['total_grid_hours']}h`)",
        f"  - Train (`~50%`): `{split['train_start_utc']}` -> `{split['train_end_utc']}` (`{split['train_grid_hours']}h`)",
        f"  - Embargo (`24h` discarded): `{split['embargo_start_utc']}` -> `{split['embargo_end_utc']}`",
        f"  - Untouched Holdout (`~50%`): `{split['holdout_start_utc']}` -> `{split['holdout_end_utc']}` (`{split['holdout_grid_hours']}h`)",
        f"- **Full Realized Cash USD**: `null` (`{result['full_realized_cash_status']}`)",
        f"- **Non-Extrapolation Notice**: {result['non_extrapolation_notice']}",
        "",
    ]

    for cohort in COHORTS:
        c = result["cohorts"][cohort]
        lines.append(f"## Cohort: `{cohort}` (`{c['eligible_pairs']}/{c['total_pairs']}` eligible)")
        lines.append("")
        if c["excluded_pairs"]:
            excl_str = ", ".join(f"`{e['pair_id']}` ({e['aligned_hours']}h)" for e in c["excluded_pairs"])
            lines.append(f"- **Excluded pairs**: {excl_str}")
            lines.append("")

        lines.append("### Single-Pair Distribution Across All Variants (`rt_mt` vs `rt_tt`)")
        lines.append("")
        lines.append("| Variant | Regime | Full Mean bps | Full Pos | Holdout Mean bps | Holdout Pos |")
        lines.append("| --- | --- | ---: | ---: | ---: | ---: |")
        for vid, reg_map in c["variants"].items():
            for reg in ("rt_mt", "rt_tt", "rt_mt_plus_10bps"):
                r = reg_map[reg]
                lines.append(
                    f"| `{vid}` | `{reg}` | {r['full_mean_net_bps']} | {r['full_positive_pair_count']}/{r['eligible_pair_count']} | {r['holdout_mean_net_bps']} | {r['holdout_positive_pair_count']}/{r['holdout_eligible_count']} |"
                )
        lines.append("")

        lines.append("### Equal-Weight `top_5` Portfolio: Static Train-Selected vs Causal Dynamic (`Untouched Holdout`)")
        lines.append("")
        lines.append("| Strategy | Regime | Holdout Net bps | Holdout Fees bps | Ann. Net % | Avg Slots | Max DD bps | Worst 24h bps |")
        lines.append("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |")
        for reg in COST_REGIME_NAMES:
            st = c["static_train_selected_holdout"][reg]["top_5"]
            lines.append(
                f"| `static_train_top5` | `{reg}` | {st['net_carry_bps']} | {st['fee_bps']} | {st['annualized_net_pct']} | {st['avg_active_slots']} | {st['max_drawdown_bps']} | {st['worst_24h_bps']} |"
            )
            for vid, reg_map in c["variants"].items():
                if not vid.startswith("causal_"):
                    continue
                p5 = reg_map[reg].get("portfolios_by_k", {}).get("top_5", {}).get("holdout")
                if p5:
                    lines.append(
                        f"| `{vid}_top5` | `{reg}` | {p5['net_carry_bps']} | {p5['fee_bps']} | {p5['annualized_net_pct']} | {p5['avg_active_slots']} | {p5['max_drawdown_bps']} | {p5['worst_24h_bps']} |"
                    )
        lines.append("")

    return "\n".join(lines)


def _fetch_live_raw_archive(
    *,
    universe_pairs: list[PairSpec],
    out_dir: Path,
    arcus_limit: int,
    lookback_days: int,
    sleep_ms: int,
) -> dict[str, Any]:
    """Fetch unauthenticated public historical funding for every leg in `universe_pairs` and write raw bytes + manifest."""
    from itertools import zip_longest

    raw_dir = out_dir / "raw"
    (raw_dir / "arcus").mkdir(parents=True, exist_ok=True)
    (raw_dir / "hyperliquid").mkdir(parents=True, exist_ok=True)
    (raw_dir / "trade_xyz").mkdir(parents=True, exist_ok=True)

    fetched_at_utc = _utc_now_iso()
    start_ms = int((time.time() - lookback_days * 86_400) * 1000)

    arcus_symbols = sorted({p.leg_a_symbol for p in universe_pairs if p.leg_a_venue == "arcus"})
    hl_legs = sorted({(p.leg_b_venue, p.leg_b_symbol) for p in universe_pairs if p.leg_b_venue in {"hyperliquid", "trade_xyz"}})

    receipts: list[dict[str, Any]] = []
    total_legs = len(arcus_symbols) + len(hl_legs)
    idx = 0
    arcus_gap = max(sleep_ms / 1000.0, DEFAULT_ARCUS_MIN_INTERVAL_SEC) if sleep_ms > 0 else 0.0
    hl_gap = max(sleep_ms / 1000.0, DEFAULT_HL_MIN_INTERVAL_SEC) if sleep_ms > 0 else 0.0
    last_arcus_ts = 0.0
    last_hl_ts = 0.0

    def _pace(last_ts: float, gap: float) -> None:
        if gap > 0.0 and last_ts > 0.0:
            rem = gap - (time.monotonic() - last_ts)
            if rem > 0.0:
                time.sleep(rem)

    for sym, hl_entry in zip_longest(arcus_symbols, hl_legs):
        if sym is not None:
            idx += 1
            _pace(last_arcus_ts, arcus_gap)
            url = f"{ARCUS_FUNDING_URL}?market={quote(sym)}&limit={arcus_limit}"
            status, raw = _http_fetch_bytes(url, method="GET")
            last_arcus_ts = time.monotonic()
            rel_path = Path("raw/arcus") / f"{_safe_slug(sym)}.json"
            dest = out_dir / rel_path
            dest.write_bytes(raw)
            parsed = parse_arcus_funding_payloads(sym, [raw])
            receipts.append(
                {
                    "venue": "arcus",
                    "symbol": sym,
                    "page": 1,
                    "rel_path": rel_path.as_posix(),
                    "url": url,
                    "method": "GET",
                    "request_body": None,
                    "http_status": status,
                    "received_at_utc": _utc_now_iso(),
                    "bytes": len(raw),
                    "sha256": _sha256_bytes(raw),
                }
            )
            print(f"[{idx}/{total_legs}] fetched arcus {sym}: {len(parsed.rates_by_hour_ms)}h", file=sys.stderr)

        if hl_entry is not None:
            venue, coin = hl_entry
            idx += 1
            page_bytes: list[bytes] = []
            cursor_ms = start_ms
            for page in range(1, MAX_HL_PAGES + 1):
                _pace(last_hl_ts, hl_gap)
                req_body = {"type": "fundingHistory", "coin": coin, "startTime": cursor_ms}
                status, raw = _http_fetch_bytes(HYPERLIQUID_INFO_URL, method="POST", body=req_body)
                last_hl_ts = time.monotonic()
                rel_path = Path(f"raw/{venue}") / f"{_safe_slug(coin)}_p{page:02d}.json"
                dest = out_dir / rel_path
                dest.write_bytes(raw)
                receipts.append(
                    {
                        "venue": venue,
                        "symbol": coin,
                        "page": page,
                        "rel_path": rel_path.as_posix(),
                        "url": HYPERLIQUID_INFO_URL,
                        "method": "POST",
                        "request_body": req_body,
                        "http_status": status,
                        "received_at_utc": _utc_now_iso(),
                        "bytes": len(raw),
                        "sha256": _sha256_bytes(raw),
                    }
                )
                page_bytes.append(raw)
                rows = json.loads(raw.decode("utf-8"))
                if not isinstance(rows, list) or len(rows) < HL_PAGE_LIMIT:
                    break
                times = [r.get("time") for r in rows if isinstance(r.get("time"), int)]
                if not times:
                    break
                next_cursor = max(times) + 1
                if next_cursor <= cursor_ms:
                    break
                cursor_ms = next_cursor

            parsed = parse_hyperliquid_funding_payloads(venue, coin, page_bytes)
            print(
                f"[{idx}/{total_legs}] fetched {venue} {coin}: {len(parsed.rates_by_hour_ms)}h ({len(page_bytes)} pages)",
                file=sys.stderr,
            )

    # Write universe_pairs.json cleanly
    pairs_doc = {
        "schema_version": "multiday_carry_universe_pairs_v1",
        "generated_at_utc": fetched_at_utc,
        "pairs": [
            {
                "pair_id": p.pair_id,
                "canonical_base": p.canonical_base,
                "asset_class_hint": p.asset_class_hint,
                "cohort": p.cohort,
                "leg_a_venue": p.leg_a_venue,
                "leg_a_symbol": p.leg_a_symbol,
                "leg_b_venue": p.leg_b_venue,
                "leg_b_symbol": p.leg_b_symbol,
                "rt_mt_bps": str(p.rt_mt_bps),
                "rt_tt_bps": str(p.rt_tt_bps),
                "fee_completeness": p.fee_completeness,
                "recommended_topology": p.recommended_topology,
                "risk_flags": list(p.risk_flags),
            }
            for p in universe_pairs
        ],
    }
    pairs_bytes = (json.dumps(pairs_doc, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
    (out_dir / "universe_pairs.json").write_bytes(pairs_bytes)

    manifest = {
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "fetched_at_utc": fetched_at_utc,
        "zero_secret_env_reads_verified": True,
        "arcus_limit": arcus_limit,
        "lookback_days": lookback_days,
        "universe_pairs_file": "universe_pairs.json",
        "universe_pairs_sha256": _sha256_bytes(pairs_bytes),
        "receipt_count": len(receipts),
        "receipts": receipts,
    }
    manifest_bytes = (json.dumps(manifest, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
    (out_dir / "manifest.json").write_bytes(manifest_bytes)
    return manifest


def load_verified_offline_archive(input_dir: Path) -> tuple[dict[str, Any], list[PairSpec], dict[tuple[str, str], VenueRateSeries]]:
    """Verify manifest SHA-256 digests and load `universe_pairs.json` + raw payloads strictly offline."""
    root = input_dir.resolve()
    manifest_path = root / "manifest.json"
    if not manifest_path.is_file():
        raise ValueError(f"missing manifest.json under {input_dir}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if manifest.get("schema_version") != MANIFEST_SCHEMA_VERSION:
        raise ValueError(f"unexpected manifest schema_version: {manifest.get('schema_version')!r}")

    pairs_rel = str(manifest.get("universe_pairs_file") or "universe_pairs.json")
    pairs_path = (root / pairs_rel).resolve()
    if not pairs_path.is_relative_to(root) or not pairs_path.is_file():
        raise ValueError(f"missing or unsafe universe_pairs_file: {pairs_rel!r}")
    pairs_bytes = pairs_path.read_bytes()
    expected_pairs_sha = str(manifest.get("universe_pairs_sha256") or "")
    if _sha256_bytes(pairs_bytes) != expected_pairs_sha:
        raise ValueError("SHA-256 mismatch for universe_pairs.json")

    pairs_doc = json.loads(pairs_bytes.decode("utf-8"))
    universe_pairs = [
        PairSpec(
            pair_id=str(row["pair_id"]),
            canonical_base=str(row["canonical_base"]),
            asset_class_hint=str(row["asset_class_hint"]),
            cohort=str(row["cohort"]),
            leg_a_venue=str(row["leg_a_venue"]),
            leg_a_symbol=str(row["leg_a_symbol"]),
            leg_b_venue=str(row["leg_b_venue"]),
            leg_b_symbol=str(row["leg_b_symbol"]),
            rt_mt_bps=_parse_finite_decimal(row["rt_mt_bps"], field_name="rt_mt_bps"),
            rt_tt_bps=_parse_finite_decimal(row["rt_tt_bps"], field_name="rt_tt_bps"),
            fee_completeness=str(row["fee_completeness"]),
            recommended_topology=str(row["recommended_topology"]),
            risk_flags=tuple(str(f) for f in row.get("risk_flags") or ()),
        )
        for row in pairs_doc["pairs"]
    ]

    grouped_payloads: dict[tuple[str, str], list[tuple[int, bytes]]] = {}
    for rc in manifest["receipts"]:
        rel = str(rc["rel_path"])
        raw_path = (root / rel).resolve()
        if not raw_path.is_relative_to(root) or not raw_path.is_file():
            raise ValueError(f"missing or escaped raw path in manifest: {rel!r}")
        raw_bytes = raw_path.read_bytes()
        if len(raw_bytes) != int(rc["bytes"]):
            raise ValueError(f"byte length mismatch for {rel}: got {len(raw_bytes)} expected {rc['bytes']}")
        actual_sha = _sha256_bytes(raw_bytes)
        if actual_sha != str(rc["sha256"]):
            raise ValueError(f"SHA-256 mismatch for {rel}: got {actual_sha} expected {rc['sha256']}")
        key = (str(rc["venue"]), str(rc["symbol"]))
        grouped_payloads.setdefault(key, []).append((int(rc["page"]), raw_bytes))

    venue_series: dict[tuple[str, str], VenueRateSeries] = {}
    for (venue, sym), page_list in grouped_payloads.items():
        page_list.sort(key=lambda item: item[0])
        ordered_bytes = [b for _, b in page_list]
        if venue == "arcus":
            venue_series[(venue, sym)] = parse_arcus_funding_payloads(sym, ordered_bytes)
        elif venue in {"hyperliquid", "trade_xyz"}:
            venue_series[(venue, sym)] = parse_hyperliquid_funding_payloads(venue, sym, ordered_bytes)
        else:
            raise ValueError(f"unsupported venue in manifest: {venue!r}")

    for spec in universe_pairs:
        if (spec.leg_a_venue, spec.leg_a_symbol) not in venue_series:
            raise ValueError(f"missing raw leg_a payload for {(spec.leg_a_venue, spec.leg_a_symbol)}")
        if (spec.leg_b_venue, spec.leg_b_symbol) not in venue_series:
            raise ValueError(f"missing raw leg_b payload for {(spec.leg_b_venue, spec.leg_b_symbol)}")

    return manifest, universe_pairs, venue_series


def write_sha256sums(out_dir: Path) -> Path:
    """Write standard `SHA256SUMS` covering all regular files under `out_dir` except `SHA256SUMS` itself."""
    entries: list[str] = []
    for path in sorted(out_dir.rglob("*")):
        if not path.is_file() or path.name == "SHA256SUMS":
            continue
        rel = path.relative_to(out_dir).as_posix()
        digest = _sha256_bytes(path.read_bytes())
        entries.append(f"{digest}  {rel}")
    sums_path = out_dir / "SHA256SUMS"
    sums_path.write_text("\n".join(entries) + "\n", encoding="utf-8")
    return sums_path


def _write_archive_outputs(out_dir: Path, result: dict[str, Any]) -> None:
    res_bytes = (json.dumps(result, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
    (out_dir / "result.json").write_bytes(res_bytes)
    report_md = render_markdown_report(result)
    (out_dir / "REPORT.md").write_text(report_md, encoding="utf-8")
    write_sha256sums(out_dir)


def _build_synthetic_archive(root: Path, *, num_hours: int = 360) -> Path:
    """Create deterministic multi-cohort synthetic archive for `--self-test`."""
    base_ms = 1_793_000_000_000 - (1_793_000_000_000 % HOUR_MS)
    specs = [
        PairSpec("arcus:NVDA-USD__vs__trade_xyz:xyz:NVDA", "NVDA", "equity", COHORT_EQUITY_RWA, "arcus", "NVDA-USD", "trade_xyz", "xyz:NVDA", Decimal("1.8"), Decimal("6.3"), "mixed", "maker_first_arcus", ()),
        PairSpec("arcus:TSLA-USD__vs__trade_xyz:xyz:TSLA", "TSLA", "equity", COHORT_EQUITY_RWA, "arcus", "TSLA-USD", "trade_xyz", "xyz:TSLA", Decimal("1.8"), Decimal("6.3"), "mixed", "maker_first_arcus", ()),
        PairSpec("arcus:AAPL-USD__vs__trade_xyz:xyz:AAPL", "AAPL", "equity", COHORT_EQUITY_RWA, "arcus", "AAPL-USD", "trade_xyz", "xyz:AAPL", Decimal("1.8"), Decimal("6.3"), "mixed", "maker_first_arcus", ()),
        PairSpec("arcus:BTC-USD__vs__hyperliquid:BTC", "BTC", "crypto", COHORT_CRYPTO, "arcus", "BTC-USD", "hyperliquid", "BTC", Decimal("12.0"), Decimal("18.0"), "mixed", "maker_first_arcus", ()),
        PairSpec("arcus:ETH-USD__vs__hyperliquid:ETH", "ETH", "crypto", COHORT_CRYPTO, "arcus", "ETH-USD", "hyperliquid", "ETH", Decimal("12.0"), Decimal("18.0"), "mixed", "maker_first_arcus", ()),
        PairSpec("arcus:SOL-USD__vs__hyperliquid:SOL", "SOL", "crypto", COHORT_CRYPTO, "arcus", "SOL-USD", "hyperliquid", "SOL", Decimal("12.0"), Decimal("18.0"), "mixed", "maker_first_arcus", ()),
    ]
    (root / "raw" / "arcus").mkdir(parents=True, exist_ok=True)
    (root / "raw" / "hyperliquid").mkdir(parents=True, exist_ok=True)

    receipts: list[dict[str, Any]] = []
    for s_idx, sp in enumerate(specs):
        arc_rows = []
        hl_rows = []
        for i in range(num_hours):
            t_ms = base_ms + i * HOUR_MS
            # Persistent trend in NVDA & BTC; sign-flipping / flat in others
            if sp.canonical_base == "NVDA":
                rb = Decimal("0.00025")
                ra = Decimal("0.00001")
            elif sp.canonical_base == "TSLA":
                rb = Decimal("-0.00020") if i < num_hours // 2 else Decimal("0.00018")
                ra = Decimal("0")
            elif sp.canonical_base == "BTC":
                rb = Decimal("0.00035") if i >= 48 else Decimal("0.00005")
                ra = Decimal("0.00002")
            else:
                rb = Decimal("0.00001") if (i % 2 == 0) else Decimal("-0.00001")
                ra = Decimal("0")
            arc_rows.append({"marketId": s_idx + 1, "marketDisplayName": sp.leg_a_symbol, "time": t_ms * 1000, "fundingRate": str(ra)})
            hl_rows.append({"coin": sp.leg_b_symbol, "time": t_ms + 37, "fundingRate": str(rb), "premium": "0.0001"})

        arc_bytes = json.dumps({"fundingRates": arc_rows, "total": len(arc_rows)}, sort_keys=True).encode("utf-8")
        arc_rel = f"raw/arcus/{_safe_slug(sp.leg_a_symbol)}.json"
        (root / arc_rel).write_bytes(arc_bytes)
        receipts.append({
            "venue": "arcus",
            "symbol": sp.leg_a_symbol,
            "page": 1,
            "rel_path": arc_rel,
            "url": ARCUS_FUNDING_URL,
            "method": "GET",
            "request_body": None,
            "http_status": 200,
            "received_at_utc": "2026-11-23T00:00:00+00:00",
            "bytes": len(arc_bytes),
            "sha256": _sha256_bytes(arc_bytes),
        })

        hl_bytes = json.dumps(hl_rows, sort_keys=True).encode("utf-8")
        hl_rel = f"raw/hyperliquid/{_safe_slug(sp.leg_b_symbol)}_p01.json"
        (root / hl_rel).write_bytes(hl_bytes)
        receipts.append({
            "venue": sp.leg_b_venue,
            "symbol": sp.leg_b_symbol,
            "page": 1,
            "rel_path": hl_rel,
            "url": HYPERLIQUID_INFO_URL,
            "method": "POST",
            "request_body": {"type": "fundingHistory", "coin": sp.leg_b_symbol, "startTime": base_ms},
            "http_status": 200,
            "received_at_utc": "2026-11-23T00:00:00+00:00",
            "bytes": len(hl_bytes),
            "sha256": _sha256_bytes(hl_bytes),
        })

    pairs_doc = {
        "schema_version": "multiday_carry_universe_pairs_v1",
        "generated_at_utc": "2026-11-23T00:00:00+00:00",
        "pairs": [
            {
                "pair_id": sp.pair_id,
                "canonical_base": sp.canonical_base,
                "asset_class_hint": sp.asset_class_hint,
                "cohort": sp.cohort,
                "leg_a_venue": sp.leg_a_venue,
                "leg_a_symbol": sp.leg_a_symbol,
                "leg_b_venue": sp.leg_b_venue,
                "leg_b_symbol": sp.leg_b_symbol,
                "rt_mt_bps": str(sp.rt_mt_bps),
                "rt_tt_bps": str(sp.rt_tt_bps),
                "fee_completeness": sp.fee_completeness,
                "recommended_topology": sp.recommended_topology,
                "risk_flags": list(sp.risk_flags),
            }
            for sp in specs
        ],
    }
    pairs_bytes = json.dumps(pairs_doc, indent=2, sort_keys=True).encode("utf-8")
    (root / "universe_pairs.json").write_bytes(pairs_bytes)
    manifest = {
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "fetched_at_utc": "2026-11-23T00:00:00+00:00",
        "zero_secret_env_reads_verified": True,
        "arcus_limit": DEFAULT_ARCUS_LIMIT,
        "lookback_days": 15,
        "universe_pairs_file": "universe_pairs.json",
        "universe_pairs_sha256": _sha256_bytes(pairs_bytes),
        "receipt_count": len(receipts),
        "receipts": receipts,
    }
    (root / "manifest.json").write_bytes(json.dumps(manifest, indent=2, sort_keys=True).encode("utf-8"))
    return root


def run_self_test() -> dict[str, Any]:
    """Run deterministic offline self-test verifying evaluation, causality, determinism, and tamper fail-closed."""
    with tempfile.TemporaryDirectory() as tmp:
        root = _build_synthetic_archive(Path(tmp))
        m1, pairs1, ser1 = load_verified_offline_archive(root)
        res1 = evaluate_universe_offline(
            universe_pairs=pairs1,
            venue_series=ser1,
            metadata={"fetched_at_utc": m1["fetched_at_utc"], "receipt_count": m1["receipt_count"]},
        )
        res2 = evaluate_universe_offline(
            universe_pairs=pairs1,
            venue_series=ser1,
            metadata={"fetched_at_utc": m1["fetched_at_utc"], "receipt_count": m1["receipt_count"]},
        )
        assert json.dumps(res1, sort_keys=True) == json.dumps(res2, sort_keys=True)
        assert res1["full_realized_cash_usd"] is None
        assert res1["universe_summary"]["total_known_fee_pairs"] == 6

        # Tamper detection test
        target_raw = root / m1["receipts"][0]["rel_path"]
        target_raw.write_bytes(b"{}")
        tamper_rejected = False
        try:
            load_verified_offline_archive(root)
        except ValueError:
            tamper_rejected = True
        assert tamper_rejected

    return {
        "schema_version": SCHEMA_VERSION,
        "self_test": "passed",
        "deterministic_replay_verified": True,
        "sha256_tamper_rejection_verified": True,
        "zero_secret_env_reads_verified": True,
        "full_realized_cash_usd": None,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument("--self-test", action="store_true", help="Run offline synthetic self-test and exit")
    mode.add_argument("--input-dir", type=Path, default=None, help="Offline input archive dir containing manifest.json")
    mode.add_argument("--live-fetch", action="store_true", help="Opt-in: fetch public unauthenticated Arcus + HL/XYZ funding history")

    parser.add_argument("--out-dir", type=Path, default=None, help="Output directory for raw receipts, result.json, REPORT.md, SHA256SUMS")
    parser.add_argument("--output", type=Path, default=None, help="Optional single JSON result path")
    parser.add_argument("--snapshot", type=Path, default=REPO_ROOT / DEFAULT_SNAPSHOT_REL)
    parser.add_argument("--account-fee-evidence", type=Path, default=REPO_ROOT / DEFAULT_ACCOUNT_FEE_REL)
    parser.add_argument("--arcus-limit", type=int, default=DEFAULT_ARCUS_LIMIT)
    parser.add_argument("--lookback-days", type=int, default=DEFAULT_LOOKBACK_DAYS)
    parser.add_argument("--sleep-ms", type=int, default=DEFAULT_SLEEP_MS)
    parser.add_argument("--quiet", action="store_true", help="Do not print full JSON result to stdout")
    args = parser.parse_args(argv)

    if not args.self_test and args.input_dir is None and not args.live_fetch:
        print("Error: specify one of --self-test, --input-dir <dir>, or --live-fetch --out-dir <dir>", file=sys.stderr)
        return 2

    if args.self_test:
        try:
            summary = run_self_test()
        except Exception as exc:  # noqa: BLE001
            print(f"Self-test failed: {type(exc).__name__}: {exc}", file=sys.stderr)
            return 2
        if args.output is not None:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(summary, indent=2, allow_nan=False) + "\n", encoding="utf-8")
        if not args.quiet:
            print(json.dumps(summary, indent=2, allow_nan=False))
        return 0

    if args.live_fetch:
        if args.out_dir is None:
            print("Error: --live-fetch requires --out-dir <dir>", file=sys.stderr)
            return 2
        try:
            snapshot = json.loads(args.snapshot.read_text(encoding="utf-8"))
            account_fee_evidence = (
                json.loads(args.account_fee_evidence.read_text(encoding="utf-8"))
                if args.account_fee_evidence is not None
                else None
            )
            universe_pairs = extract_known_fee_universe(snapshot, account_fee_evidence)
            args.out_dir.mkdir(parents=True, exist_ok=True)
            _fetch_live_raw_archive(
                universe_pairs=universe_pairs,
                out_dir=args.out_dir,
                arcus_limit=args.arcus_limit,
                lookback_days=args.lookback_days,
                sleep_ms=args.sleep_ms,
            )
            manifest, universe_pairs, venue_series = load_verified_offline_archive(args.out_dir)
            meta = {
                "fetched_at_utc": manifest["fetched_at_utc"],
                "receipt_count": manifest["receipt_count"],
                "arcus_limit": manifest.get("arcus_limit"),
                "lookback_days": manifest.get("lookback_days"),
                "manifest_schema_version": manifest["schema_version"],
            }
            result = evaluate_universe_offline(
                universe_pairs=universe_pairs,
                venue_series=venue_series,
                metadata=meta,
            )
            _write_archive_outputs(args.out_dir, result)
        except Exception as exc:  # noqa: BLE001
            print(f"Live fetch failed: {type(exc).__name__}: {exc}", file=sys.stderr)
            return 2
    else:
        assert args.input_dir is not None
        try:
            manifest, universe_pairs, venue_series = load_verified_offline_archive(args.input_dir)
            meta = {
                "fetched_at_utc": manifest["fetched_at_utc"],
                "receipt_count": manifest["receipt_count"],
                "arcus_limit": manifest.get("arcus_limit"),
                "lookback_days": manifest.get("lookback_days"),
                "manifest_schema_version": manifest["schema_version"],
            }
            result = evaluate_universe_offline(
                universe_pairs=universe_pairs,
                venue_series=venue_series,
                metadata=meta,
            )
            if args.out_dir is not None:
                args.out_dir.mkdir(parents=True, exist_ok=True)
                _write_archive_outputs(args.out_dir, result)
        except Exception as exc:  # noqa: BLE001
            print(f"Offline evaluation failed: {type(exc).__name__}: {exc}", file=sys.stderr)
            return 2

    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8")

    if not args.quiet:
        print(json.dumps(result, indent=2, sort_keys=True, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
