#!/bin/bash
# Close Lighter position before shutdown

set -e

# Load environment variables
if [ -f .env.lighter ]; then
    export $(cat .env.lighter | xargs)
else
    echo "❌ .env.lighter not found"
    exit 1
fi

# Check if we have the required environment variables
if [ -z "$API_KEY_PRIVATE_KEY" ] || [ -z "$LIGHTER_ACCOUNT_INDEX" ] || [ -z "$LIGHTER_API_KEY_INDEX" ]; then
    echo "❌ Missing required environment variables"
    exit 1
fi

echo "🔍 Checking Lighter position..."

# Query current position via API
ACCOUNT_INDEX=$LIGHTER_ACCOUNT_INDEX
API_URL="https://mainnet.zklighter.elliot.ai/api/v1"

# Get account info
RESPONSE=$(curl -s "${API_URL}/accounts/${ACCOUNT_INDEX}")

# Parse position (this is a simplified version - you may need to adjust based on actual API response)
POSITION=$(echo "$RESPONSE" | jq -r '.positions[0].size // 0' 2>/dev/null || echo "0")

echo "📊 Current position: $POSITION ETH"

if [ "$POSITION" != "0" ] && [ "$POSITION" != "0.0" ]; then
    echo "⚠️  Non-zero position detected: $POSITION ETH"
    echo "🚀 Executing market close order..."

    # Run the Rust binary to close position
    export LD_LIBRARY_PATH=$(pwd)/lib:$LD_LIBRARY_PATH
    cargo run --release --bin close_position -- \
        --market-id 0 \
        --position "$POSITION"

    echo "✅ Position close order submitted"
else
    echo "✅ No position to close"
fi
