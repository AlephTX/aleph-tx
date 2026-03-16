#!/bin/bash
set -euo pipefail

STRATEGY_LOG="${1:-logs/lighter-lighter_inventory_mm.log}"
FEEDER_LOG="${2:-logs/feeder-lighter.log}"

if [[ ! -f "$STRATEGY_LOG" ]]; then
  echo "strategy log not found: $STRATEGY_LOG" >&2
  exit 1
fi

if [[ ! -f "$FEEDER_LOG" ]]; then
  echo "feeder log not found: $FEEDER_LOG" >&2
  exit 1
fi

start_line=$(awk '/Startup complete:/{line=NR} END{print line+0}' "$STRATEGY_LOG")
if [[ "$start_line" -le 0 ]]; then
  start_line=1
fi

strategy_tmp=$(mktemp)
feeder_tmp=$(mktemp)
trap 'rm -f "$strategy_tmp" "$feeder_tmp"' EXIT

tail -n +"$start_line" "$STRATEGY_LOG" > "$strategy_tmp"

start_ts=$(head -n 1 "$strategy_tmp" | awk '{print $1}')
if [[ -n "$start_ts" ]]; then
  feeder_start_line=$(awk -v ts="$start_ts" '$1 >= substr(ts,1,10) {print NR; exit}' "$FEEDER_LOG")
else
  feeder_start_line=1
fi
tail -n +"${feeder_start_line:-1}" "$FEEDER_LOG" > "$feeder_tmp"

fills=$(grep -c "💰 Fill:" "$strategy_tmp" || true)
orders_placed=$(grep -c 'Order placed successfully' "$strategy_tmp" || true)
orders_rejected=$(grep -c 'order_rejected\|Order rejected' "$strategy_tmp" || true)
batches=$(grep -c 'Submitting batch of' "$strategy_tmp" || true)
cancels=$(grep -c '🚫 Order canceled' "$strategy_tmp" || true)
post_only_rejects=$(grep -c 'canceled-post-only' "$feeder_tmp" || true)

summary=$(awk '
/📊 PnL:/ {
  line=$0
}
END {
  print line
}' "$strategy_tmp")

fill_stats=$(awk '
/💰 Fill:/ {
  for (i=1; i<=NF; i++) {
    if ($i ~ /^side=/) {
      split($i,a,"="); side=a[2]
    } else if ($i ~ /^size=/) {
      split($i,a,"="); size=a[2] + 0
    } else if ($i ~ /^price=/) {
      split($i,a,"="); price=a[2] + 0
    }
  }
  total_count++
  total_base += size
  total_notional += size * price
  if (side == "bid") {
    bid_count++
    bid_base += size
    bid_notional += size * price
  } else if (side == "ask") {
    ask_count++
    ask_base += size
    ask_notional += size * price
  }
}
END {
  printf "fills=%d total_base=%.4f total_notional=%.2f bid_count=%d bid_base=%.4f ask_count=%d ask_base=%.4f\n",
    total_count, total_base, total_notional, bid_count, bid_base, ask_count, ask_base
}' "$strategy_tmp")

open_order_stats=$(awk '
/open_order_count/ {
  if (match($0, /open_order_count[^0-9]*([0-9]+)/, m)) {
    val = m[1] + 0
    if (!seen++) {
      min = max = val
    }
    if (val < min) min = val
    if (val > max) max = val
    sum += val
    count++
  }
}
END {
  if (count == 0) {
    printf "min=0 max=0 avg=0.00 count=0\n"
  } else {
    printf "min=%d max=%d avg=%.2f count=%d\n", min, max, sum / count, count
  }
}' "$feeder_tmp")

echo "═══════════════════════════════════════════════════════════════"
echo "  Lighter Session Review"
echo "═══════════════════════════════════════════════════════════════"
echo "Session start line: $start_line"
echo "Strategy log: $STRATEGY_LOG"
echo "Feeder log:   $FEEDER_LOG"
echo
echo "Execution:"
echo "  orders_placed=$orders_placed"
echo "  orders_rejected=$orders_rejected"
echo "  batches=$batches"
echo "  cancels=$cancels"
echo "  post_only_rejects=$post_only_rejects"
echo
echo "Fills:"
echo "  $fill_stats"
echo
echo "Open orders:"
echo "  $open_order_stats"
echo
echo "Latest PnL:"
echo "  ${summary:-no pnl line found}"
echo "═══════════════════════════════════════════════════════════════"
