#!/bin/bash

echo "🧪 Testing Adaptive Market Maker"
echo "================================"
echo ""

# Test 1: Account stats reader
echo "Test 1: Account Stats Reader"
echo "----------------------------"
cargo run --example test_account_stats
echo ""

# Test 2: Check shared memory
echo "Test 2: Shared Memory Status"
echo "----------------------------"
ls -lh /dev/shm/aleph-* 2>/dev/null || echo "No shared memory files found"
echo ""

# Test 3: Check feeder logs
echo "Test 3: Feeder Account Stats"
echo "----------------------------"
if [ -f logs/feeder-adaptive.log ]; then
    echo "Last 5 account stats updates:"
    grep "collateral=" logs/feeder-adaptive.log | tail -5
else
    echo "Feeder log not found"
fi
echo ""

echo "✅ Tests complete"
