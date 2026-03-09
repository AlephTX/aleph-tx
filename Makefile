.PHONY: help build build-feeder clean status
.PHONY: lighter-up lighter-down lighter-logs
.PHONY: backpack-up backpack-down backpack-logs
.PHONY: edgex-up edgex-down edgex-logs

# Default strategy for each exchange
STRATEGY ?= inventory_neutral_mm

# Default target
help:
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "  AlephTX v4.0.0 - Tier-1 HFT Framework"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo ""
	@echo "Build Commands:"
	@echo "  make build          - Compile all binaries"
	@echo "  make build-feeder   - Compile Go feeder only"
	@echo ""
	@echo "Unified Trading Commands:"
	@echo "  make lighter-up STRATEGY=<name>   - Start Lighter DEX strategy"
	@echo "  make lighter-down                 - Stop Lighter DEX"
	@echo "  make lighter-logs                 - View Lighter logs"
	@echo ""
	@echo "  make backpack-up STRATEGY=<name>  - Start Backpack strategy"
	@echo "  make backpack-down                - Stop Backpack"
	@echo "  make backpack-logs                - View Backpack logs"
	@echo ""
	@echo "  make edgex-up STRATEGY=<name>     - Start EdgeX strategy"
	@echo "  make edgex-down                   - Stop EdgeX"
	@echo "  make edgex-logs                   - View EdgeX logs"
	@echo ""
	@echo "Available Strategies:"
	@echo "  inventory_neutral_mm  - Inventory-neutral market maker (default)"
	@echo "  adaptive_mm           - Adaptive market maker"
	@echo "  simple_mm             - Simple market maker demo"
	@echo ""
	@echo "Examples:"
	@echo "  make lighter-up                          # Default: inventory_neutral_mm"
	@echo "  make lighter-up STRATEGY=adaptive_mm     # Adaptive MM on Lighter"
	@echo "  make backpack-up STRATEGY=simple_mm      # Simple MM on Backpack"
	@echo ""
	@echo "Monitoring:"
	@echo "  make status         - Show all running strategies"
	@echo ""
	@echo "Utilities:"
	@echo "  make clean          - Clean build artifacts"
	@echo ""

# Build all binaries
build:
	@echo "🔨 Building AlephTX..."
	cargo build --release
	@echo "✅ Build complete"

# Build feeder
build-feeder:
	@echo "🔨 Building Go feeder..."
	cd feeder && go build -o feeder-app
	@echo "✅ Feeder build complete"

# ============================================================================
# Lighter DEX
# ============================================================================

lighter-up: build-feeder
	@echo "🚀 Starting Lighter DEX - Strategy: $(STRATEGY)"
	@mkdir -p logs pids
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@# Start feeder (uses unified config.toml from root)
	@export $$(cat .env.lighter | xargs) && \
		./feeder/feeder-app config.toml > logs/feeder-lighter.log 2>&1 & \
		echo $$! > pids/feeder-lighter.pid
	@sleep 2
	@echo "✅ Feeder started (PID: $$(cat pids/feeder-lighter.pid))"
	@# Start strategy
	@export $$(cat .env.lighter | xargs) && \
		export LD_LIBRARY_PATH=$$(pwd)/src/native:$$LD_LIBRARY_PATH && \
		cargo run --release --example $(STRATEGY) > logs/lighter-$(STRATEGY).log 2>&1 & \
		echo $$! > pids/lighter-$(STRATEGY).pid
	@echo "✅ Strategy started (PID: $$(cat pids/lighter-$(STRATEGY).pid))"
	@echo "📊 Logs: tail -f logs/feeder-lighter.log logs/lighter-$(STRATEGY).log"

lighter-down:
	@echo "🛑 Stopping Lighter DEX..."
	@# Find and stop strategy
	@for pid_file in pids/lighter-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			echo "📤 Sending graceful shutdown signal..."; \
			kill -2 $$(cat $$pid_file) 2>/dev/null || true; \
			echo "⏳ Waiting for graceful shutdown (15s)..."; \
			sleep 15; \
			if ps -p $$(cat $$pid_file) > /dev/null 2>&1; then \
				echo "⚠️  Process still running, forcing shutdown..."; \
				kill -9 $$(cat $$pid_file) 2>/dev/null || true; \
			fi; \
			rm -f $$pid_file; \
			echo "✅ Strategy stopped"; \
		fi; \
	done
	@# Stop feeder
	@if [ -f pids/feeder-lighter.pid ]; then \
		kill -9 $$(cat pids/feeder-lighter.pid) 2>/dev/null || true; \
		rm -f pids/feeder-lighter.pid; \
		echo "✅ Feeder stopped"; \
	fi
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@echo "✅ Shared memory cleaned"

lighter-logs:
	@tail -f logs/feeder-lighter.log logs/lighter-*.log

# ============================================================================
# Backpack
# ============================================================================

backpack-up: build-feeder
	@echo "🚀 Starting Backpack - Strategy: $(STRATEGY)"
	@mkdir -p logs pids
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@# Start feeder
	@export $$(cat .env.lighter | xargs) && \
		(cd feeder && ./feeder-app) > logs/feeder-backpack.log 2>&1 & \
		echo $$! > pids/feeder-backpack.pid
	@sleep 2
	@echo "✅ Feeder started (PID: $$(cat pids/feeder-backpack.pid))"
	@# Start strategy
	@export $$(cat .env.backpack | xargs) && \
		export BACKPACK_ENV_PATH=.env.backpack && \
		cargo run --release --example $(STRATEGY) > logs/backpack-$(STRATEGY).log 2>&1 & \
		echo $$! > pids/backpack-$(STRATEGY).pid
	@echo "✅ Strategy started (PID: $$(cat pids/backpack-$(STRATEGY).pid))"
	@echo "📊 Logs: tail -f logs/feeder-backpack.log logs/backpack-$(STRATEGY).log"

backpack-down:
	@echo "🛑 Stopping Backpack..."
	@# Find and stop strategy
	@for pid_file in pids/backpack-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			echo "📤 Sending graceful shutdown signal..."; \
			kill -2 $$(cat $$pid_file) 2>/dev/null || true; \
			echo "⏳ Waiting for graceful shutdown (10s)..."; \
			sleep 10; \
			if ps -p $$(cat $$pid_file) > /dev/null 2>&1; then \
				echo "⚠️  Process still running, forcing shutdown..."; \
				kill -9 $$(cat $$pid_file) 2>/dev/null || true; \
			fi; \
			rm -f $$pid_file; \
			echo "✅ Strategy stopped"; \
		fi; \
	done
	@# Stop feeder
	@if [ -f pids/feeder-backpack.pid ]; then \
		kill -9 $$(cat pids/feeder-backpack.pid) 2>/dev/null || true; \
		rm -f pids/feeder-backpack.pid; \
		echo "✅ Feeder stopped"; \
	fi
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@echo "✅ Shared memory cleaned"

backpack-logs:
	@tail -f logs/feeder-backpack.log logs/backpack-*.log

# ============================================================================
# EdgeX
# ============================================================================

edgex-up: build-feeder
	@echo "🚀 Starting EdgeX - Strategy: edgex_mm"
	@mkdir -p logs pids
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@# Build strategy first
	@cargo build --release --example edgex_mm
	@# Start feeder (uses unified config.toml from root)
	@./feeder/feeder-app config.toml > logs/feeder-edgex.log 2>&1 & \
		echo $$! > pids/feeder-edgex.pid
	@sleep 2
	@echo "✅ Feeder started (PID: $$(cat pids/feeder-edgex.pid))"
	@# Start strategy
	@export $$(cat .env.edgex | xargs) && \
		export EDGEX_ENV_PATH=.env.edgex && \
		./target/release/examples/edgex_mm > logs/edgex-mm.log 2>&1 & \
		echo $$! > pids/edgex-mm.pid
	@echo "✅ Strategy started (PID: $$(cat pids/edgex-mm.pid))"
	@echo "📊 Logs: tail -f logs/feeder-edgex.log logs/edgex-mm.log"

edgex-down:
	@echo "🛑 Stopping EdgeX..."
	@# Find and stop strategy
	@for pid_file in pids/edgex-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			echo "📤 Sending graceful shutdown signal..."; \
			kill -2 $$(cat $$pid_file) 2>/dev/null || true; \
			echo "⏳ Waiting for graceful shutdown (10s)..."; \
			sleep 10; \
			if ps -p $$(cat $$pid_file) > /dev/null 2>&1; then \
				echo "⚠️  Process still running, forcing shutdown..."; \
				kill -9 $$(cat $$pid_file) 2>/dev/null || true; \
			fi; \
			rm -f $$pid_file; \
			echo "✅ Strategy stopped"; \
		fi; \
	done
	@# Stop feeder
	@if [ -f pids/feeder-edgex.pid ]; then \
		kill -9 $$(cat pids/feeder-edgex.pid) 2>/dev/null || true; \
		rm -f pids/feeder-edgex.pid; \
		echo "✅ Feeder stopped"; \
	fi
	@rm -f /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats
	@echo "✅ Shared memory cleaned"

edgex-logs:
	@tail -f logs/feeder-edgex.log logs/edgex-*.log

# ============================================================================
# Monitoring & Utilities
# ============================================================================

status:
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo "  AlephTX Status"
	@echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
	@echo ""
	@echo "Lighter DEX:"
	@if [ -f pids/feeder-lighter.pid ] && kill -0 $$(cat pids/feeder-lighter.pid) 2>/dev/null; then \
		echo "  ✅ Feeder:   RUNNING (PID: $$(cat pids/feeder-lighter.pid))"; \
	else \
		echo "  ❌ Feeder:   STOPPED"; \
	fi
	@for pid_file in pids/lighter-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			strategy=$$(basename $$pid_file .pid | sed 's/lighter-//'); \
			if kill -0 $$(cat $$pid_file) 2>/dev/null; then \
				echo "  ✅ $$strategy: RUNNING (PID: $$(cat $$pid_file))"; \
			else \
				echo "  ❌ $$strategy: STOPPED"; \
			fi; \
		fi; \
	done
	@echo ""
	@echo "Backpack:"
	@if [ -f pids/feeder-backpack.pid ] && kill -0 $$(cat pids/feeder-backpack.pid) 2>/dev/null; then \
		echo "  ✅ Feeder:   RUNNING (PID: $$(cat pids/feeder-backpack.pid))"; \
	else \
		echo "  ❌ Feeder:   STOPPED"; \
	fi
	@for pid_file in pids/backpack-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			strategy=$$(basename $$pid_file .pid | sed 's/backpack-//'); \
			if kill -0 $$(cat $$pid_file) 2>/dev/null; then \
				echo "  ✅ $$strategy: RUNNING (PID: $$(cat $$pid_file))"; \
			else \
				echo "  ❌ $$strategy: STOPPED"; \
			fi; \
		fi; \
	done
	@echo ""
	@echo "EdgeX:"
	@if [ -f pids/feeder-edgex.pid ] && kill -0 $$(cat pids/feeder-edgex.pid) 2>/dev/null; then \
		echo "  ✅ Feeder:   RUNNING (PID: $$(cat pids/feeder-edgex.pid))"; \
	else \
		echo "  ❌ Feeder:   STOPPED"; \
	fi
	@for pid_file in pids/edgex-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			strategy=$$(basename $$pid_file .pid | sed 's/edgex-//'); \
			if kill -0 $$(cat $$pid_file) 2>/dev/null; then \
				echo "  ✅ $$strategy: RUNNING (PID: $$(cat $$pid_file))"; \
			else \
				echo "  ❌ $$strategy: STOPPED"; \
			fi; \
		fi; \
	done
	@echo ""

clean:
	@echo "🧹 Cleaning build artifacts..."
	cargo clean
	@echo "✅ Clean complete"
