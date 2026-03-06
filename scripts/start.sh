#!/bin/bash
# AlephTX Quick Start Script

set -e

echo "🚀 AlephTX Startup Script"
echo "=========================="
echo ""

# Check prerequisites
echo "📋 Checking prerequisites..."

if [ ! -f ".env.backpack" ]; then
    echo "❌ Missing .env.backpack"
    exit 1
fi

if [ ! -f ".env.edgex" ]; then
    echo "❌ Missing .env.edgex"
    exit 1
fi

if [ ! -f "/dev/shm/aleph-matrix" ]; then
    echo "⚠️  Shared memory not found. Make sure Go feeder is running!"
    echo "   Run: cd feeder && go run ."
    read -p "Continue anyway? (y/n) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 1
    fi
fi

echo "✅ Prerequisites OK"
echo ""

# Build
echo "🔨 Building release binary..."
cargo build --release --quiet

echo "✅ Build complete"
echo ""

# Choose strategy
echo "📊 Select strategy:"
echo "  1) Original v3 (BackpackMM + EdgeX MM)"
echo "  2) Advanced v4 (AdvancedMM - experimental)"
echo "  3) Performance Monitor only"
echo ""
read -p "Choice (1-3): " choice

case $choice in
    1)
        echo "🎯 Starting with original v3 strategies..."
        RUST_LOG=info,aleph_tx=debug ./target/release/aleph-tx
        ;;
    2)
        echo "🚀 Starting with advanced v4 strategy..."
        echo "⚠️  Note: You need to modify src/main.rs to use AdvancedMMStrategy"
        echo "   See OPTIMIZATION_GUIDE.md for instructions"
        RUST_LOG=info,aleph_tx=debug ./target/release/aleph-tx
        ;;
    3)
        echo "📈 Starting performance monitor..."
        ./target/release/performance_monitor
        ;;
    *)
        echo "❌ Invalid choice"
        exit 1
        ;;
esac
