.PHONY: help build up down logs status clean

# Default target
help:
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "  AlephTX v3.2.0 - Tier-1 HFT Management"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo ""
	@echo "Build Commands:"
	@echo "  make build          - Compile all binaries"
	@echo "  make build-feeder   - Compile Go feeder only"
	@echo ""
	@echo "Testing:"
	@echo "  make test-up        - Start test environment (feeder + example)"
	@echo "  make test-down      - Stop test environment"
	@echo "  make test-logs      - View test logs"
	@echo ""
	@echo "Strategy Management:"
	@echo "  make up STRATEGY=lighter    - Start Lighter MM"
	@echo "  make up STRATEGY=edgex      - Start EdgeX MM"
	@echo "  make up STRATEGY=backpack   - Start Backpack MM"
	@echo "  make up STRATEGY=all        - Start all strategies"
	@echo ""
	@echo "  make down STRATEGY=lighter  - Stop Lighter MM"
	@echo "  make down STRATEGY=all      - Stop all strategies"
	@echo ""
	@echo "Monitoring:"
	@echo "  make logs STRATEGY=lighter  - View logs"
	@echo "  make status                 - Show running strategies"
	@echo ""
	@echo "Utilities:"
	@echo "  make clean                  - Clean build artifacts"
	@echo ""

# Build all binaries
build:
	@echo "🔨 Building AlephTX..."
	cargo build --release
	@echo "✅ Build complete"

# Build feeder for testing
build-feeder:
	@echo "🔨 Building Go feeder..."
	cd feeder && go build -o feeder-app
	@echo "✅ Feeder build complete"

# Start test environment (feeder + example)
test-up: build-feeder
	@echo "🧪 Starting test environment..."
	@mkdir -p logs pids
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events
	@(cd feeder && ./feeder-app) > logs/feeder-test.log 2>&1 & echo $$! > pids/feeder-test.pid
	@sleep 2
	@echo "✅ Feeder started (PID: $$(cat pids/feeder-test.pid))"
	@export $$(cat .env.lighter | xargs) && \
		export LD_LIBRARY_PATH=$$(pwd)/lib:$$LD_LIBRARY_PATH && \
		cargo run --example lighter_trading > logs/lighter-test.log 2>&1 & echo $$! > pids/lighter-test.pid
	@echo "✅ Lighter test started (PID: $$(cat pids/lighter-test.pid))"
	@echo "📊 Logs: tail -f logs/feeder-test.log logs/lighter-test.log"

# Stop test environment
test-down:
	@echo "🛑 Stopping test environment..."
	@if [ -f pids/feeder-test.pid ]; then kill -9 $$(cat pids/feeder-test.pid) 2>/dev/null || true; rm -f pids/feeder-test.pid; echo "✅ Feeder stopped"; fi
	@if [ -f pids/lighter-test.pid ]; then kill -9 $$(cat pids/lighter-test.pid) 2>/dev/null || true; rm -f pids/lighter-test.pid; echo "✅ Lighter test stopped"; fi
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events
	@echo "✅ Shared memory cleaned"

# View test logs
test-logs:
	@tail -f logs/feeder-test.log logs/lighter-test.log

# Start feeder (prerequisite for all strategies)
feeder:
	@if ! pgrep -f "feeder/feeder" > /dev/null; then \
		echo "🚀 Starting Go feeder..."; \
		cd feeder && nohup ./feeder > ../logs/feeder.log 2>&1 & \
		echo $$! > ../pids/feeder.pid; \
		sleep 2; \
		echo "✅ Feeder started (PID: $$(cat ../pids/feeder.pid))"; \
	else \
		echo "✅ Feeder already running"; \
	fi

# Start strategy
up:
	@mkdir -p logs pids
	@if [ "$(STRATEGY)" = "lighter" ]; then \
		$(MAKE) feeder; \
		echo "🚀 Starting Lighter MM..."; \
		export $$(cat .env | xargs) && \
		export LD_LIBRARY_PATH=$$(pwd)/lib:$$LD_LIBRARY_PATH && \
		nohup ./target/release/lighter_mm > logs/lighter.log 2>&1 & \
		echo $$! > pids/lighter.pid; \
		echo "✅ Lighter MM started (PID: $$(cat pids/lighter.pid))"; \
	elif [ "$(STRATEGY)" = "edgex" ]; then \
		$(MAKE) feeder; \
		echo "🚀 Starting EdgeX MM..."; \
		export $$(cat .env.edgex | xargs) && nohup ./target/release/aleph-tx > logs/edgex.log 2>&1 & \
		echo $$! > pids/edgex.pid; \
		echo "✅ EdgeX MM started (PID: $$(cat pids/edgex.pid))"; \
	elif [ "$(STRATEGY)" = "backpack" ]; then \
		$(MAKE) feeder; \
		echo "🚀 Starting Backpack MM..."; \
		export $$(cat .env.backpack | xargs) && nohup ./target/release/aleph-tx > logs/backpack.log 2>&1 & \
		echo $$! > pids/backpack.pid; \
		echo "✅ Backpack MM started (PID: $$(cat pids/backpack.pid))"; \
	elif [ "$(STRATEGY)" = "all" ]; then \
		$(MAKE) up STRATEGY=lighter; \
		$(MAKE) up STRATEGY=edgex; \
		$(MAKE) up STRATEGY=backpack; \
	else \
		echo "❌ Unknown strategy: $(STRATEGY)"; \
		echo "   Use: lighter, edgex, backpack, or all"; \
		exit 1; \
	fi

# Stop strategy
down:
	@if [ "$(STRATEGY)" = "lighter" ]; then \
		if [ -f pids/lighter.pid ]; then \
			echo "🛑 Stopping Lighter MM..."; \
			kill $$(cat pids/lighter.pid) 2>/dev/null || true; \
			rm -f pids/lighter.pid; \
			echo "✅ Lighter MM stopped"; \
		else \
			echo "⚠️  Lighter MM not running"; \
		fi \
	elif [ "$(STRATEGY)" = "edgex" ]; then \
		if [ -f pids/edgex.pid ]; then \
			echo "🛑 Stopping EdgeX MM..."; \
			kill $$(cat pids/edgex.pid) 2>/dev/null || true; \
			rm -f pids/edgex.pid; \
			echo "✅ EdgeX MM stopped"; \
		else \
			echo "⚠️  EdgeX MM not running"; \
		fi \
	elif [ "$(STRATEGY)" = "backpack" ]; then \
		if [ -f pids/backpack.pid ]; then \
			echo "🛑 Stopping Backpack MM..."; \
			kill $$(cat pids/backpack.pid) 2>/dev/null || true; \
			rm -f pids/backpack.pid; \
			echo "✅ Backpack MM stopped"; \
		else \
			echo "⚠️  Backpack MM not running"; \
		fi \
	elif [ "$(STRATEGY)" = "all" ]; then \
		$(MAKE) down STRATEGY=lighter; \
		$(MAKE) down STRATEGY=edgex; \
		$(MAKE) down STRATEGY=backpack; \
		if [ -f pids/feeder.pid ]; then \
			echo "🛑 Stopping feeder..."; \
			kill $$(cat pids/feeder.pid) 2>/dev/null || true; \
			rm -f pids/feeder.pid; \
			echo "✅ Feeder stopped"; \
		fi \
	else \
		echo "❌ Unknown strategy: $(STRATEGY)"; \
		exit 1; \
	fi

# View logs
logs:
	@if [ "$(STRATEGY)" = "lighter" ]; then \
		tail -f logs/lighter.log; \
	elif [ "$(STRATEGY)" = "edgex" ]; then \
		tail -f logs/edgex.log; \
	elif [ "$(STRATEGY)" = "backpack" ]; then \
		tail -f logs/backpack.log; \
	elif [ "$(STRATEGY)" = "feeder" ]; then \
		tail -f logs/feeder.log; \
	else \
		echo "❌ Unknown strategy: $(STRATEGY)"; \
		exit 1; \
	fi

# Show status
status:
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "  AlephTX Status"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo ""
	@if [ -f pids/feeder.pid ] && kill -0 $$(cat pids/feeder.pid) 2>/dev/null; then \
		echo "✅ Feeder:   RUNNING (PID: $$(cat pids/feeder.pid))"; \
	else \
		echo "❌ Feeder:   STOPPED"; \
	fi
	@if [ -f pids/lighter.pid ] && kill -0 $$(cat pids/lighter.pid) 2>/dev/null; then \
		echo "✅ Lighter:  RUNNING (PID: $$(cat pids/lighter.pid))"; \
	else \
		echo "❌ Lighter:  STOPPED"; \
	fi
	@if [ -f pids/edgex.pid ] && kill -0 $$(cat pids/edgex.pid) 2>/dev/null; then \
		echo "✅ EdgeX:    RUNNING (PID: $$(cat pids/edgex.pid))"; \
	else \
		echo "❌ EdgeX:    STOPPED"; \
	fi
	@if [ -f pids/backpack.pid ] && kill -0 $$(cat pids/backpack.pid) 2>/dev/null; then \
		echo "✅ Backpack: RUNNING (PID: $$(cat pids/backpack.pid))"; \
	else \
		echo "❌ Backpack: STOPPED"; \
	fi
	@echo ""

# Clean build artifacts
clean:
	@echo "🧹 Cleaning build artifacts..."
	cargo clean
	@echo "✅ Clean complete"
