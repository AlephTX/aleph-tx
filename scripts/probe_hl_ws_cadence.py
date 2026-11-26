#!/usr/bin/env python3
"""Read-only Hyperliquid public WS cadence probe.

Public market data only (no keys, no orders). Subscribes the requested
channels for each coin on one connection (or one connection per variant with
--split) and prints aggregate inter-arrival stats per (variant, coin). Never
prints raw payloads.

Variants: l2Book, l2Book_fast (adds "fast": true), bbo.
"""

from __future__ import annotations

import argparse
import asyncio
import json
import os
import statistics
import time

import websockets

DIRECT = os.environ.get("HL_PROBE_PROXY") != "1"


def _ssl_ctx():
    import ssl

    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


def _subscription(variant: str, coin: str) -> dict:
    if variant == "l2Book":
        return {"type": "l2Book", "coin": coin}
    if variant == "l2Book_fast":
        return {"type": "l2Book", "coin": coin, "fast": True}
    if variant == "bbo":
        return {"type": "bbo", "coin": coin}
    raise ValueError(variant)


def _stats(ts: list[float], seconds: float) -> dict | None:
    d = sorted((b - a) * 1000 for a, b in zip(ts, ts[1:]))
    if not d:
        return {"msgs": len(ts)}
    return {
        "msgs": len(ts),
        "per_s": round(len(ts) / seconds, 2),
        "p10_ms": round(d[len(d) // 10], 1),
        "p50_ms": round(statistics.median(d), 1),
        "p90_ms": round(d[int(len(d) * 0.9)], 1),
        "max_ms": round(d[-1], 1),
    }


async def probe_conn(url: str, variants: list[str], coins: list[str], seconds: float) -> dict:
    arrivals: dict[str, list[float]] = {}
    src_gaps: dict[str, list[int]] = {}
    last_src: dict[str, int] = {}
    acks: list[str] = []
    errors: list[str] = []
    kw = {"proxy": None} if DIRECT else {}
    async with websockets.connect(url, max_size=2**23, ssl=_ssl_ctx(), **kw) as ws:
        for v in variants:
            for c in coins:
                await ws.send(json.dumps({"method": "subscribe", "subscription": _subscription(v, c)}))
        t_end = time.monotonic() + seconds
        while time.monotonic() < t_end:
            try:
                raw = await asyncio.wait_for(ws.recv(), timeout=max(0.1, t_end - time.monotonic()))
            except asyncio.TimeoutError:
                break
            now = time.monotonic()
            try:
                msg = json.loads(raw)
            except Exception:
                continue
            ch = msg.get("channel")
            data = msg.get("data")
            if ch == "subscriptionResponse":
                sub = (data or {}).get("subscription", {})
                acks.append(f"{sub.get('type')}:{sub.get('coin')}:fast={sub.get('fast', False)}")
                continue
            if ch == "error":
                errors.append(str(data)[:160])
                continue
            if ch not in ("l2Book", "bbo") or not isinstance(data, dict):
                continue
            coin = data.get("coin", "?")
            # Fast and normal l2Book share the channel name; label by what this
            # connection subscribed (use --split to separate them).
            label = ch if ch == "bbo" else "+".join(v for v in variants if v.startswith("l2Book"))
            key = f"{label}:{coin}"
            arrivals.setdefault(key, []).append(now)
            src = data.get("time")
            if isinstance(src, int):
                if key in last_src:
                    src_gaps.setdefault(key, []).append(src - last_src[key])
                last_src[key] = src
    out = {}
    for key, ts in sorted(arrivals.items()):
        s = _stats(ts, seconds)
        g = sorted(src_gaps.get(key, []))
        if g:
            s["src_time_p50_ms"] = statistics.median(g)
        out[key] = s
    return {"cadence": out, "acks": acks, "errors": errors[:10]}


async def main_async(a) -> dict:
    coins = a.coins.split(",")
    variants = a.variants.split(",")
    if a.split:
        results = await asyncio.gather(*(probe_conn(a.url, [v], coins, a.seconds) for v in variants))
        merged: dict = {"cadence": {}, "acks": [], "errors": []}
        for r in results:
            merged["cadence"].update(r["cadence"])
            merged["acks"] += r["acks"]
            merged["errors"] += r["errors"]
        return merged
    return await probe_conn(a.url, variants, coins, a.seconds)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="wss://api.hyperliquid.xyz/ws")
    ap.add_argument("--coins", default="BTC,xyz:NVDA")
    ap.add_argument("--variants", default="l2Book,l2Book_fast,bbo")
    ap.add_argument("--split", action="store_true", help="one connection per variant")
    ap.add_argument("--seconds", type=float, default=45.0)
    a = ap.parse_args()
    print(json.dumps(asyncio.run(main_async(a)), indent=1))


if __name__ == "__main__":
    main()
