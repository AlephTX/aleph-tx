#!/bin/bash
# Real-time AlephTX Dashboard

clear
echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║              AlephTX Real-Time Monitoring Dashboard              ║"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""

# Check if system is running
ALEPH_PID=$(pgrep -f "target/release/aleph-tx" | head -1)
if [ -z "$ALEPH_PID" ]; then
    echo "❌ AlephTX is NOT running"
    echo "   Start with: cargo run --release --bin aleph-tx"
    exit 1
else
    echo "✅ AlephTX is running (PID: $ALEPH_PID)"
fi

# Check feeders
FEEDER_COUNT=$(ps aux | grep feeder | grep -v grep | wc -l)
echo "📡 Go Feeders: $FEEDER_COUNT processes"

# Check shared memory
if [ -f "/dev/shm/aleph-matrix" ]; then
    SHM_SIZE=$(ls -lh /dev/shm/aleph-matrix | awk '{print $5}')
    SHM_TIME=$(stat -c %y /dev/shm/aleph-matrix | cut -d'.' -f1)
    echo "💾 Shared Memory: $SHM_SIZE (last update: $SHM_TIME)"
else
    echo "❌ Shared memory not found"
fi

echo ""
echo "═══════════════════════════════════════════════════════════════════"
echo "                        LIVE STRATEGY LOGS"
echo "═══════════════════════════════════════════════════════════════════"
echo ""

# Tail logs with color filtering
tail -f /tmp/aleph-tx.log 2>/dev/null | grep --line-buffered -E "(💰|🎒|🔌|✅|❌|⚠️|🛑|Balance|Vol=|Mom=|Pos=|INFO|WARN|ERROR)" | while read line; do
    # Add timestamp
    echo "[$(date '+%H:%M:%S')] $line"
done
