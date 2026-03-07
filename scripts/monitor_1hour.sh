#!/bin/bash
# 1-hour live trading monitor and performance analyzer

LOG_FILE="logs/inventory-neutral-live.log"
REPORT_FILE="logs/1hour_report_$(date +%Y%m%d_%H%M%S).txt"
START_TIME=$(date +%s)
END_TIME=$((START_TIME + 3600))

echo "═══════════════════════════════════════════════════════════════"
echo "  Inventory-Neutral MM - 1 Hour Live Trading Monitor"
echo "═══════════════════════════════════════════════════════════════"
echo ""
echo "Start time: $(date)"
echo "End time:   $(date -d @$END_TIME)"
echo "Log file:   $LOG_FILE"
echo "Report:     $REPORT_FILE"
echo ""
echo "Monitoring in progress..."
echo ""

# Wait for 1 hour
sleep 3600

echo "═══════════════════════════════════════════════════════════════"
echo "  1-Hour Trading Session Complete"
echo "═══════════════════════════════════════════════════════════════"
echo ""

# Generate comprehensive report
./scripts/analyze_performance.sh "$LOG_FILE" | tee "$REPORT_FILE"

echo ""
echo "Report saved to: $REPORT_FILE"
echo ""
echo "To stop trading: make live-down"
