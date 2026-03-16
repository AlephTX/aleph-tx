# ============================================================================
# AlephTX v5.0.0 - Institutional Grade Build System
# ============================================================================

.PHONY: help build build-feeder clean status clean-shm
.PHONY: lighter-up lighter-down lighter-logs
.PHONY: backpack-up backpack-down backpack-logs
.PHONY: edgex-up edgex-down edgex-logs

# Configuration
STRATEGY      ?= lighter_inventory_mm
LOG_DIR       ?= logs
PID_DIR       ?= pids
BIN_DIR       ?= target/release
FEEDER_BIN    ?= feeder/feeder-app
SHM_PATHS     ?= /dev/shm/aleph-matrix /dev/shm/aleph-events /dev/shm/aleph-account-stats /dev/shm/aleph-depth

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
	@echo "  make build          - Compile all Rust binaries"
	@echo "  make build-feeder   - Compile Go feeder only"
	@echo ""
	@echo "$(YELLOW)Exchange Commands (<exchange>-up/down/logs):$(NC)"
	@echo "  Target exchanges: lighter, backpack, edgex"
	@echo "  Example: make lighter-up STRATEGY=lighter_adaptive_mm"
	@echo ""
	@echo "$(YELLOW)Monitoring:$(NC)"
	@echo "  make status         - Show all running components"
	@echo ""
	@echo "$(YELLOW)Utilities:$(NC)"
	@echo "  make clean          - Purge build artifacts and logs"
	@echo ""

# Core Build
build:
	@echo "$(BLUE)🔨 Building AlephTX binaries...$(NC)"
	cargo build --release
	@echo "$(GREEN)✅ Rust binaries ready$(NC)"

build-feeder:
	@echo "$(BLUE)🔨 Building Go feeder...$(NC)"
	@cd feeder && go build -o feeder-app
	@echo "$(GREEN)✅ Feeder binary ready$(NC)"

clean-shm:
	@echo "$(YELLOW)🧹 Cleaning shared memory...$(NC)"
	@rm -f $(SHM_PATHS)

# Generic Exchange Management Template
# Usage: $(call exchange_up,exchange_name,env_file,extra_args)
define exchange_up
	@echo "$(BLUE)🚀 Starting $(1) - Strategy: $(STRATEGY)$(NC)"
	@mkdir -p $(LOG_DIR) $(PID_DIR)
	@# Clean up stale same-exchange processes before starting a fresh session
	@pgrep -f '(^|/)$(STRATEGY)($$| )' >/dev/null 2>&1 && pkill -9 -f '(^|/)$(STRATEGY)($$| )' || true
	@pgrep -f '(^|/)feeder-app($$| )' >/dev/null 2>&1 && pkill -9 -f '(^|/)feeder-app($$| )' || true
	@rm -f $(PID_DIR)/$(1)-$(STRATEGY).pid $(PID_DIR)/feeder-$(1).pid
	@make clean-shm
	@# Start Feeder
	@set -a; . ./$(2); set +a; \
		: > $(LOG_DIR)/feeder-$(1).log; \
		setsid $(FEEDER_BIN) config.toml >> $(LOG_DIR)/feeder-$(1).log 2>&1 < /dev/null & \
		feeder_pid=$$!; \
		echo $$feeder_pid > $(PID_DIR)/feeder-$(1).pid; \
		ready=0; \
		for i in $$(seq 1 75); do \
			if [ -e /dev/shm/aleph-matrix ] && [ -e /dev/shm/aleph-account-stats ] && [ -e /dev/shm/aleph-depth ]; then \
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
	@cargo build --release --bin $(STRATEGY)
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
			kill -9 $$pid 2>/dev/null || true; \
			rm -f $$pid_file; \
		fi; \
	done
	@# Kill Feeder
	@if [ -f $(PID_DIR)/feeder-$(1).pid ]; then \
		kill -9 $$(cat $(PID_DIR)/feeder-$(1).pid) 2>/dev/null || true; \
		rm -f $(PID_DIR)/feeder-$(1).pid; \
	fi
	@# Kill orphaned processes left behind by failed starts or stale pid files
	@pgrep -f '(^|/)$(STRATEGY)($$| )' >/dev/null 2>&1 && pkill -9 -f '(^|/)$(STRATEGY)($$| )' || true
	@pgrep -f '(^|/)feeder-app($$| )' >/dev/null 2>&1 && pkill -9 -f '(^|/)feeder-app($$| )' || true
	@make clean-shm
	@echo "$(GREEN)✅ $(1) clean stop complete$(NC)"
endef

# Exchange Targets
lighter-up: build-feeder
	$(call exchange_up,lighter,.env.lighter)

lighter-down:
	$(call exchange_down,lighter)

lighter-logs:
	@tail -f $(LOG_DIR)/feeder-lighter.log $(LOG_DIR)/lighter-*.log

backpack-up: build-feeder
	$(call exchange_up,backpack,.env.backpack)

backpack-down:
	$(call exchange_down,backpack)

backpack-logs:
	@tail -f $(LOG_DIR)/feeder-backpack.log $(LOG_DIR)/backpack-*.log

edgex-up: build-feeder
	$(call exchange_up,edgex,.env.edgex)

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
