#!/usr/bin/env python3
"""Read-only Arcus public WS cadence probe: measures message inter-arrival per channel.

Public market data only (no keys, no orders). Prints aggregate cadence stats and the
envelope keys seen per channel (never raw payloads beyond key names).
"""

from __future__ import annotations

import argparse
import asyncio
import json
import statistics
import time

import os
import websockets

DIRECT = os.environ.get("ARCUS_PROBE_DIRECT") == "1"


def _ssl_ctx():
    import ssl

    try:
        import certifi

        return ssl.create_default_context(cafile=certifi.where())
    except ImportError:
        return ssl.create_default_context()


async def probe(url: str, markets: list[str], channels: list[str], seconds: float) -> dict:
    arrivals: dict[tuple[str, str], list[float]] = {}
    keys: dict[str, set] = {}
    errors: list[str] = []
    async with websockets.connect(url, max_size=2**23, ssl=_ssl_ctx(), **({"proxy": None} if DIRECT else {})) as ws:
        for m in markets:
            for ch in channels:
                sub = {"type": "subscribe", "channel": ch, "id": m}
                if ch == "l2Orderbook":
                    sub["nLevels"] = 20
                await ws.send(json.dumps(sub))
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
            ch = str(msg.get("channel", msg.get("type", "?")))
            mid = str(msg.get("id", "?"))
            if msg.get("type") in ("error",) or "error" in msg:
                errors.append(f"{ch}:{str(msg)[:160]}")
                continue
            keys.setdefault(ch, set()).update(msg.keys())
            data = msg.get("contents") or msg.get("data")
            if isinstance(data, dict):
                keys[ch].update(f"data.{k}" for k in list(data.keys())[:12])
            if msg.get("type") == "channel_data":
                arrivals.setdefault((ch, mid), []).append(now)
    out = {}
    for (ch, mid), ts in sorted(arrivals.items()):
        d = [(b - a) * 1000 for a, b in zip(ts, ts[1:])]
        if not d:
            continue
        d.sort()
        out[f"{ch}:{mid}"] = {
            "msgs": len(ts),
            "per_s": round(len(ts) / seconds, 2),
            "p10_ms": round(d[len(d) // 10], 1),
            "p50_ms": round(statistics.median(d), 1),
            "p90_ms": round(d[int(len(d) * 0.9)], 1),
            "min_ms": round(d[0], 1),
        }
    return {"cadence": out, "envelope_keys": {k: sorted(v) for k, v in keys.items()}, "errors": errors[:10]}


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="wss://api.arcus.xyz/v1/ws")
    ap.add_argument("--markets", default="BTC-USD,SPY-USD,ETH-USD")
    ap.add_argument("--channels", default="l2Orderbook,bbo,trades")
    ap.add_argument("--seconds", type=float, default=60.0)
    a = ap.parse_args()
    res = asyncio.run(probe(a.url, a.markets.split(","), a.channels.split(","), a.seconds))
    print(json.dumps(res, indent=1))


if __name__ == "__main__":
    main()
