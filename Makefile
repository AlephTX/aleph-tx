# ============================================================================
# AlephTX v5.0.0 - Institutional Grade Build System
# ============================================================================

.PHONY: help build build-rust build-feeder test test-rust test-go check fmt-check benchmark clean status clean-shm
.PHONY: trade
.PHONY: dashboard-install dashboard-build dashboard-check dashboard-dev test-python
.PHONY: lighter-up lighter-down lighter-logs
.PHONY: backpack-up backpack-down backpack-logs
.PHONY: edgex-up edgex-down edgex-logs

# Configuration
STRATEGY      ?= lighter_inventory_mm
LOG_DIR       ?= logs
PID_DIR       ?= pids
BIN_DIR       ?= target/release
FEEDER_BIN    ?= feeder/feeder-app
MODE          ?= check
ifeq ($(MODE),testnet)
CONFIG        ?= config.testnet.toml
ENV_FILE      ?= .env.testnet
else
CONFIG        ?= config.toml
ENV_FILE      ?= .env.lighter
endif
SHM_PATHS     ?= /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-events-v2 /dev/shm/aleph-account-stats /dev/shm/aleph-depth

# Colors for terminal output
BLUE   := \033[1;34m
GREEN  := \033[1;32m
YELLOW := \033[1;33m
RED    := \033[1;31m
NC     := \033[0m

help:
	@echo "$(BLUE)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━$(NC)"
	@echo "$(BLUE)  AlephTX v5.0.0 - Institutional HFT Framework$(NC)"
	@echo "$(BLUE)━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━$(NC)"
	@echo ""
	@echo "$(YELLOW)Build Commands:$(NC)"
	@echo "  make build          - Compile Rust binaries and Go feeder"
	@echo "  make build-feeder   - Compile Go feeder only"
	@echo "  make trade          - Check the trading profile offline"
	@echo "  make trade MODE=live - Run Lighter with optional Hyperliquid inventory hedge"
	@echo "  make trade MODE=testnet - Run the isolated Lighter/Hyperliquid testnet profile"
	@echo ""
	@echo "$(YELLOW)Exchange Commands (<exchange>-up/down/logs):$(NC)"
	@echo "  Target exchanges: lighter, backpack, edgex"
	@echo "  Example: make lighter-up STRATEGY=lighter_adaptive_mm"
	@echo ""
	@echo "$(YELLOW)Monitoring:$(NC)"
	@echo "  make status         - Show all running components"
	@echo "  make dashboard-build - Check and build the trading console SPA"
	@echo "  make dashboard-dev  - Run the console against the oasis tunnel"
	@echo "  make test-python    - Test the monitor telemetry store"
	@echo ""
	@echo "$(YELLOW)Utilities:$(NC)"
	@echo "  make clean          - Purge build artifacts and logs"
	@echo ""

# Offline verification never starts a strategy or submits exchange orders.
benchmark:
	cargo test --locked --release --lib benchmark_ -- --ignored --nocapture
	cd feeder && go test -run '^$$' -bench . -benchmem ./shm ./exchanges

test: test-rust test-go

test-rust:
	cargo test --locked --lib --bins

test-go:
	cd feeder && go test -race ./...

test-python:
	cd deploy/oasis && python3 -m unittest -v test_telemetry_store test_alerts

# Dashboard SPA (Vite + React). Built locally; install.sh ships dashboard/dist.
dashboard-install:
	cd dashboard && npm ci

dashboard-check: dashboard-install
	cd dashboard && npm run check

dashboard-build: dashboard-check
	cd dashboard && npm run build

# Dev server proxies /api to ALEPH_API (default: the IAP tunnel on 127.0.0.1:8765).
dashboard-dev:
	cd dashboard && npm run dev

check:
	cargo clippy --locked --lib --bins -- -D warnings
	cd feeder && go vet ./...

fmt-check:
	cargo fmt --check
	test -z "$$(cd feeder && gofmt -l .)"

ifneq ($(filter $(MODE),live testnet),)
trade: build-feeder
	cargo build --locked --release --bin lighter_inventory_mm
	"$(BIN_DIR)/lighter_inventory_mm" "--$(MODE)" "$(CONFIG)" "$(ENV_FILE)" "$(FEEDER_BIN)"
else ifeq ($(MODE),check)
trade:
	cargo run --locked --quiet --bin lighter_inventory_mm -- --check "$(CONFIG)"
else
trade:
	@echo "MODE must be check, live, or testnet" >&2; exit 2
endif

# Core Build
build: build-rust build-feeder

build-rust:
	@echo "$(BLUE)🔨 Building AlephTX binaries...$(NC)"
	cargo build --locked --release
	@echo "$(GREEN)✅ Rust binaries ready$(NC)"

build-feeder:
	@echo "$(BLUE)🔨 Building Go feeder...$(NC)"
	@cd feeder && go build -o feeder-app
	@cd feeder && go build -o perp-capture ./cmd/perp-capture
	@echo "$(GREEN)✅ Feeder binary ready$(NC)"

clean-shm:
	@echo "$(YELLOW)🧹 Cleaning shared memory...$(NC)"
	@rm -f $(SHM_PATHS)

# Generic Exchange Management Template
# Usage: $(call exchange_up,exchange_name,env_file,extra_args)
define exchange_up
	@echo "$(BLUE)🚀 Starting $(1) - Strategy: $(STRATEGY)$(NC)"
	@mkdir -p $(LOG_DIR) $(PID_DIR)
	@# Legacy stacks share one IPC namespace. Refuse to start over another feeder.
	@if pgrep -f '(^|/)feeder-app($$| )' >/dev/null 2>&1; then \
		echo "A feeder is already running. Stop its strategy and feeder before using the legacy launcher." >&2; exit 1; \
	fi
	@rm -f $(PID_DIR)/$(1)-$(STRATEGY).pid $(PID_DIR)/feeder-$(1).pid
	@make clean-shm
	@# Build strategy before starting feeder so SHM readiness is checked immediately before launch
	@cargo build --release --bin $(STRATEGY)
	@# Start Feeder
	@set -a; . ./$(2); set +a; \
		: > $(LOG_DIR)/feeder-$(1).log; \
		setsid $(FEEDER_BIN) config.toml >> $(LOG_DIR)/feeder-$(1).log 2>&1 < /dev/null & \
		feeder_pid=$$!; \
		echo $$feeder_pid > $(PID_DIR)/feeder-$(1).pid; \
		ready=0; \
		for i in $$(seq 1 75); do \
			if [ -e /dev/shm/aleph-matrix ] && [ -e /dev/shm/aleph-account-stats ] && [ -e /dev/shm/aleph-events-v2 ] && [ -e /dev/shm/aleph-depth ]; then \
				ready=1; \
				break; \
			fi; \
			if ! kill -0 $$feeder_pid 2>/dev/null; then \
				echo "$(RED)❌ feeder-$(1) exited before SHM was ready$(NC)"; \
				tail -n 80 $(LOG_DIR)/feeder-$(1).log; \
				rm -f $(PID_DIR)/feeder-$(1).pid; \
				exit 1; \
			fi; \
			sleep 0.2; \
		done; \
		if [ $$ready -ne 1 ]; then \
			echo "$(RED)❌ feeder-$(1) did not initialize SHM in time$(NC)"; \
			tail -n 80 $(LOG_DIR)/feeder-$(1).log; \
			kill -9 $$feeder_pid 2>/dev/null || true; \
			rm -f $(PID_DIR)/feeder-$(1).pid; \
			exit 1; \
		fi
	@# Start Strategy
	@set -a; . ./$(2); set +a; \
		: > $(LOG_DIR)/$(1)-$(STRATEGY).log; \
		export LD_LIBRARY_PATH=$$(pwd)/src/native:$$LD_LIBRARY_PATH; \
		setsid $(BIN_DIR)/$(STRATEGY) >> $(LOG_DIR)/$(1)-$(STRATEGY).log 2>&1 < /dev/null & \
		echo $$! > $(PID_DIR)/$(1)-$(STRATEGY).pid
	@echo "$(GREEN)✅ $(1) components started$(NC)"
endef

define exchange_down
	@echo "$(YELLOW)🛑 Stopping $(1)...$(NC)"
	@# Kill strategies
	@for pid_file in $(PID_DIR)/$(1)-*.pid; do \
		if [ -f "$$pid_file" ]; then \
			pid=$$(cat $$pid_file); \
			kill -2 $$pid 2>/dev/null || true; \
			echo "⏳ Waiting for PID $$pid..."; \
			timeout 30s tail --pid=$$pid -f /dev/null 2>/dev/null || true; \
			if kill -0 $$pid 2>/dev/null; then \
				echo "Strategy did not finish cleanup; feeder left running." >&2; exit 1; \
			fi; \
			rm -f $$pid_file; \
		fi; \
	done
	@# Kill Feeder
	@if [ -f $(PID_DIR)/feeder-$(1).pid ]; then \
		kill -9 $$(cat $(PID_DIR)/feeder-$(1).pid) 2>/dev/null || true; \
		rm -f $(PID_DIR)/feeder-$(1).pid; \
	fi
	@echo "$(GREEN)✅ $(1) clean stop complete$(NC)"
endef

# Exchange Targets
ifeq ($(STRATEGY),lighter_inventory_mm)
lighter-up:
	$(MAKE) trade MODE=live CONFIG="$(CONFIG)" ENV_FILE="$(ENV_FILE)"
else
lighter-up: build-feeder
	$(call exchange_up,lighter,.env.lighter)
endif

lighter-down:
	$(call exchange_down,lighter)

lighter-logs:
	@tail -f $(LOG_DIR)/feeder-lighter.log $(LOG_DIR)/lighter-*.log

backpack-up: build-feeder
	$(call exchange_up,backpack,.env.backpack)
backpack-up: STRATEGY := $(if $(filter lighter_inventory_mm,$(STRATEGY)),backpack_mm,$(STRATEGY))

backpack-down:
	$(call exchange_down,backpack)

backpack-logs:
	@tail -f $(LOG_DIR)/feeder-backpack.log $(LOG_DIR)/backpack-*.log

edgex-up: build-feeder
	$(call exchange_up,edgex,.env.edgex)
edgex-up: STRATEGY := $(if $(filter lighter_inventory_mm,$(STRATEGY)),edgex_mm,$(STRATEGY))

edgex-down:
	$(call exchange_down,edgex)

edgex-logs:
	@tail -f $(LOG_DIR)/feeder-edgex.log $(LOG_DIR)/edgex-*.log

status:
	@echo "$(BLUE)📊 AlephTX System Status$(NC)"
	@echo "--------------------------------------------------"
	@for pid_file in $(PID_DIR)/*.pid; do \
		if [ -f "$$pid_file" ]; then \
			strategy=$$(basename $$pid_file .pid); \
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
