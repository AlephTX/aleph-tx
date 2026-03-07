#!/bin/bash
# Performance analysis script for Inventory-Neutral MM

LOG_FILE="${1:-logs/inventory-neutral-live.log}"

echo "═══════════════════════════════════════════════════════════════"
echo "  Inventory-Neutral MM Performance Analysis"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# Extract session start time and balance
START_TIME=$(grep "Starting main loop" "$LOG_FILE" | head -1 | awk '{print $1}')
START_BALANCE=$(grep "available" "$LOG_FILE" | head -1 | grep -oP '\$\K[0-9.]+')

# Extract current time and latest balance
END_TIME=$(tail -1 "$LOG_FILE" | awk '{print $1}')
CURRENT_BALANCE=$(grep "available" "$LOG_FILE" | tail -1 | grep -oP '\$\K[0-9.]+')

echo "📅 Session Period:"
echo "   Start:  $START_TIME"
echo "   End:    $END_TIME"
echo ""

echo "💰 Balance:"
echo "   Start:   \$$START_BALANCE"
echo "   Current: \$$CURRENT_BALANCE"
if [ -n "$START_BALANCE" ] && [ -n "$CURRENT_BALANCE" ]; then
    PNL=$(echo "$CURRENT_BALANCE - $START_BALANCE" | bc -l)
    PNL_PCT=$(echo "scale=2; ($PNL / $START_BALANCE) * 100" | bc -l)
    echo "   PnL:     \$$PNL ($PNL_PCT%)"
fi
echo ""

echo "📊 Fill Statistics:"
grep "type=2" "$LOG_FILE" | \
awk '{
  match($0, /fill_price=([0-9.]+)/, price);
  match($0, /fill_size=([0-9.]+)/, size);
  match($0, /order_id=([0-9]+)/, id);

  # Determine side based on order_id prefix
  if (substr(id[1], 1, 3) == "281") {
    side = "ASK";
    ask_vol += size[1];
    ask_count++;
    ask_sum += price[1] * size[1];
  } else {
    side = "BID";
    bid_vol += size[1];
    bid_count++;
    bid_sum += price[1] * size[1];
  }

  total_vol += size[1];
  total_count++;
  total_sum += price[1] * size[1];
}
END {
  printf "   Total fills:    %d\n", total_count;
  printf "   Total volume:   %.4f ETH ($%.2f)\n", total_vol, total_sum;
  printf "   Bid fills:      %d (%.4f ETH, avg $%.2f)\n", bid_count, bid_vol, bid_count > 0 ? bid_sum/bid_vol : 0;
  printf "   Ask fills:      %d (%.4f ETH, avg $%.2f)\n", ask_count, ask_vol, ask_count > 0 ? ask_sum/ask_vol : 0;
  printf "   Net position:   %.4f ETH\n", bid_vol - ask_vol;
  printf "   Inventory %%:    %.1f%%\n", total_vol > 0 ? ((bid_vol - ask_vol) / total_vol) * 100 : 0;
}'
echo ""

echo "📈 Order Statistics:"
BATCH_COUNT=$(grep -c "Batch:" "$LOG_FILE")
echo "   Batch orders:   $BATCH_COUNT"

# Calculate session duration in minutes
if [ -n "$START_TIME" ] && [ -n "$END_TIME" ]; then
    START_SEC=$(date -d "$START_TIME" +%s 2>/dev/null || echo 0)
    END_SEC=$(date -d "$END_TIME" +%s 2>/dev/null || echo 0)
    if [ $START_SEC -gt 0 ] && [ $END_SEC -gt 0 ]; then
        DURATION_MIN=$(echo "($END_SEC - $START_SEC) / 60" | bc -l)
        echo "   Duration:       $(printf "%.1f" $DURATION_MIN) minutes"

        FILL_COUNT=$(grep -c "type=2" "$LOG_FILE")
        if [ $FILL_COUNT -gt 0 ] && [ $(echo "$DURATION_MIN > 0" | bc) -eq 1 ]; then
            FILLS_PER_MIN=$(echo "scale=1; $FILL_COUNT / $DURATION_MIN" | bc -l)
            echo "   Fills/minute:   $FILLS_PER_MIN"
        fi
    fi
fi
echo ""

echo "⚠️  Warnings/Errors:"
ERROR_COUNT=$(grep -c "WARN\|ERROR" "$LOG_FILE")
echo "   Total warnings: $ERROR_COUNT"
if [ $ERROR_COUNT -gt 0 ]; then
    echo ""
    echo "   Recent issues:"
    grep "WARN\|ERROR" "$LOG_FILE" | tail -5 | sed 's/^/   /'
fi
echo ""

echo "═══════════════════════════════════════════════════════════════"
