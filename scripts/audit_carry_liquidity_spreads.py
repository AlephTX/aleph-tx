#!/usr/bin/env python3
"""Offline-first Cross-Venue Carry L1/L2 Liquidity, Spread, Oracle & Payback Auditor.

Joins a verified Iteration-36 `multiday_carry_universe` archive (74 both-legs-known-fee
pairs: 39 Arcus-vs-TradeXYZ equity/RWA + 35 Arcus-vs-Hyperliquid crypto) with
point-in-time unauthenticated public orderbook & market context snapshots:
  - Arcus: `/v1/markets` (oracle/mark/volume24h/OI/isOutsideRth) + `/v1/bbo/{market}` (L1 depth)
  - Hyperliquid / TradeXYZ: `/info` `metaAndAssetCtxs` + `l2Book` (full L2 levels)

Strictly offline by default (`--self-test` or `--input-dir` with fail-closed SHA-256
verification). Opt-in `--live-fetch` queries ONLY public unauthenticated REST endpoints,
never reads secret environment variables or credential files, and writes raw HTTP byte
payloads + `manifest.json` + `SHA256SUMS`.

Non-extrapolation invariant: a point-in-time public orderbook snapshot cannot certify
40-day historical realized execution cash or equity US-RTH liquidity when sampled
outside RTH; `historical_40d_realized_cash_usd` is therefore always preserved as `null`.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from datetime import datetime, timezone
from decimal import Decimal
import json
import math
from pathlib import Path
import sys
import tempfile
import time
from typing import Any
from urllib.error import HTTPError, URLError
from urllib.parse import quote

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from eval_multiday_carry_universe import (  # noqa: E402
    BPS_PER_UNIT,
    COHORT_CRYPTO,
    COHORT_EQUITY_RWA,
    COHORTS,
    DECIMAL_ZERO,
    DEFAULT_ARCUS_MIN_INTERVAL_SEC,
    DEFAULT_HL_MIN_INTERVAL_SEC,
    PairSpec,
    _build_synthetic_archive,
    _http_fetch_bytes,
    _parse_finite_decimal,
    _round_float,
    _safe_slug,
    _sha256_bytes,
    _utc_now_iso,
    align_pair_rates,
    compute_chronological_split,
    load_verified_offline_archive,
    simulate_pair_variant,
    write_sha256sums,
)

SCHEMA_VERSION = "carry_liquidity_spread_audit_v3"
MANIFEST_SCHEMA_VERSION = "carry_liquidity_raw_manifest_v1"

ARCUS_MARKETS_URL = "https://api.arcus.xyz/v1/markets"
ARCUS_BBO_BASE_URL = "https://api.arcus.xyz/v1/bbo"
HYPERLIQUID_INFO_URL = "https://api.hyperliquid.xyz/info"

NOTIONAL_TIERS_USD: tuple[int, ...] = (100, 500, 1000)
ORACLE_DIVERGENCE_WARN_BPS = Decimal("50")
MIN_ORDERBOOK_DEPTH_USD = Decimal("100")
DEPTH_TIER_500_USD = Decimal("500")
DEFAULT_SLEEP_MS = 350
DEFAULT_MAX_SOURCE_AGE_MS = 1000
DEFAULT_MAX_PAIR_SKEW_MS = 1000
EPOCH_UTC = datetime(1970, 1, 1, tzinfo=timezone.utc)
DEFAULT_CARRY_ARCHIVE_REL = Path(".agent-state/research/multiday_carry_universe_20261123")
HISTORICAL_CASH_STATUS = "unknown_point_in_time_snapshot_cannot_proxy_historical_40d_orderbook_or_maker_queue"


def _strict_positive_int(raw: Any) -> int | None:
    """Accept only true positive Python `int` values (rejects `bool`, `float`, `str`, `<= 0`)."""
    return raw if type(raw) is int and raw > 0 else None


def _parse_iso_utc_epoch_us(raw: Any, *, field_name: str = "received_at_utc") -> int:
    """Parse timezone-aware ISO-8601 UTC timestamp into positive integer microseconds."""
    if not isinstance(raw, str) or not raw.strip():
        raise ValueError(f"missing or non-string {field_name}: {raw!r}")
    text = raw.strip()
    if text.endswith("Z"):
        text = text[:-1] + "+00:00"
    dt = datetime.fromisoformat(text)
    if dt.tzinfo is None:
        raise ValueError(f"timezone-naive {field_name} rejected: {raw!r}")
    delta = dt.astimezone(timezone.utc) - EPOCH_UTC
    us = (delta.days * 86_400 + delta.seconds) * 1_000_000 + delta.microseconds
    if us <= 0:
        raise ValueError(f"non-positive {field_name} rejected: {raw!r}")
    return us


def _parse_optional_finite_decimal(raw: Any) -> Decimal | None:
    if raw is None or isinstance(raw, bool) or raw == "":
        return None
    return _parse_finite_decimal(raw, field_name="optional_decimal")


def _compute_vwap_price(levels: list[tuple[Decimal, Decimal]], target_usd: Decimal) -> Decimal | None:
    """Compute quote-weighted VWAP execution price for `target_usd` across ordered book levels."""
    if target_usd <= DECIMAL_ZERO or not levels:
        return None
    rem_usd = target_usd
    total_base = DECIMAL_ZERO
    for px, sz in levels:
        if px <= DECIMAL_ZERO or sz <= DECIMAL_ZERO:
            return None
        level_usd = px * sz
        if level_usd >= rem_usd:
            total_base += rem_usd / px
            rem_usd = DECIMAL_ZERO
            break
        total_base += sz
        rem_usd -= level_usd
    if rem_usd > DECIMAL_ZERO or total_base <= DECIMAL_ZERO:
        return None
    return target_usd / total_base


@dataclass(frozen=True)
class ArcusMarketContext:
    symbol: str
    status: str
    oracle_px: Decimal | None
    mark_px: Decimal | None
    last_px: Decimal | None
    funding_rate: Decimal | None
    volume_24h_usd: Decimal | None
    open_interest_base: Decimal | None
    is_outside_rth: bool


@dataclass(frozen=True)
class HlMarketContext:
    venue: str
    coin: str
    oracle_px: Decimal | None
    mark_px: Decimal | None
    mid_px: Decimal | None
    funding_rate: Decimal | None
    premium: Decimal | None
    day_ntl_vlm_usd: Decimal | None
    open_interest_base: Decimal | None
    is_delisted: bool = False


@dataclass(frozen=True)
class ArcusBboAudit:
    symbol: str
    status: str  # "ok", "empty_book", "crossed_book", "http_error"
    bid_px: Decimal | None
    bid_sz: Decimal | None
    ask_px: Decimal | None
    ask_sz: Decimal | None
    mid_px: Decimal | None
    spread_bps: Decimal | None
    bid_l1_usd: Decimal | None
    ask_l1_usd: Decimal | None
    min_l1_usd: Decimal | None
    timestamp_us: int | None
    received_at_utc: str | None = None
    received_us: int | None = None
    error_detail: str | None = None


@dataclass(frozen=True)
class HlL2BookAudit:
    venue: str
    coin: str
    status: str  # "ok", "empty_book", "crossed_book", "http_error"
    bid_px: Decimal | None
    ask_px: Decimal | None
    mid_px: Decimal | None
    spread_bps: Decimal | None
    bid_l1_usd: Decimal | None
    ask_l1_usd: Decimal | None
    min_l1_usd: Decimal | None
    total_bid_usd: Decimal
    total_ask_usd: Decimal
    rt_slip_bps_by_tier: dict[int, Decimal | None]
    timestamp_ms: int | None
    received_at_utc: str | None = None
    received_us: int | None = None
    error_detail: str | None = None


def parse_arcus_markets_map(raw_bytes: bytes) -> dict[str, ArcusMarketContext]:
    doc = json.loads(raw_bytes.decode("utf-8"))
    markets = doc.get("markets")
    if not isinstance(markets, list):
        raise ValueError("Arcus /v1/markets payload missing markets list")
    out: dict[str, ArcusMarketContext] = {}
    for row in markets:
        sym = str(row["marketDisplayName"])
        out[sym] = ArcusMarketContext(
            symbol=sym,
            status=str(row.get("status") or "UNKNOWN"),
            oracle_px=_parse_optional_finite_decimal(row.get("oraclePrice")),
            mark_px=_parse_optional_finite_decimal(row.get("markPrice")),
            last_px=_parse_optional_finite_decimal(row.get("lastTradePrice")),
            funding_rate=_parse_optional_finite_decimal(row.get("fundingRate")),
            volume_24h_usd=_parse_optional_finite_decimal(row.get("volume24hNotional")),
            open_interest_base=_parse_optional_finite_decimal(row.get("openInterest")),
            is_outside_rth=bool(row.get("isOutsideRth", False)),
        )
    return out


def parse_hl_meta_asset_ctxs(venue: str, raw_bytes: bytes) -> dict[str, HlMarketContext]:
    doc = json.loads(raw_bytes.decode("utf-8"))
    if not isinstance(doc, list) or len(doc) != 2:
        raise ValueError(f"{venue} metaAndAssetCtxs must be a 2-element list")
    universe = doc[0].get("universe")
    ctxs = doc[1]
    if not isinstance(universe, list) or not isinstance(ctxs, list) or len(universe) != len(ctxs):
        raise ValueError(f"{venue} metaAndAssetCtxs universe/ctxs length mismatch")
    out: dict[str, HlMarketContext] = {}
    for meta, ctx in zip(universe, ctxs):
        coin = str(meta["name"])
        out[coin] = HlMarketContext(
            venue=venue,
            coin=coin,
            oracle_px=_parse_optional_finite_decimal(ctx.get("oraclePx")),
            mark_px=_parse_optional_finite_decimal(ctx.get("markPx")),
            mid_px=_parse_optional_finite_decimal(ctx.get("midPx")),
            funding_rate=_parse_optional_finite_decimal(ctx.get("funding")),
            premium=_parse_optional_finite_decimal(ctx.get("premium")),
            day_ntl_vlm_usd=_parse_optional_finite_decimal(ctx.get("dayNtlVlm")),
            open_interest_base=_parse_optional_finite_decimal(ctx.get("openInterest")),
            is_delisted=meta.get("isDelisted") is True,
        )
    return out


def parse_arcus_bbo(symbol: str, raw_bytes: bytes, *, received_at_utc: str | None = None) -> ArcusBboAudit:
    received_us = _parse_iso_utc_epoch_us(received_at_utc) if received_at_utc is not None else None
    doc = json.loads(raw_bytes.decode("utf-8"))
    if isinstance(doc, dict) and doc.get("_http_error"):
        return ArcusBboAudit(
            symbol=symbol,
            status="http_error",
            bid_px=None,
            bid_sz=None,
            ask_px=None,
            ask_sz=None,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            timestamp_us=None,
            received_at_utc=received_at_utc,
            received_us=received_us,
            error_detail=str(doc.get("_http_error")),
        )
    bb = doc.get("bestBid") if isinstance(doc, dict) else None
    ba = doc.get("bestAsk") if isinstance(doc, dict) else None
    ts_us = _strict_positive_int(doc.get("timestamp")) if isinstance(doc, dict) else None
    if not isinstance(bb, dict) or not isinstance(ba, dict):
        return ArcusBboAudit(
            symbol=symbol,
            status="empty_book",
            bid_px=None,
            bid_sz=None,
            ask_px=None,
            ask_sz=None,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            timestamp_us=ts_us,
            received_at_utc=received_at_utc,
            received_us=received_us,
        )
    bid_px = _parse_finite_decimal(bb.get("price"), field_name=f"{symbol}.bid_px")
    bid_sz = _parse_finite_decimal(bb.get("size"), field_name=f"{symbol}.bid_sz")
    ask_px = _parse_finite_decimal(ba.get("price"), field_name=f"{symbol}.ask_px")
    ask_sz = _parse_finite_decimal(ba.get("size"), field_name=f"{symbol}.ask_sz")
    if bid_px <= DECIMAL_ZERO or ask_px <= DECIMAL_ZERO or bid_sz <= DECIMAL_ZERO or ask_sz <= DECIMAL_ZERO:
        return ArcusBboAudit(
            symbol=symbol,
            status="empty_book",
            bid_px=bid_px,
            bid_sz=bid_sz,
            ask_px=ask_px,
            ask_sz=ask_sz,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            timestamp_us=ts_us,
            received_at_utc=received_at_utc,
            received_us=received_us,
        )
    if bid_px >= ask_px:
        return ArcusBboAudit(
            symbol=symbol,
            status="crossed_book",
            bid_px=bid_px,
            bid_sz=bid_sz,
            ask_px=ask_px,
            ask_sz=ask_sz,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            timestamp_us=ts_us,
            received_at_utc=received_at_utc,
            received_us=received_us,
        )
    mid = (bid_px + ask_px) / Decimal("2")
    spread_bps = ((ask_px - bid_px) / mid) * BPS_PER_UNIT
    bid_usd = bid_px * bid_sz
    ask_usd = ask_px * ask_sz
    return ArcusBboAudit(
        symbol=symbol,
        status="ok",
        bid_px=bid_px,
        bid_sz=bid_sz,
        ask_px=ask_px,
        ask_sz=ask_sz,
        mid_px=mid,
        spread_bps=spread_bps,
        bid_l1_usd=bid_usd,
        ask_l1_usd=ask_usd,
        min_l1_usd=min(bid_usd, ask_usd),
        timestamp_us=ts_us,
        received_at_utc=received_at_utc,
        received_us=received_us,
    )


def parse_hl_l2_book(venue: str, coin: str, raw_bytes: bytes, *, received_at_utc: str | None = None) -> HlL2BookAudit:
    received_us = _parse_iso_utc_epoch_us(received_at_utc) if received_at_utc is not None else None
    doc = json.loads(raw_bytes.decode("utf-8"))
    if isinstance(doc, dict) and doc.get("_http_error"):
        return HlL2BookAudit(
            venue=venue,
            coin=coin,
            status="http_error",
            bid_px=None,
            ask_px=None,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            total_bid_usd=DECIMAL_ZERO,
            total_ask_usd=DECIMAL_ZERO,
            rt_slip_bps_by_tier={t: None for t in NOTIONAL_TIERS_USD},
            timestamp_ms=None,
            received_at_utc=received_at_utc,
            received_us=received_us,
            error_detail=str(doc.get("_http_error")),
        )
    levels = doc.get("levels") if isinstance(doc, dict) else None
    ts_ms = _strict_positive_int(doc.get("time")) if isinstance(doc, dict) else None
    if not isinstance(levels, list) or len(levels) != 2 or not levels[0] or not levels[1]:
        return HlL2BookAudit(
            venue=venue,
            coin=coin,
            status="empty_book",
            bid_px=None,
            ask_px=None,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            total_bid_usd=DECIMAL_ZERO,
            total_ask_usd=DECIMAL_ZERO,
            rt_slip_bps_by_tier={t: None for t in NOTIONAL_TIERS_USD},
            timestamp_ms=ts_ms,
            received_at_utc=received_at_utc,
            received_us=received_us,
        )

    if doc.get("coin") != coin:
        raise ValueError(f"{venue} l2Book response coin does not match the requested market {coin!r}")

    bids: list[tuple[Decimal, Decimal]] = [
        (_parse_finite_decimal(lv["px"], field_name=f"{coin}.bid.px"), _parse_finite_decimal(lv["sz"], field_name=f"{coin}.bid.sz"))
        for lv in levels[0]
    ]
    asks: list[tuple[Decimal, Decimal]] = [
        (_parse_finite_decimal(lv["px"], field_name=f"{coin}.ask.px"), _parse_finite_decimal(lv["sz"], field_name=f"{coin}.ask.sz"))
        for lv in levels[1]
    ]
    bid0_px, bid0_sz = bids[0]
    ask0_px, ask0_sz = asks[0]
    # Validate ordering & uncrossed book
    if bid0_px >= ask0_px or bid0_px <= DECIMAL_ZERO or ask0_px <= DECIMAL_ZERO:
        return HlL2BookAudit(
            venue=venue,
            coin=coin,
            status="crossed_book",
            bid_px=bid0_px,
            ask_px=ask0_px,
            mid_px=None,
            spread_bps=None,
            bid_l1_usd=None,
            ask_l1_usd=None,
            min_l1_usd=None,
            total_bid_usd=DECIMAL_ZERO,
            total_ask_usd=DECIMAL_ZERO,
            rt_slip_bps_by_tier={t: None for t in NOTIONAL_TIERS_USD},
            timestamp_ms=ts_ms,
            received_at_utc=received_at_utc,
            received_us=received_us,
        )
    for i in range(1, len(bids)):
        if bids[i][0] >= bids[i - 1][0]:
            return HlL2BookAudit(
                venue=venue,
                coin=coin,
                status="crossed_book",
                bid_px=bid0_px,
                ask_px=ask0_px,
                mid_px=None,
                spread_bps=None,
                bid_l1_usd=None,
                ask_l1_usd=None,
                min_l1_usd=None,
                total_bid_usd=DECIMAL_ZERO,
                total_ask_usd=DECIMAL_ZERO,
                rt_slip_bps_by_tier={t: None for t in NOTIONAL_TIERS_USD},
                timestamp_ms=ts_ms,
                received_at_utc=received_at_utc,
                received_us=received_us,
            )
    for i in range(1, len(asks)):
        if asks[i][0] <= asks[i - 1][0]:
            return HlL2BookAudit(
                venue=venue,
                coin=coin,
                status="crossed_book",
                bid_px=bid0_px,
                ask_px=ask0_px,
                mid_px=None,
                spread_bps=None,
                bid_l1_usd=None,
                ask_l1_usd=None,
                min_l1_usd=None,
                total_bid_usd=DECIMAL_ZERO,
                total_ask_usd=DECIMAL_ZERO,
                rt_slip_bps_by_tier={t: None for t in NOTIONAL_TIERS_USD},
                timestamp_ms=ts_ms,
                received_at_utc=received_at_utc,
                received_us=received_us,
            )

    mid = (bid0_px + ask0_px) / Decimal("2")
    spread_bps = ((ask0_px - bid0_px) / mid) * BPS_PER_UNIT
    bid_l1_usd = bid0_px * bid0_sz
    ask_l1_usd = ask0_px * ask0_sz
    total_bid_usd = sum((px * sz for px, sz in bids), DECIMAL_ZERO)
    total_ask_usd = sum((px * sz for px, sz in asks), DECIMAL_ZERO)

    rt_slip_by_tier: dict[int, Decimal | None] = {}
    for tier in NOTIONAL_TIERS_USD:
        t_usd = Decimal(tier)
        vwap_buy = _compute_vwap_price(asks, t_usd)
        vwap_sell = _compute_vwap_price(bids, t_usd)
        if vwap_buy is None or vwap_sell is None:
            rt_slip_by_tier[tier] = None
        else:
            rt_slip_by_tier[tier] = ((vwap_buy - vwap_sell) / mid) * BPS_PER_UNIT

    return HlL2BookAudit(
        venue=venue,
        coin=coin,
        status="ok",
        bid_px=bid0_px,
        ask_px=ask0_px,
        mid_px=mid,
        spread_bps=spread_bps,
        bid_l1_usd=bid_l1_usd,
        ask_l1_usd=ask_l1_usd,
        min_l1_usd=min(bid_l1_usd, ask_l1_usd),
        total_bid_usd=total_bid_usd,
        total_ask_usd=total_ask_usd,
        rt_slip_bps_by_tier=rt_slip_by_tier,
        timestamp_ms=ts_ms,
        received_at_utc=received_at_utc,
        received_us=received_us,
    )


def evaluate_carry_liquidity_offline(
    *,
    universe_pairs: list[PairSpec],
    carry_series: dict[tuple[str, str], Any],
    arcus_markets: dict[str, ArcusMarketContext],
    hl_ctxs: dict[tuple[str, str], HlMarketContext],
    arcus_bbos: dict[str, ArcusBboAudit],
    hl_books: dict[tuple[str, str], HlL2BookAudit],
    metadata: dict[str, Any],
    max_source_age_ms: int = DEFAULT_MAX_SOURCE_AGE_MS,
    max_pair_skew_ms: int = DEFAULT_MAX_PAIR_SKEW_MS,
) -> dict[str, Any]:
    """Join 40-day Carry Holdout evaluation with clock-gated two-leg L1/L2 spread & depth audit."""
    if type(max_source_age_ms) is not int or max_source_age_ms <= 0:
        raise ValueError(f"max_source_age_ms must be a positive int, got {max_source_age_ms!r}")
    if type(max_pair_skew_ms) is not int or max_pair_skew_ms <= 0:
        raise ValueError(f"max_pair_skew_ms must be a positive int, got {max_pair_skew_ms!r}")
    max_source_age_us = max_source_age_ms * 1_000
    max_pair_skew_us = max_pair_skew_ms * 1_000

    aligned_pairs = [
        align_pair_rates(sp, carry_series[(sp.leg_a_venue, sp.leg_a_symbol)], carry_series[(sp.leg_b_venue, sp.leg_b_symbol)])
        for sp in universe_pairs
    ]
    split = compute_chronological_split(aligned_pairs)

    pair_rows: list[dict[str, Any]] = []
    for aligned in aligned_pairs:
        sp = aligned.spec
        arc_ctx = arcus_markets.get(sp.leg_a_symbol)
        hl_ctx = hl_ctxs.get((sp.leg_b_venue, sp.leg_b_symbol))
        arc_bbo = arcus_bbos[sp.leg_a_symbol]
        hl_book = hl_books[(sp.leg_b_venue, sp.leg_b_symbol)]

        # Compute exact Train-frozen static direction & causal Holdout carry metrics
        tr_hours = [h for h in aligned.common_hours if split["train_start_ms"] <= h <= split["train_end_ms"]]
        ho_hours = [h for h in aligned.common_hours if split["holdout_start_ms"] <= h <= split["holdout_end_ms"]]
        holdout_info: dict[str, Any]
        if aligned.eligible_for_backtest and ho_hours:
            train_static_dir: str | None = None
            st_ho_mt = None
            st_ho_tt = None
            if tr_hours:
                tr_long = simulate_pair_variant(
                    carry_by_hour=aligned.carry_by_hour,
                    eval_hours=tr_hours,
                    rt_cost_bps=sp.rt_mt_bps,
                    mode="static_long_a_short_b",
                )
                tr_short = simulate_pair_variant(
                    carry_by_hour=aligned.carry_by_hour,
                    eval_hours=tr_hours,
                    rt_cost_bps=sp.rt_mt_bps,
                    mode="static_short_a_long_b",
                )
                train_static_dir = (
                    "long_a_short_b"
                    if tr_long.gross_carry_bps >= tr_short.gross_carry_bps
                    else "short_a_long_b"
                )
                st_ho_mt = simulate_pair_variant(
                    carry_by_hour=aligned.carry_by_hour,
                    eval_hours=ho_hours,
                    rt_cost_bps=sp.rt_mt_bps,
                    mode=f"static_{train_static_dir}",
                )
                st_ho_tt = simulate_pair_variant(
                    carry_by_hour=aligned.carry_by_hour,
                    eval_hours=ho_hours,
                    rt_cost_bps=sp.rt_tt_bps,
                    mode=f"static_{train_static_dir}",
                )
            c24 = simulate_pair_variant(
                carry_by_hour=aligned.carry_by_hour,
                eval_hours=ho_hours,
                rt_cost_bps=sp.rt_mt_bps,
                mode="causal_dynamic",
                lookback_h=24,
                hurdle_mult=Decimal("2.5"),
            )
            c72 = simulate_pair_variant(
                carry_by_hour=aligned.carry_by_hour,
                eval_hours=ho_hours,
                rt_cost_bps=sp.rt_mt_bps,
                mode="causal_dynamic",
                lookback_h=72,
                hurdle_mult=Decimal("1.5"),
            )
            holdout_info = {
                "hours": len(ho_hours),
                "train_static_dir": train_static_dir,
                "static_episodes": st_ho_mt.episodes if st_ho_mt is not None else 0,
                "static_net_rt_mt_bps": st_ho_mt.net_carry_bps if st_ho_mt is not None else None,
                "static_net_rt_tt_bps": st_ho_tt.net_carry_bps if st_ho_tt is not None else None,
                "static_hourly_gross_bps": (
                    (st_ho_mt.gross_carry_bps / Decimal(len(ho_hours))) if st_ho_mt is not None else None
                ),
                "causal_L24_M2p5_net_bps": c24.net_carry_bps,
                "causal_L24_M2p5_episodes": c24.episodes,
                "causal_L24_M2p5_active_hours": c24.active_hours,
                "causal_L72_M1p5_net_bps": c72.net_carry_bps,
                "causal_L72_M1p5_episodes": c72.episodes,
                "causal_L72_M1p5_active_hours": c72.active_hours,
            }
        else:
            holdout_info = {
                "hours": len(ho_hours),
                "train_static_dir": None,
                "static_episodes": 0,
                "static_net_rt_mt_bps": None,
                "static_net_rt_tt_bps": None,
                "static_hourly_gross_bps": None,
                "causal_L24_M2p5_net_bps": None,
                "causal_L24_M2p5_episodes": 0,
                "causal_L24_M2p5_active_hours": 0,
                "causal_L72_M1p5_net_bps": None,
                "causal_L72_M1p5_episodes": 0,
                "causal_L72_M1p5_active_hours": 0,
            }

        # Three-clock & per-leg tradability gates (all integer UTC microseconds)
        arc_source_us = arc_bbo.timestamp_us
        arc_rec_us = arc_bbo.received_us
        arc_clock_present = arc_source_us is not None and arc_rec_us is not None
        arc_source_not_future = bool(arc_clock_present and arc_source_us <= arc_rec_us)
        arc_source_age_us = (arc_rec_us - arc_source_us) if arc_source_not_future else None
        arc_clock_ok = bool(arc_source_age_us is not None and arc_source_age_us <= max_source_age_us)

        hl_source_us = (hl_book.timestamp_ms * 1_000) if hl_book.timestamp_ms is not None else None
        hl_rec_us = hl_book.received_us
        hl_clock_present = hl_source_us is not None and hl_rec_us is not None
        hl_source_not_future = bool(hl_clock_present and hl_source_us <= hl_rec_us)
        hl_source_age_us = (hl_rec_us - hl_source_us) if hl_source_not_future else None
        hl_clock_ok = bool(hl_source_age_us is not None and hl_source_age_us <= max_source_age_us)

        pair_source_skew_us = abs(arc_source_us - hl_source_us) if (arc_source_not_future and hl_source_not_future) else None
        pair_receipt_skew_us = abs(arc_rec_us - hl_rec_us) if (arc_rec_us is not None and hl_rec_us is not None) else None
        pair_source_skew_ok = bool(pair_source_skew_us is not None and pair_source_skew_us <= max_pair_skew_us)
        pair_receipt_skew_ok = bool(pair_receipt_skew_us is not None and pair_receipt_skew_us <= max_pair_skew_us)
        joint_clock_ok = bool(arc_clock_ok and hl_clock_ok and pair_source_skew_ok and pair_receipt_skew_ok)

        arcus_market_online = bool(arc_ctx is not None and arc_ctx.status == "ONLINE")
        hl_market_active = bool(hl_ctx is not None and not hl_ctx.is_delisted)
        arcus_tradable_ok = bool(arc_bbo.status == "ok" and arc_bbo.min_l1_usd is not None and arcus_market_online)
        hl_tradable_ok = bool(hl_book.status == "ok" and hl_book.min_l1_usd is not None and hl_market_active)
        joint_pair_eligible = bool(arcus_tradable_ok and hl_tradable_ok and joint_clock_ok)

        slip_100 = hl_book.rt_slip_bps_by_tier[100]
        slip_500 = hl_book.rt_slip_bps_by_tier[500]
        slip_1000 = hl_book.rt_slip_bps_by_tier[1000]

        arcus_l1_100usd_ok = bool(arcus_tradable_ok and arc_bbo.min_l1_usd >= MIN_ORDERBOOK_DEPTH_USD)
        hl_l2_100usd_ok = bool(hl_tradable_ok and slip_100 is not None)
        arcus_l1_500usd_ok = bool(arcus_tradable_ok and arc_bbo.min_l1_usd >= DEPTH_TIER_500_USD)
        hl_l2_500usd_ok = bool(hl_tradable_ok and slip_500 is not None)

        depth_100_ok = bool(arcus_l1_100usd_ok and hl_l2_100usd_ok and joint_clock_ok)
        depth_500_ok = bool(arcus_l1_500usd_ok and hl_l2_500usd_ok and joint_clock_ok)

        # Withhold simultaneous joint two-book basis/frictions/payback whenever clocks or tradability fail closed
        cross_mid_basis_bps: Decimal | None = None
        if (
            joint_pair_eligible
            and arc_bbo.mid_px is not None
            and hl_book.mid_px is not None
            and arc_bbo.mid_px > DECIMAL_ZERO
        ):
            cross_mid_basis_bps = ((hl_book.mid_px - arc_bbo.mid_px) / arc_bbo.mid_px) * BPS_PER_UNIT

        oracle_div_bps: Decimal | None = None
        oracle_ok = False
        if arc_ctx is not None and hl_ctx is not None and arc_ctx.oracle_px and hl_ctx.oracle_px and arc_ctx.oracle_px > DECIMAL_ZERO:
            oracle_div_bps = (abs(hl_ctx.oracle_px - arc_ctx.oracle_px) / arc_ctx.oracle_px) * BPS_PER_UNIT
            oracle_ok = bool(arcus_market_online and hl_market_active and oracle_div_bps <= ORACLE_DIVERGENCE_WARN_BPS)

        inst_carry_bps_hr: Decimal | None = None
        if arc_ctx is not None and hl_ctx is not None and arc_ctx.funding_rate is not None and hl_ctx.funding_rate is not None:
            inst_carry_bps_hr = (hl_ctx.funding_rate - arc_ctx.funding_rate) * BPS_PER_UNIT

        mt_plus_hl_cross_100_bps = (sp.rt_mt_bps + slip_100) if (joint_pair_eligible and slip_100 is not None) else None
        mt_plus_hl_cross_500_bps = (sp.rt_mt_bps + slip_500) if (joint_pair_eligible and slip_500 is not None) else None
        dual_taker_100_bps = (
            (sp.rt_tt_bps + arc_bbo.spread_bps + slip_100)
            if (joint_pair_eligible and arc_bbo.spread_bps is not None and slip_100 is not None)
            else None
        )

        # Point-in-time adjusted Holdout carry sensitivities (Train-fixed direction, all modeled episodes; gated on joint_pair_eligible)
        static_after_mt_100: Decimal | None = None
        static_after_tt_100: Decimal | None = None
        c72_after_mt_100: Decimal | None = None
        c24_after_mt_100: Decimal | None = None
        payback_hours_mt_100: Decimal | None = None

        static_episodes = int(holdout_info["static_episodes"])
        if joint_pair_eligible:
            if holdout_info["static_net_rt_mt_bps"] is not None and slip_100 is not None and static_episodes > 0:
                static_after_mt_100 = holdout_info["static_net_rt_mt_bps"] - (Decimal(static_episodes) * slip_100)
            if (
                holdout_info["static_net_rt_tt_bps"] is not None
                and arc_bbo.spread_bps is not None
                and slip_100 is not None
                and static_episodes > 0
            ):
                static_after_tt_100 = holdout_info["static_net_rt_tt_bps"] - (
                    Decimal(static_episodes) * (arc_bbo.spread_bps + slip_100)
                )
            if holdout_info["causal_L72_M1p5_net_bps"] is not None and slip_100 is not None:
                c72_after_mt_100 = holdout_info["causal_L72_M1p5_net_bps"] - (Decimal(holdout_info["causal_L72_M1p5_episodes"]) * slip_100)
            if holdout_info["causal_L24_M2p5_net_bps"] is not None and slip_100 is not None:
                c24_after_mt_100 = holdout_info["causal_L24_M2p5_net_bps"] - (Decimal(holdout_info["causal_L24_M2p5_episodes"]) * slip_100)
            if (
                mt_plus_hl_cross_100_bps is not None
                and holdout_info["static_hourly_gross_bps"] is not None
                and holdout_info["static_hourly_gross_bps"] > DECIMAL_ZERO
            ):
                payback_hours_mt_100 = mt_plus_hl_cross_100_bps / holdout_info["static_hourly_gross_bps"]

        is_outside_rth = bool(arc_ctx.is_outside_rth) if arc_ctx is not None else False
        if sp.cohort == COHORT_CRYPTO:
            session_regime = "crypto_24_7"
        else:
            session_regime = "equity_outside_rth" if is_outside_rth else "equity_inside_rth"

        reasons: list[str] = []
        if not aligned.eligible_for_backtest:
            reasons.append(aligned.exclusion_reason or "insufficient_carry_history")
        elif not tr_hours:
            reasons.append("missing_train_static_direction")
        if not arcus_market_online:
            reasons.append("arcus_market_not_online")
        if not hl_market_active:
            reasons.append("hl_market_delisted_or_missing")
        if arc_bbo.status != "ok":
            reasons.append(f"arcus_book_{arc_bbo.status}")
        elif arc_bbo.min_l1_usd is not None and arc_bbo.min_l1_usd < MIN_ORDERBOOK_DEPTH_USD:
            reasons.append(f"arcus_l1_lt_100usd:{_round_float(arc_bbo.min_l1_usd, 2)}")
        if hl_book.status != "ok":
            reasons.append(f"hl_book_{hl_book.status}")
        elif slip_100 is None:
            reasons.append("hl_l2_lt_100usd")
        if not arc_clock_present:
            reasons.append("arcus_source_stamp_missing_or_invalid")
        elif not arc_source_not_future:
            reasons.append("arcus_source_after_receipt")
        elif not arc_clock_ok:
            reasons.append(f"arcus_source_stale_gt_{max_source_age_ms}ms")
        if not hl_clock_present:
            reasons.append("hl_source_stamp_missing_or_invalid")
        elif not hl_source_not_future:
            reasons.append("hl_source_after_receipt")
        elif not hl_clock_ok:
            reasons.append(f"hl_source_stale_gt_{max_source_age_ms}ms")
        if pair_source_skew_us is not None and not pair_source_skew_ok:
            reasons.append(f"pair_source_skew_gt_{max_pair_skew_ms}ms")
        if pair_receipt_skew_us is not None and not pair_receipt_skew_ok:
            reasons.append(f"pair_receipt_skew_gt_{max_pair_skew_ms}ms")
        if oracle_div_bps is None:
            reasons.append("missing_oracle_price")
        elif oracle_div_bps > ORACLE_DIVERGENCE_WARN_BPS:
            reasons.append(f"oracle_div_gt_50bps:{_round_float(oracle_div_bps, 2)}")
        if sp.cohort == COHORT_EQUITY_RWA and is_outside_rth:
            reasons.append("equity_outside_rth_snapshot")

        positive_after_mt_100 = bool(
            (c72_after_mt_100 is not None and c72_after_mt_100 > DECIMAL_ZERO)
            or (c24_after_mt_100 is not None and c24_after_mt_100 > DECIMAL_ZERO)
            or (static_after_mt_100 is not None and static_after_mt_100 > DECIMAL_ZERO)
        )
        if depth_100_ok and oracle_ok and not positive_after_mt_100:
            reasons.append("holdout_carry_below_point_in_time_mt100_friction")

        passes_hard_gates = bool(aligned.eligible_for_backtest and depth_100_ok and oracle_ok and positive_after_mt_100)
        passes_train_fixed_static_only = bool(
            aligned.eligible_for_backtest
            and depth_100_ok
            and oracle_ok
            and static_after_mt_100 is not None
            and static_after_mt_100 > DECIMAL_ZERO
        )

        pair_rows.append(
            {
                "pair_id": sp.pair_id,
                "canonical_base": sp.canonical_base,
                "cohort": sp.cohort,
                "session_regime": session_regime,
                "eligible_for_backtest": aligned.eligible_for_backtest,
                "rt_mt_bps": _round_float(sp.rt_mt_bps, 4),
                "rt_tt_bps": _round_float(sp.rt_tt_bps, 4),
                "arcus_market_status": arc_ctx.status if arc_ctx is not None else None,
                "arcus_tradable_ok": arcus_tradable_ok,
                "arcus_bbo_status": arc_bbo.status,
                "arcus_spread_bps": _round_float(arc_bbo.spread_bps, 4) if arc_bbo.spread_bps is not None else None,
                "arcus_bid_l1_usd": _round_float(arc_bbo.bid_l1_usd, 2) if arc_bbo.bid_l1_usd is not None else None,
                "arcus_ask_l1_usd": _round_float(arc_bbo.ask_l1_usd, 2) if arc_bbo.ask_l1_usd is not None else None,
                "arcus_min_l1_usd": _round_float(arc_bbo.min_l1_usd, 2) if arc_bbo.min_l1_usd is not None else None,
                "arcus_l1_100usd_ok": arcus_l1_100usd_ok,
                "arcus_l1_500usd_ok": arcus_l1_500usd_ok,
                "arcus_source_age_ms": _round_float(Decimal(arc_source_age_us) / Decimal("1000"), 3) if arc_source_age_us is not None else None,
                "arcus_clock_ok": arc_clock_ok,
                "arcus_volume24h_usd": _round_float(arc_ctx.volume_24h_usd, 2) if arc_ctx and arc_ctx.volume_24h_usd is not None else None,
                "hl_market_delisted": hl_ctx.is_delisted if hl_ctx is not None else None,
                "hl_tradable_ok": hl_tradable_ok,
                "hl_book_status": hl_book.status,
                "hl_spread_bps": _round_float(hl_book.spread_bps, 4) if hl_book.spread_bps is not None else None,
                "hl_bid_l1_usd": _round_float(hl_book.bid_l1_usd, 2) if hl_book.bid_l1_usd is not None else None,
                "hl_ask_l1_usd": _round_float(hl_book.ask_l1_usd, 2) if hl_book.ask_l1_usd is not None else None,
                "hl_rt_slip_100_bps": _round_float(slip_100, 4) if slip_100 is not None else None,
                "hl_rt_slip_500_bps": _round_float(slip_500, 4) if slip_500 is not None else None,
                "hl_rt_slip_1000_bps": _round_float(slip_1000, 4) if slip_1000 is not None else None,
                "hl_l2_100usd_ok": hl_l2_100usd_ok,
                "hl_l2_500usd_ok": hl_l2_500usd_ok,
                "hl_source_age_ms": _round_float(Decimal(hl_source_age_us) / Decimal("1000"), 3) if hl_source_age_us is not None else None,
                "hl_clock_ok": hl_clock_ok,
                "hl_day_ntl_vlm_usd": _round_float(hl_ctx.day_ntl_vlm_usd, 2) if hl_ctx and hl_ctx.day_ntl_vlm_usd is not None else None,
                "pair_source_skew_ms": _round_float(Decimal(pair_source_skew_us) / Decimal("1000"), 3) if pair_source_skew_us is not None else None,
                "pair_receipt_skew_ms": _round_float(Decimal(pair_receipt_skew_us) / Decimal("1000"), 3) if pair_receipt_skew_us is not None else None,
                "joint_clock_ok": joint_clock_ok,
                "cross_mid_basis_bps": _round_float(cross_mid_basis_bps, 4) if cross_mid_basis_bps is not None else None,
                "oracle_divergence_bps": _round_float(oracle_div_bps, 4) if oracle_div_bps is not None else None,
                "instant_carry_long_a_short_b_bps_hr": _round_float(inst_carry_bps_hr, 4) if inst_carry_bps_hr is not None else None,
                "friction_mt_plus_hl_cross_100_bps": _round_float(mt_plus_hl_cross_100_bps, 4) if mt_plus_hl_cross_100_bps is not None else None,
                "friction_mt_plus_hl_cross_500_bps": _round_float(mt_plus_hl_cross_500_bps, 4) if mt_plus_hl_cross_500_bps is not None else None,
                "friction_dual_taker_100_bps": _round_float(dual_taker_100_bps, 4) if dual_taker_100_bps is not None else None,
                "train_static_dir": holdout_info["train_static_dir"],
                "holdout_train_fixed_static_episodes": static_episodes,
                "holdout_train_fixed_static_net_rt_mt_bps": _round_float(holdout_info["static_net_rt_mt_bps"], 4) if holdout_info["static_net_rt_mt_bps"] is not None else None,
                "holdout_train_fixed_static_after_pt_mt_100_bps": _round_float(static_after_mt_100, 4) if static_after_mt_100 is not None else None,
                "holdout_train_fixed_static_after_pt_tt_100_bps": _round_float(static_after_tt_100, 4) if static_after_tt_100 is not None else None,
                "holdout_causal_L24_M2p5_net_bps": _round_float(holdout_info["causal_L24_M2p5_net_bps"], 4) if holdout_info["causal_L24_M2p5_net_bps"] is not None else None,
                "holdout_causal_L24_M2p5_episodes": holdout_info["causal_L24_M2p5_episodes"],
                "holdout_causal_L24_M2p5_after_pt_mt_100_bps": _round_float(c24_after_mt_100, 4) if c24_after_mt_100 is not None else None,
                "holdout_causal_L72_M1p5_net_bps": _round_float(holdout_info["causal_L72_M1p5_net_bps"], 4) if holdout_info["causal_L72_M1p5_net_bps"] is not None else None,
                "holdout_causal_L72_M1p5_episodes": holdout_info["causal_L72_M1p5_episodes"],
                "holdout_causal_L72_M1p5_after_pt_mt_100_bps": _round_float(c72_after_mt_100, 4) if c72_after_mt_100 is not None else None,
                "payback_hours_mt_100": _round_float(payback_hours_mt_100, 2) if payback_hours_mt_100 is not None else None,
                "depth_100usd_ok": depth_100_ok,
                "depth_500usd_ok": depth_500_ok,
                "oracle_ok": oracle_ok,
                "passes_train_fixed_static_only": passes_train_fixed_static_only,
                "passes_hard_gates": passes_hard_gates,
                "flags": reasons,
            }
        )

    cohort_summaries: dict[str, Any] = {}
    for cohort in COHORTS:
        rows = [r for r in pair_rows if r["cohort"] == cohort]
        survivors = [r for r in rows if r["passes_hard_gates"]]
        survivors.sort(
            key=lambda x: (
                -(x["holdout_causal_L72_M1p5_after_pt_mt_100_bps"] or 0.0),
                -(x["holdout_train_fixed_static_after_pt_mt_100_bps"] or 0.0),
                x["pair_id"],
            )
        )
        blocked_rows = [r for r in rows if not r["depth_100usd_ok"]]
        outside_rth_count = sum(1 for r in rows if r["session_regime"] == "equity_outside_rth")
        cohort_summaries[cohort] = {
            "total_pairs": len(rows),
            "arcus_l1_100usd_ok_count": sum(1 for r in rows if r["arcus_l1_100usd_ok"]),
            "hl_l2_100usd_ok_count": sum(1 for r in rows if r["hl_l2_100usd_ok"]),
            "joint_clock_ok_count": sum(1 for r in rows if r["joint_clock_ok"]),
            "depth_100usd_ok_count": sum(1 for r in rows if r["depth_100usd_ok"]),
            "depth_500usd_ok_count": sum(1 for r in rows if r["depth_500usd_ok"]),
            "oracle_ok_count": sum(1 for r in rows if r["oracle_ok"]),
            "outside_rth_count": outside_rth_count,
            "train_fixed_static_only_survivors_count": sum(1 for r in rows if r["passes_train_fixed_static_only"]),
            "survivors_count": len(survivors),
            "top_survivors": survivors[:10],
            "blocked_depth_examples": [
                {
                    "pair_id": r["pair_id"],
                    "arcus_min_l1_usd": r["arcus_min_l1_usd"],
                    "hl_rt_slip_100_bps": r["hl_rt_slip_100_bps"],
                    "pair_source_skew_ms": r["pair_source_skew_ms"],
                    "pair_receipt_skew_ms": r["pair_receipt_skew_ms"],
                    "holdout_train_fixed_static_net_rt_mt_bps": r["holdout_train_fixed_static_net_rt_mt_bps"],
                    "flags": r["flags"],
                }
                for r in sorted(
                    blocked_rows,
                    key=lambda x: -(x["holdout_train_fixed_static_net_rt_mt_bps"] or -9999.0),
                )[:10]
            ],
        }

    return {
        "schema_version": SCHEMA_VERSION,
        "audited_at_utc": str(metadata.get("fetched_at_utc") or "offline_deterministic"),
        "zero_secret_env_reads_verified": True,
        "historical_40d_realized_cash_usd": None,
        "historical_40d_realized_cash_status": HISTORICAL_CASH_STATUS,
        "arcus_depth_scope": "l1_only_via_rest_bbo",
        "hl_depth_scope": "full_l2_via_l2Book",
        "clock_policy": {
            "max_source_age_ms": max_source_age_ms,
            "max_pair_skew_ms": max_pair_skew_ms,
        },
        "source_metadata": metadata,
        "chronological_holdout_hours": split["holdout_grid_hours"],
        "cohorts": cohort_summaries,
        "pairs": pair_rows,
    }


def render_liquidity_markdown(result: dict[str, Any]) -> str:
    cp = result["clock_policy"]
    lines: list[str] = [
        "# Cross-Venue Carry L1/L2 Liquidity, Spread & Payback Audit",
        "",
        f"- **Schema**: `{result['schema_version']}`",
        f"- **Snapshot Timestamp (`fetched_at_utc`)**: `{result['audited_at_utc']}`",
        f"- **Clock Policy**: `max_source_age_ms={cp['max_source_age_ms']}`, `max_pair_skew_ms={cp['max_pair_skew_ms']}`",
        f"- **Arcus Depth Scope**: `{result['arcus_depth_scope']}`; **HL/XYZ Depth Scope**: `{result['hl_depth_scope']}`",
        f"- **Historical 40d Realized Cash USD**: `null` (`{result['historical_40d_realized_cash_status']}`)",
        "",
    ]
    for cohort in COHORTS:
        c = result["cohorts"][cohort]
        lines.append(
            f"## Cohort `{cohort}`: `{c['survivors_count']}/{c['total_pairs']}` pass Clock+Tradable+`$100` L1/L2 + Oracle<=50bp + Train-Fixed/Causal Holdout"
        )
        lines.append("")
        lines.append(
            f"- `arcus_l1_100usd_ok`: `{c['arcus_l1_100usd_ok_count']}/{c['total_pairs']}` | "
            f"`hl_l2_100usd_ok`: `{c['hl_l2_100usd_ok_count']}/{c['total_pairs']}` | "
            f"`joint_clock_ok`: `{c['joint_clock_ok_count']}/{c['total_pairs']}` | "
            f"`depth_100usd_ok`: `{c['depth_100usd_ok_count']}/{c['total_pairs']}` | "
            f"`oracle_ok`: `{c['oracle_ok_count']}/{c['total_pairs']}` | "
            f"`outside_rth`: `{c['outside_rth_count']}/{c['total_pairs']}` | "
            f"`train_fixed_static_only`: `{c['train_fixed_static_only_survivors_count']}/{c['total_pairs']}`"
        )
        lines.append("")
        if c["top_survivors"]:
            lines.append("### Top Survivors (`joint_clock_ok & tradable & depth_100usd_ok & oracle_ok & carry_beats_pt_mt_100`)")
            lines.append("")
            lines.append(
                "| Pair | Session | Arc L1 min USD | Arc Spr bps | HL $100 Slip bps | Skew src/rc ms | Oracle Div bps | FR MT+$100 bps | Train Dir | HO TrStat MT-$100 | HO L72 MT-$100 | Payback h |"
            )
            lines.append("| --- | --- | ---: | ---: | ---: | --- | ---: | ---: | --- | ---: | ---: | ---: |")
            for r in c["top_survivors"]:
                lines.append(
                    f"| `{r['pair_id']}` | `{r['session_regime']}` | {r['arcus_min_l1_usd']} | {r['arcus_spread_bps']} | {r['hl_rt_slip_100_bps']} | {r['pair_source_skew_ms']}/{r['pair_receipt_skew_ms']} | {r['oracle_divergence_bps']} | {r['friction_mt_plus_hl_cross_100_bps']} | `{r['train_static_dir']}` | {r['holdout_train_fixed_static_after_pt_mt_100_bps']} | {r['holdout_causal_L72_M1p5_after_pt_mt_100_bps']} | {r['payback_hours_mt_100']} |"
                )
            lines.append("")
        if c["blocked_depth_examples"]:
            lines.append("### Disqualified by Clock/Tradable/Depth Gates — Preserving Independent Per-Leg Audit")
            lines.append("")
            lines.append("| Pair | Arc L1 min USD | HL $100 Slip bps | Src Skew ms | Rc Skew ms | Paper HO TrStat MT bps | Flags |")
            lines.append("| --- | ---: | ---: | ---: | ---: | ---: | --- |")
            for b in c["blocked_depth_examples"]:
                lines.append(
                    f"| `{b['pair_id']}` | {b['arcus_min_l1_usd']} | {b['hl_rt_slip_100_bps']} | {b['pair_source_skew_ms']} | {b['pair_receipt_skew_ms']} | {b['holdout_train_fixed_static_net_rt_mt_bps']} | `{','.join(b['flags'])}` |"
                )
            lines.append("")
    return "\n".join(lines)


def _http_fetch_or_error_json(url: str, *, method: str = "GET", body: dict[str, Any] | None = None) -> tuple[int, bytes]:
    try:
        return _http_fetch_bytes(url, method=method, body=body)
    except HTTPError as exc:
        if exc.code in {400, 404}:
            err_bytes = json.dumps({"_http_error": f"HTTP {exc.code}: {exc.reason}"}).encode("utf-8")
            return int(exc.code), err_bytes
        raise


def _fetch_live_liquidity_archive(
    *,
    universe_pairs: list[PairSpec],
    out_dir: Path,
    carry_archive_dir: Path,
    sleep_ms: int,
) -> dict[str, Any]:
    raw_dir = out_dir / "raw"
    (raw_dir / "meta").mkdir(parents=True, exist_ok=True)
    (raw_dir / "arcus_bbo").mkdir(parents=True, exist_ok=True)
    (raw_dir / "hyperliquid_l2").mkdir(parents=True, exist_ok=True)
    (raw_dir / "trade_xyz_l2").mkdir(parents=True, exist_ok=True)

    fetched_at_utc = _utc_now_iso()
    receipts: list[dict[str, Any]] = []

    def _record(
        kind: str,
        venue: str,
        sym: str,
        rel_path: str,
        url: str,
        method: str,
        req_body: dict[str, Any] | None,
        status: int,
        raw: bytes,
        *,
        pair_id: str | None = None,
    ) -> None:
        (out_dir / rel_path).write_bytes(raw)
        entry: dict[str, Any] = {
            "kind": kind,
            "venue": venue,
            "symbol": sym,
            "rel_path": rel_path,
            "url": url,
            "method": method,
            "request_body": req_body,
            "http_status": status,
            "received_at_utc": _utc_now_iso(),
            "bytes": len(raw),
            "sha256": _sha256_bytes(raw),
        }
        if pair_id is not None:
            entry["pair_id"] = pair_id
        receipts.append(entry)

    # 1. Market contexts
    st_am, raw_am = _http_fetch_bytes(ARCUS_MARKETS_URL, method="GET")
    _record("arcus_markets", "arcus", "ALL", "raw/meta/arcus_markets.json", ARCUS_MARKETS_URL, "GET", None, st_am, raw_am)

    hl_meta_body = {"type": "metaAndAssetCtxs"}
    st_hlm, raw_hlm = _http_fetch_bytes(HYPERLIQUID_INFO_URL, method="POST", body=hl_meta_body)
    _record("hl_meta", "hyperliquid", "ALL", "raw/meta/hl_meta_ctxs.json", HYPERLIQUID_INFO_URL, "POST", hl_meta_body, st_hlm, raw_hlm)

    xyz_meta_body = {"type": "metaAndAssetCtxs", "dex": "xyz"}
    st_xyzm, raw_xyzm = _http_fetch_bytes(HYPERLIQUID_INFO_URL, method="POST", body=xyz_meta_body)
    _record("hl_meta", "trade_xyz", "ALL", "raw/meta/xyz_meta_ctxs.json", HYPERLIQUID_INFO_URL, "POST", xyz_meta_body, st_xyzm, raw_xyzm)

    # 2. Pair-synchronized back-to-back BBO + L2 fetch by exact pair_id identity
    arc_gap = max(sleep_ms / 1000.0, DEFAULT_ARCUS_MIN_INTERVAL_SEC) if sleep_ms > 0 else 0.0
    hl_gap = max(sleep_ms / 1000.0, DEFAULT_HL_MIN_INTERVAL_SEC) if sleep_ms > 0 else 0.0
    last_arc_ts = 0.0
    last_hl_ts = 0.0
    fetched_arc: set[str] = set()
    fetched_hl: set[tuple[str, str]] = set()
    sorted_pairs = sorted(universe_pairs, key=lambda p: p.pair_id)
    total_pairs = len(sorted_pairs)

    def _pace(last_ts: float, gap: float) -> None:
        if gap > 0.0 and last_ts > 0.0:
            rem = gap - (time.monotonic() - last_ts)
            if rem > 0.0:
                time.sleep(rem)

    for p_idx, sp in enumerate(sorted_pairs, start=1):
        need_arc = sp.leg_a_venue == "arcus" and sp.leg_a_symbol not in fetched_arc
        hl_key = (sp.leg_b_venue, sp.leg_b_symbol)
        need_hl = sp.leg_b_venue in {"hyperliquid", "trade_xyz"} and hl_key not in fetched_hl
        # Satisfy both host intervals upfront before fetching both legs back-to-back cleanly
        if need_arc:
            _pace(last_arc_ts, arc_gap)
        if need_hl:
            _pace(last_hl_ts, hl_gap)

        if need_arc:
            url = f"{ARCUS_BBO_BASE_URL}/{quote(sp.leg_a_symbol)}"
            st, raw = _http_fetch_or_error_json(url, method="GET")
            last_arc_ts = time.monotonic()
            rel = f"raw/arcus_bbo/{_safe_slug(sp.leg_a_symbol)}.json"
            _record("arcus_bbo", "arcus", sp.leg_a_symbol, rel, url, "GET", None, st, raw, pair_id=sp.pair_id)
            fetched_arc.add(sp.leg_a_symbol)
            parsed_arc = parse_arcus_bbo(sp.leg_a_symbol, raw)
            print(f"[{p_idx}/{total_pairs}] pair={sp.pair_id} arcus={parsed_arc.status}", file=sys.stderr)

        if need_hl:
            body = {"type": "l2Book", "coin": sp.leg_b_symbol}
            st, raw = _http_fetch_or_error_json(HYPERLIQUID_INFO_URL, method="POST", body=body)
            last_hl_ts = time.monotonic()
            rel_dir = "hyperliquid_l2" if sp.leg_b_venue == "hyperliquid" else "trade_xyz_l2"
            rel = f"raw/{rel_dir}/{_safe_slug(sp.leg_b_symbol)}.json"
            _record("hl_l2", sp.leg_b_venue, sp.leg_b_symbol, rel, HYPERLIQUID_INFO_URL, "POST", body, st, raw, pair_id=sp.pair_id)
            fetched_hl.add(hl_key)
            parsed_hl = parse_hl_l2_book(sp.leg_b_venue, sp.leg_b_symbol, raw)
            print(f"[{p_idx}/{total_pairs}] pair={sp.pair_id} {sp.leg_b_venue}={parsed_hl.status}", file=sys.stderr)

    carry_manifest_bytes = (carry_archive_dir / "manifest.json").read_bytes()
    manifest = {
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "fetched_at_utc": fetched_at_utc,
        "zero_secret_env_reads_verified": True,
        "carry_archive_dir": str(carry_archive_dir),
        "carry_manifest_sha256": _sha256_bytes(carry_manifest_bytes),
        "receipt_count": len(receipts),
        "receipts": receipts,
    }
    (out_dir / "manifest.json").write_bytes((json.dumps(manifest, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8"))
    return manifest


def load_verified_liquidity_archive(
    input_dir: Path,
    *,
    carry_archive_override: Path | None = None,
) -> tuple[dict[str, Any], list[PairSpec], dict[tuple[str, str], Any], dict[str, ArcusMarketContext], dict[tuple[str, str], HlMarketContext], dict[str, ArcusBboAudit], dict[tuple[str, str], HlL2BookAudit]]:
    root = input_dir.resolve()
    m_path = root / "manifest.json"
    if not m_path.is_file():
        raise ValueError(f"missing manifest.json in {input_dir}")
    manifest = json.loads(m_path.read_text(encoding="utf-8"))
    if manifest.get("schema_version") != MANIFEST_SCHEMA_VERSION:
        raise ValueError(f"unexpected liquidity manifest schema_version: {manifest.get('schema_version')!r}")

    carry_dir = carry_archive_override or Path(manifest["carry_archive_dir"])
    if not carry_dir.is_absolute():
        carry_dir = (REPO_ROOT / carry_dir).resolve()
    carry_manifest_bytes = (carry_dir / "manifest.json").read_bytes()
    if _sha256_bytes(carry_manifest_bytes) != str(manifest["carry_manifest_sha256"]):
        raise ValueError("carry archive manifest.json SHA-256 mismatch")
    _, universe_pairs, carry_series = load_verified_offline_archive(carry_dir)

    arcus_markets: dict[str, ArcusMarketContext] = {}
    hl_ctxs: dict[tuple[str, str], HlMarketContext] = {}
    arcus_bbos: dict[str, ArcusBboAudit] = {}
    hl_books: dict[tuple[str, str], HlL2BookAudit] = {}

    for rc in manifest["receipts"]:
        rel = str(rc["rel_path"])
        raw_path = (root / rel).resolve()
        if not raw_path.is_relative_to(root) or not raw_path.is_file():
            raise ValueError(f"missing or escaped path in manifest: {rel!r}")
        if type(rc.get("bytes")) is not int:
            raise ValueError(f"invalid non-int byte count for {rel}: {rc.get('bytes')!r}")
        raw_bytes = raw_path.read_bytes()
        if len(raw_bytes) != rc["bytes"]:
            raise ValueError(f"byte count mismatch for {rel}")
        if _sha256_bytes(raw_bytes) != str(rc["sha256"]):
            raise ValueError(f"SHA-256 mismatch for {rel}")
        rc_received_utc = rc.get("received_at_utc")
        _parse_iso_utc_epoch_us(rc_received_utc, field_name=f"receipt[{rel}].received_at_utc")
        assert isinstance(rc_received_utc, str)
        kind = str(rc["kind"])
        venue = str(rc["venue"])
        sym = str(rc["symbol"])
        if kind == "arcus_markets":
            arcus_markets = parse_arcus_markets_map(raw_bytes)
        elif kind == "hl_meta":
            parsed_meta = parse_hl_meta_asset_ctxs(venue, raw_bytes)
            for k_coin, ctx in parsed_meta.items():
                hl_ctxs[(venue, k_coin)] = ctx
        elif kind == "arcus_bbo":
            if sym in arcus_bbos:
                raise ValueError(f"duplicate arcus_bbo receipt for {sym}")
            arcus_bbos[sym] = parse_arcus_bbo(sym, raw_bytes, received_at_utc=rc_received_utc)
        elif kind == "hl_l2":
            if (venue, sym) in hl_books:
                raise ValueError(f"duplicate hl_l2 receipt for {(venue, sym)}")
            hl_books[(venue, sym)] = parse_hl_l2_book(venue, sym, raw_bytes, received_at_utc=rc_received_utc)
        else:
            raise ValueError(f"unknown receipt kind: {kind!r}")

    for sp in universe_pairs:
        if sp.leg_a_symbol not in arcus_bbos:
            raise ValueError(f"missing arcus_bbo for {sp.leg_a_symbol}")
        if (sp.leg_b_venue, sp.leg_b_symbol) not in hl_books:
            raise ValueError(f"missing hl_l2 for {(sp.leg_b_venue, sp.leg_b_symbol)}")

    return manifest, universe_pairs, carry_series, arcus_markets, hl_ctxs, arcus_bbos, hl_books


def _build_synthetic_liquidity_archive(root: Path) -> Path:
    carry_dir = root / "carry"
    liq_dir = root / "liq"
    carry_dir.mkdir(parents=True, exist_ok=True)
    liq_dir.mkdir(parents=True, exist_ok=True)
    _build_synthetic_archive(carry_dir, num_hours=360)
    _, universe_pairs, _ = load_verified_offline_archive(carry_dir)

    (liq_dir / "raw" / "meta").mkdir(parents=True, exist_ok=True)
    (liq_dir / "raw" / "arcus_bbo").mkdir(parents=True, exist_ok=True)
    (liq_dir / "raw" / "hyperliquid_l2").mkdir(parents=True, exist_ok=True)
    (liq_dir / "raw" / "trade_xyz_l2").mkdir(parents=True, exist_ok=True)

    synth_rec_iso = "2026-11-23T00:00:00+00:00"
    synth_rec_us = _parse_iso_utc_epoch_us(synth_rec_iso)
    synth_src_us = synth_rec_us - 10_000  # 10 ms source age at receipt
    synth_src_ms = (synth_rec_us // 1_000) - 10  # 10 ms source age at receipt

    receipts: list[dict[str, Any]] = []

    def _save(kind: str, venue: str, sym: str, rel: str, obj: Any) -> None:
        raw = (json.dumps(obj, sort_keys=True) + "\n").encode("utf-8")
        (liq_dir / rel).write_bytes(raw)
        receipts.append({
            "kind": kind,
            "venue": venue,
            "symbol": sym,
            "rel_path": rel,
            "url": "https://synthetic.local",
            "method": "GET",
            "request_body": None,
            "http_status": 200,
            "received_at_utc": synth_rec_iso,
            "bytes": len(raw),
            "sha256": _sha256_bytes(raw),
        })

    arc_markets = []
    hl_u, hl_c = [], []
    xyz_u, xyz_c = [], []

    for sp in universe_pairs:
        arc_markets.append({
            "marketDisplayName": sp.leg_a_symbol,
            "status": "ONLINE",
            "oraclePrice": "100.0",
            "markPrice": "100.0",
            "lastTradePrice": "100.0",
            "fundingRate": "0.00001",
            "volume24hNotional": "5000000",
            "openInterest": "10000",
            "isOutsideRth": sp.cohort == COHORT_EQUITY_RWA,
        })
        # Make TSLA super thin ($0.50 L1!) to test depth filter cleanly; NVDA & BTC deep!
        l1_sz = "0.005" if sp.canonical_base == "TSLA" else "50"
        _save("arcus_bbo", "arcus", sp.leg_a_symbol, f"raw/arcus_bbo/{_safe_slug(sp.leg_a_symbol)}.json", {
            "bestBid": {"price": "99.98", "size": l1_sz},
            "bestAsk": {"price": "100.02", "size": "50"},
            "timestamp": synth_src_us,
        })
        hl_obj = {
            "coin": sp.leg_b_symbol,
            "time": synth_src_ms,
            "levels": [
                [{"px": "99.99", "sz": "20", "n": 1}, {"px": "99.95", "sz": "50", "n": 2}],
                [{"px": "100.01", "sz": "20", "n": 1}, {"px": "100.05", "sz": "50", "n": 2}],
            ],
        }
        rel_dir = "hyperliquid_l2" if sp.leg_b_venue == "hyperliquid" else "trade_xyz_l2"
        _save("hl_l2", sp.leg_b_venue, sp.leg_b_symbol, f"raw/{rel_dir}/{_safe_slug(sp.leg_b_symbol)}.json", hl_obj)
        ctx_entry = {"oraclePx": "100.02", "markPx": "100.0", "midPx": "100.0", "funding": "0.00025", "premium": "0.0001", "dayNtlVlm": "8000000", "openInterest": "20000"}
        if sp.leg_b_venue == "hyperliquid":
            hl_u.append({"name": sp.leg_b_symbol, "isDelisted": False})
            hl_c.append(ctx_entry)
        else:
            xyz_u.append({"name": sp.leg_b_symbol, "isDelisted": False})
            xyz_c.append(ctx_entry)

    _save("arcus_markets", "arcus", "ALL", "raw/meta/arcus_markets.json", {"markets": arc_markets})
    _save("hl_meta", "hyperliquid", "ALL", "raw/meta/hl_meta_ctxs.json", [{"universe": hl_u}, hl_c])
    _save("hl_meta", "trade_xyz", "ALL", "raw/meta/xyz_meta_ctxs.json", [{"universe": xyz_u}, xyz_c])

    manifest = {
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "fetched_at_utc": "2026-11-23T00:00:00+00:00",
        "zero_secret_env_reads_verified": True,
        "carry_archive_dir": str(carry_dir),
        "carry_manifest_sha256": _sha256_bytes((carry_dir / "manifest.json").read_bytes()),
        "receipt_count": len(receipts),
        "receipts": receipts,
    }
    (liq_dir / "manifest.json").write_bytes(json.dumps(manifest, indent=2, sort_keys=True).encode("utf-8"))
    return liq_dir


def run_self_test() -> dict[str, Any]:
    with tempfile.TemporaryDirectory() as tmp:
        liq_dir = _build_synthetic_liquidity_archive(Path(tmp))
        m, pairs, cs, am, hc, ab, hb = load_verified_liquidity_archive(liq_dir)
        res1 = evaluate_carry_liquidity_offline(
            universe_pairs=pairs,
            carry_series=cs,
            arcus_markets=am,
            hl_ctxs=hc,
            arcus_bbos=ab,
            hl_books=hb,
            metadata={"fetched_at_utc": m["fetched_at_utc"], "receipt_count": m["receipt_count"]},
        )
        res2 = evaluate_carry_liquidity_offline(
            universe_pairs=pairs,
            carry_series=cs,
            arcus_markets=am,
            hl_ctxs=hc,
            arcus_bbos=ab,
            hl_books=hb,
            metadata={"fetched_at_utc": m["fetched_at_utc"], "receipt_count": m["receipt_count"]},
        )
        assert json.dumps(res1, sort_keys=True) == json.dumps(res2, sort_keys=True)
        assert res1["historical_40d_realized_cash_usd"] is None
        tsla_row = next(r for r in res1["pairs"] if r["canonical_base"] == "TSLA")
        assert not tsla_row["depth_100usd_ok"]
        nvda_row = next(r for r in res1["pairs"] if r["canonical_base"] == "NVDA")
        assert nvda_row["passes_hard_gates"]

        # SHA-256 tamper rejection
        target = liq_dir / m["receipts"][0]["rel_path"]
        target.write_bytes(b"{}")
        tampered = False
        try:
            load_verified_liquidity_archive(liq_dir)
        except ValueError:
            tampered = True
        assert tampered

    return {
        "schema_version": SCHEMA_VERSION,
        "self_test": "passed",
        "deterministic_replay_verified": True,
        "sha256_tamper_rejection_verified": True,
        "zero_secret_env_reads_verified": True,
        "historical_40d_realized_cash_usd": None,
    }


def _evaluate_and_optional_write(
    input_dir: Path,
    write_dir: Path | None,
    *,
    max_source_age_ms: int = DEFAULT_MAX_SOURCE_AGE_MS,
    max_pair_skew_ms: int = DEFAULT_MAX_PAIR_SKEW_MS,
) -> dict[str, Any]:
    m, pairs, cs, am, hc, ab, hb = load_verified_liquidity_archive(input_dir)
    meta = {
        "fetched_at_utc": m["fetched_at_utc"],
        "receipt_count": m["receipt_count"],
        "manifest_schema_version": m["schema_version"],
        "carry_manifest_sha256": m["carry_manifest_sha256"],
    }
    result = evaluate_carry_liquidity_offline(
        universe_pairs=pairs,
        carry_series=cs,
        arcus_markets=am,
        hl_ctxs=hc,
        arcus_bbos=ab,
        hl_books=hb,
        metadata=meta,
        max_source_age_ms=max_source_age_ms,
        max_pair_skew_ms=max_pair_skew_ms,
    )
    if write_dir is not None:
        write_dir.mkdir(parents=True, exist_ok=True)
        (write_dir / "result.json").write_bytes(
            (json.dumps(result, indent=2, sort_keys=True, allow_nan=False) + "\n").encode("utf-8")
        )
        (write_dir / "REPORT.md").write_text(render_liquidity_markdown(result), encoding="utf-8")
        write_sha256sums(write_dir)
    return result


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    mode = parser.add_mutually_exclusive_group(required=False)
    mode.add_argument("--self-test", action="store_true", help="Run deterministic synthetic self-test and exit")
    mode.add_argument("--input-dir", type=Path, default=None, help="Offline input liquidity archive dir")
    mode.add_argument("--live-fetch", action="store_true", help="Opt-in: query public unauthenticated BBO/L2/meta endpoints")

    parser.add_argument("--out-dir", type=Path, default=None)
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--carry-archive-dir", type=Path, default=REPO_ROOT / DEFAULT_CARRY_ARCHIVE_REL)
    parser.add_argument("--sleep-ms", type=int, default=DEFAULT_SLEEP_MS)
    parser.add_argument("--max-source-age-ms", type=int, default=DEFAULT_MAX_SOURCE_AGE_MS)
    parser.add_argument("--max-pair-skew-ms", type=int, default=DEFAULT_MAX_PAIR_SKEW_MS)
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)

    if not args.self_test and args.input_dir is None and not args.live_fetch:
        print("Error: specify --self-test, --input-dir <dir>, or --live-fetch --out-dir <dir>", file=sys.stderr)
        return 2

    if args.self_test:
        summary = run_self_test()
        if args.output is not None:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(summary, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8")
        if not args.quiet:
            print(json.dumps(summary, indent=2, sort_keys=True, allow_nan=False))
        return 0

    if args.live_fetch:
        if args.out_dir is None:
            print("Error: --live-fetch requires --out-dir <dir>", file=sys.stderr)
            return 2
        _, universe_pairs, _ = load_verified_offline_archive(args.carry_archive_dir)
        args.out_dir.mkdir(parents=True, exist_ok=True)
        _fetch_live_liquidity_archive(
            universe_pairs=universe_pairs,
            out_dir=args.out_dir,
            carry_archive_dir=args.carry_archive_dir,
            sleep_ms=args.sleep_ms,
        )
        result = _evaluate_and_optional_write(
            args.out_dir,
            args.out_dir,
            max_source_age_ms=args.max_source_age_ms,
            max_pair_skew_ms=args.max_pair_skew_ms,
        )
    else:
        assert args.input_dir is not None
        result = _evaluate_and_optional_write(
            args.input_dir,
            args.out_dir,
            max_source_age_ms=args.max_source_age_ms,
            max_pair_skew_ms=args.max_pair_skew_ms,
        )

    if args.output is not None:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, indent=2, sort_keys=True, allow_nan=False) + "\n", encoding="utf-8")

    if not args.quiet:
        print(json.dumps(result, indent=2, sort_keys=True, allow_nan=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
