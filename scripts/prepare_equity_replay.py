#!/usr/bin/env python3
"""Build a capture config from an archived Nasdaq chart and a reviewed session."""

import argparse
import json
from datetime import date, datetime, time, timezone
from decimal import Decimal
from pathlib import Path
from zoneinfo import ZoneInfo

NEW_YORK = ZoneInfo("America/New_York")
MINUTE_MS = 60_000


def read_json(path: Path) -> dict:
    with path.open(encoding="utf-8") as stream:
        return json.load(stream)


def local_time_ms(day: date, clock: time) -> int:
    if clock.tzinfo is not None:
        raise ValueError("session clock values must be local New York times")
    return int(datetime.combine(day, clock, NEW_YORK).timestamp() * 1_000)


def prepare(config: dict, archive: dict, session: dict, publication_delay: int) -> dict:
    chart = archive["data"]
    day = date.fromisoformat(session["trade_date"])
    underlying = f"NASDAQ:{chart['symbol']}"
    if (
        archive["status"]["rCode"] != 200
        or not chart["exchange"].startswith("NASDAQ-")
        or datetime.strptime(chart["timeAsOf"], "%b %d, %Y").date() != day
        or config["pair"]["asset_class"] != "equity"
        or underlying != session["underlying_id"]
        or any(leg["underlying_id"] != underlying for leg in config["pair"]["legs"])
    ):
        raise ValueError("chart, reviewed session, and replay pair must identify the same equity and date")

    opens = local_time_ms(day, time.fromisoformat(session["opens_et"]))
    closes = local_time_ms(day, time.fromisoformat(session["closes_et"]))
    clear = local_time_ms(day, time.fromisoformat(session["actions_clear_through_et"]))
    if not 0 < opens < clear <= closes:
        raise ValueError("invalid session or company action coverage")
    observation = {"opens_at_ms": opens, "closes_at_ms": closes, "clear_until_ms": clear}
    observations = {opens: {**observation, "observed_at_ms": opens, "halted": False}}
    prior_end = opens
    for halt in session["halts"]:
        start = local_time_ms(day, time.fromisoformat(halt["starts_et"]))
        resume = halt["resumes_et"]
        end = local_time_ms(day, time.fromisoformat(resume)) if resume is not None else closes
        if not prior_end <= start < end <= closes:
            raise ValueError("invalid or overlapping halt interval")
        observations[start] = {**observation, "observed_at_ms": start, "halted": True}
        if end < closes:
            observations[end] = {**observation, "observed_at_ms": end, "halted": False}
        prior_end = end

    bars = []
    for point in chart["chart"]:
        label = datetime.strptime(point["z"]["dateTime"], "%I:%M %p ET").time()
        # Nasdaq's display coordinate encodes the local wall time as if it were UTC.
        wall_ms = int(datetime.combine(day, label, timezone.utc).timestamp() * 1_000)
        if point["x"] != wall_ms:
            raise ValueError("Nasdaq chart display coordinate does not match its New York label")
        start = local_time_ms(day, label)
        end = start + MINUTE_MS
        if opens <= start and end <= closes and point["y"] is not None:
            price = Decimal(point["z"]["value"].replace(",", ""))
            if not price.is_finite() or price <= 0:
                raise ValueError("invalid regular-session stock price")
            bars.append({"ends_at_ms": end, "close": format(price, "f")})
    if not bars or any(a["ends_at_ms"] >= b["ends_at_ms"] for a, b in zip(bars, bars[1:])):
        raise ValueError("missing or unordered regular-session stock bars")
    if not 1 <= publication_delay <= MINUTE_MS:
        raise ValueError("publication delay must be between 1 ms and 1 minute")
    config["pair"]["equity_minute_reference_max_age_ms"] = publication_delay + MINUTE_MS
    config["reference"] = {
        "kind": "equity_minute_bars",
        "underlying_id": underlying,
        "publication_delay_ms": publication_delay,
        "bars": bars,
    }
    config["equity_sessions"] = list(observations.values())
    return config


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("nasdaq_chart", type=Path)
    parser.add_argument("reviewed_session", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--publication-delay-ms", type=int, default=MINUTE_MS)
    args = parser.parse_args()
    config = prepare(
        read_json(args.config), read_json(args.nasdaq_chart), read_json(args.reviewed_session),
        args.publication_delay_ms,
    )
    with args.output.open("x", encoding="utf-8") as stream:
        json.dump(config, stream, indent=2)
        stream.write("\n")


if __name__ == "__main__":
    main()
