PORT ?= 47100
PIDFILE ?= /tmp/orchestratord.pid
LOG ?= /tmp/orchestratord.dev.log
BIN ?= target/debug/orchestratord

# `start` must launch orchestratord from the repo root (not apps/orchestrator/) so
# its CWD-relative defaults (--workflows-dir, --activity-dir, ORCHESTRATOR_CALENDAR_SCRIPT)
# resolve correctly whether this Makefile is invoked directly or via `$(MAKE) -C
# apps/orchestrator start` from the root Makefile (which sets CWD=apps/orchestrator).
# Since the recipe's CWD changes, $(BIN) becomes unusable relative to the daemon's new
# CWD, so the launch must use an absolute binary path instead (mirrors the root
# Makefile's DAEMON_BIN_ABS convention).
REPO_ROOT ?= $(CURDIR)/../..
BIN_ABS ?= $(CURDIR)/$(BIN)

# These duplicate strings that also appear in the root Makefile's packaging
# paths (PLIST_DEST/SYSTEMD_DEST). That duplication is deliberate: `stop`
# only needs the service label / unit name to ask the OS service manager to
# stop the daemon, and moving install/uninstall themselves here is out of
# this task's boundary.
LAUNCHD_LABEL ?= com.wk-voice-agent.orchestratord
SYSTEMD_UNIT ?= orchestratord.service
PLIST_DEST = $(HOME)/Library/LaunchAgents/$(LAUNCHD_LABEL).plist

.PHONY: test build run build-daemon start stop restart status tail release

test:
	cargo test -p orchestrator --target-dir target

build:
	cargo build -p orchestrator --bin orchestratord --target-dir target

run:
	cargo run -p orchestrator --bin orchestratord --target-dir target

build-daemon:
	cargo build -p orchestrator --bin orchestratord --target-dir target

# Zero-dependency fallback for a `make start`-backgrounded dev instance
# only. Prefer `orchestrator log -f` (quick task 260812-qeg): it works
# whether the daemon is dev-backgrounded, launchd-managed, or (now)
# systemd-managed, and works even when the daemon isn't running at all.
tail:
	tail -f /tmp/orchestratord.dev.log

start: build-daemon
	@cd $(REPO_ROOT) || exit 1; \
	if [ -f "$(PIDFILE)" ] && kill -0 "$$(cat $(PIDFILE))" 2>/dev/null; then \
		echo "orchestratord already running (pid $$(cat $(PIDFILE)))"; \
		exit 0; \
	fi; \
	nohup $(BIN_ABS) --port $(PORT) > $(LOG) 2>&1 & \
	echo $$! > $(PIDFILE); \
	PID=$$(cat $(PIDFILE)); \
	TRIES=0; \
	while [ $$TRIES -lt 50 ]; do \
		if command -v nc >/dev/null 2>&1; then \
			nc -z 127.0.0.1 $(PORT) 2>/dev/null && break; \
		else \
			sleep 0.1; \
			break; \
		fi; \
		sleep 0.1; \
		TRIES=$$((TRIES + 1)); \
	done; \
	echo "orchestratord started (pid $$PID) on 127.0.0.1:$(PORT)"

stop:
	@ACTED=0; \
	TIER1_ACTED=0; \
	OS=$$(uname -s); \
	if [ "$$OS" = "Darwin" ]; then \
		if launchctl list 2>/dev/null | grep -q "$(LAUNCHD_LABEL)"; then \
			echo "stopping OS-managed launchd agent $(LAUNCHD_LABEL)"; \
			launchctl bootout "gui/$$(id -u)/$(LAUNCHD_LABEL)" 2>/dev/null || launchctl unload "$(PLIST_DEST)" 2>/dev/null || true; \
			ACTED=1; \
			TIER1_ACTED=1; \
		fi; \
	elif [ "$$OS" = "Linux" ]; then \
		if systemctl --user is-active --quiet $(SYSTEMD_UNIT) 2>/dev/null; then \
			echo "stopping OS-managed systemd --user unit $(SYSTEMD_UNIT)"; \
			systemctl --user stop $(SYSTEMD_UNIT) 2>/dev/null || true; \
			ACTED=1; \
			TIER1_ACTED=1; \
		fi; \
	fi; \
	if [ -f "$(PIDFILE)" ]; then \
		if kill -0 "$$(cat $(PIDFILE))" 2>/dev/null; then \
			kill "$$(cat $(PIDFILE))" 2>/dev/null || true; \
			ACTED=1; \
		fi; \
		rm -f "$(PIDFILE)"; \
	fi; \
	TRIES=0; \
	while [ $$TRIES -lt 10 ]; do \
		SURVIVORS=$$(pgrep -x orchestratord 2>/dev/null); \
		if [ -z "$$SURVIVORS" ]; then \
			break; \
		fi; \
		echo "$$SURVIVORS" | xargs -r kill 2>/dev/null || true; \
		ACTED=1; \
		sleep 0.3; \
		TRIES=$$((TRIES + 1)); \
	done; \
	SURVIVORS=$$(pgrep -x orchestratord 2>/dev/null); \
	if [ -n "$$SURVIVORS" ]; then \
		echo "$$SURVIVORS" | xargs -r kill -9 2>/dev/null || true; \
		ACTED=1; \
		sleep 0.3; \
	fi; \
	SURVIVORS=$$(pgrep -x orchestratord 2>/dev/null); \
	if [ -n "$$SURVIVORS" ]; then \
		echo "FAILED to stop orchestratord -- still running (pid(s): $$SURVIVORS)"; \
		exit 1; \
	fi; \
	rm -f "$(PIDFILE)"; \
	if [ "$$ACTED" = "1" ]; then \
		echo "orchestratord stopped"; \
	else \
		echo "orchestratord not running"; \
	fi; \
	if [ "$$TIER1_ACTED" = "1" ]; then \
		echo "note: orchestratord was OS-registered and will not come back on its own -- re-register it with the OS service manager to restart it"; \
	fi

restart:
	$(MAKE) stop
	$(MAKE) start

status:
	@if [ -f "$(PIDFILE)" ] && kill -0 "$$(cat $(PIDFILE))" 2>/dev/null; then \
		echo "running (pid $$(cat $(PIDFILE)))"; \
	elif pgrep -x orchestratord >/dev/null 2>&1; then \
		echo "running (pid $$(pgrep -x orchestratord), untracked -- OS service or orphan)"; \
	else \
		echo "not running"; \
	fi

# Builds and packages a release archive containing all three binaries
# (orchestratord, orchestrator, orchestrator-tui) together -- they ship as
# one bundle because the CLI/TUI speak a WS protocol version tied to
# whatever daemon they were built alongside; releasing them independently
# risks a client/daemon version mismatch. This is the ONLY apps/*/Makefile
# with a `release:` target: that's what makes esumerfd/actions' shared
# release.yml discover exactly one releasable product ("orchestrator") for
# this repo, not three (see .github/workflows/release.yml).
#
# TARGET is the Rust target triple to build for (e.g. aarch64-apple-darwin,
# x86_64-apple-darwin) -- optional; omitted for a local run, which builds
# for the host's own native target (detected via `rustc -vV`). The shared
# workflow always passes it explicitly, one matrix leg per target.
#
# Version comes from this crate's own Cargo.toml: by the time this runs
# (checked out at the release tag in CI, or on whatever's committed
# locally) that manifest's version IS the version being packaged.
ASSET_DIR := dist

.PHONY: release
release:
	@set -e; \
	VERSION=$$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1); \
	if [ -z "$$VERSION" ]; then echo "release: could not read [package] version from Cargo.toml" >&2; exit 1; fi; \
	TRIPLE="$(TARGET)"; \
	if [ -z "$$TRIPLE" ]; then TRIPLE=$$(rustc -vV | sed -n 's/^host: //p'); fi; \
	echo "release: building orchestratord/orchestrator/orchestrator-tui $$VERSION for $$TRIPLE"; \
	cargo build --release --target "$$TRIPLE" --manifest-path Cargo.toml --target-dir target; \
	cargo build --release --target "$$TRIPLE" --manifest-path ../orchestrator-cli/Cargo.toml --target-dir ../orchestrator-cli/target; \
	cargo build --release --target "$$TRIPLE" --manifest-path ../orchestrator-tui/Cargo.toml --target-dir ../orchestrator-tui/target; \
	STAGE_DIR=$$(mktemp -d); \
	cp "target/$$TRIPLE/release/orchestratord" "$$STAGE_DIR/"; \
	cp "../orchestrator-cli/target/$$TRIPLE/release/orchestrator" "$$STAGE_DIR/"; \
	cp "../orchestrator-tui/target/$$TRIPLE/release/orchestrator-tui" "$$STAGE_DIR/"; \
	rm -rf $(ASSET_DIR); mkdir -p $(ASSET_DIR); \
	ASSET="voice-agent-v$${VERSION}-$${TRIPLE}.tar.gz"; \
	tar -czf "$(ASSET_DIR)/$$ASSET" -C "$$STAGE_DIR" orchestratord orchestrator orchestrator-tui; \
	shasum -a 256 "$(ASSET_DIR)/$$ASSET" | awk '{print $$1}' > "$(ASSET_DIR)/$$ASSET.sha256"; \
	rm -rf "$$STAGE_DIR"; \
	echo "release: wrote $(ASSET_DIR)/$$ASSET"
