#!/bin/bash

# Load environment variables
export $(cat .env.edgex | xargs)

# Run the order placement
cargo run --bin aleph-tx -- --exchange edgex --action place-order --side buy --size 0.01 --price 1500 2>&1 | grep -E "INVALID|SUCCESS|EdgeX API error|Order placed|error"
