DAEMON_BIN_ABS = $(CURDIR)/apps/orchestrator/target/debug/orchestratord
PLIST_SRC   = apps/orchestrator/packaging/macos/com.wk-voice-agent.orchestratord.plist
PLIST_DEST  = $(HOME)/Library/LaunchAgents/com.wk-voice-agent.orchestratord.plist
SYSTEMD_SRC  = apps/orchestrator/packaging/linux/orchestratord.service
SYSTEMD_DEST = $(HOME)/.config/systemd/user/orchestratord.service
# XDG_DATA_HOME (falls back to the spec's default, matching every other
# XDG-aware tool) -- activity/agent-runs/agent-scratch land here instead of
# the daemon's own CWD-relative ./data default, so an OS-managed instance's
# data survives independently of wherever the repo checkout happens to live.
DATA_DIR = $(if $(XDG_DATA_HOME),$(XDG_DATA_HOME),$(HOME)/.local/share)/orchestratord
CLI_BIN_SRC  = apps/orchestrator-cli/target/debug/orchestrator
CLI_BIN_DEST = $(HOME)/bin/orchestrator
TUI_BIN_SRC  = apps/orchestrator-tui/target/debug/orchestrator-tui
TUI_BIN_DEST = $(HOME)/bin/orchestrator-tui

.DEFAULT_GOAL := help

.PHONY: test build run run-daemon run-tui start stop restart status build-daemon install uninstall install-cli uninstall-cli install-tui uninstall-tui help

help:
	@echo "Build/test:"
	@echo "  build         build all products (orchestrator, orchestrator-cli, orchestrator-tui, shared)"
	@echo "  test          test all products"
	@echo "  build-daemon  build only the orchestratord binary (delegates to apps/orchestrator)"
	@echo ""
	@echo "Foreground run (blocks the terminal):"
	@echo "  run           run the orchestrator CLI (delegates to apps/orchestrator-cli)"
	@echo "  run-daemon    run orchestratord in the foreground (delegates to apps/orchestrator)"
	@echo "  run-tui       run the orchestrator TUI client (delegates to apps/orchestrator-tui)"
	@echo ""
	@echo "Backgrounded orchestratord lifecycle (pidfile-managed, dev-session only):"
	@echo "  start         build-daemon, then launch orchestratord in the background"
	@echo "  stop          stop orchestratord however it was started"
	@echo "  restart       stop then start"
	@echo "  status        report whether the backgrounded orchestratord is running"
	@echo "  NOTE: start/restart/status remain ad-hoc pidfile-based dev backgrounding --"
	@echo "  they die on logout/reboot. stop also takes down an OS-managed instance; once"
	@echo "  it does, the daemon stays down until re-registered with the OS service manager."
	@echo ""
	@echo "OS-managed persistent service registration (survives reboot/logout):"
	@echo "  install       build-daemon, then register orchestratord with the OS service"
	@echo "                manager (launchd LaunchAgent on macOS, systemd --user on Linux);"
	@echo "                also builds and copies the orchestrator CLI and orchestrator-tui"
	@echo "                to $(HOME)/bin (puts the CLI and TUI on PATH)"
	@echo "                activity/agent-run/agent-scratch data is written to"
	@echo "                $(DATA_DIR) (XDG_DATA_HOME), not the repo checkout"
	@echo "  uninstall     unregister orchestratord from the OS service manager, remove"
	@echo "                the installed service file, and remove $(HOME)/bin/orchestrator"
	@echo "                and $(HOME)/bin/orchestrator-tui"
	@echo "  NOTE: install/uninstall are REAL OS-managed services, distinct from the"
	@echo "  pidfile-based start/stop/restart dev backgrounding above."
	@echo ""
	@echo "CLI-on-PATH install:"
	@echo "  install-cli   build the orchestrator CLI and copy it to $(HOME)/bin/orchestrator"
	@echo "                (puts the CLI on PATH)"
	@echo "  uninstall-cli remove $(HOME)/bin/orchestrator"
	@echo "  NOTE: install-cli/uninstall-cli are also available standalone if you want just"
	@echo "  the CLI without the daemon service -- install/uninstall above call these"
	@echo "  automatically as part of the full service install."
	@echo ""
	@echo "TUI-on-PATH install:"
	@echo "  install-tui   build the orchestrator TUI and copy it to $(HOME)/bin/orchestrator-tui"
	@echo "                (puts the TUI on PATH)"
	@echo "  uninstall-tui remove $(HOME)/bin/orchestrator-tui"
	@echo "  NOTE: install-tui/uninstall-tui are also available standalone if you want just"
	@echo "  the TUI without the daemon service -- install/uninstall above call these"
	@echo "  automatically as part of the full service install."

# shared gets its own explicit delegation (rather than relying purely on
# transitive builds via orchestrator/orchestrator-cli) because its 14
# relocated envelope tests must run under `make test` -- a transitive-only
# build would silently skip them (quick task 260712-nc9).
test:
	$(MAKE) -C apps/shared test
	$(MAKE) -C apps/orchestrator test
	$(MAKE) -C apps/orchestrator-cli test
	$(MAKE) -C apps/orchestrator-tui test

build:
	$(MAKE) -C apps/shared build
	$(MAKE) -C apps/orchestrator build
	$(MAKE) -C apps/orchestrator-cli build
	$(MAKE) -C apps/orchestrator-tui build

run:
	$(MAKE) -C apps/orchestrator-cli run

run-daemon:
	$(MAKE) -C apps/orchestrator run

run-tui:
	$(MAKE) -C apps/orchestrator-tui run

build-daemon:
	$(MAKE) -C apps/orchestrator build-daemon

start:
	$(MAKE) -C apps/orchestrator start

stop:
	$(MAKE) -C apps/orchestrator stop

restart:
	$(MAKE) -C apps/orchestrator restart

status:
	$(MAKE) -C apps/orchestrator status

install: build-daemon
	@OS=$$(uname -s); \
	if [ "$$OS" = "Darwin" ]; then \
		sed -e 's|__ORCHESTRATORD_BIN__|$(DAEMON_BIN_ABS)|' -e 's|__WORKDIR__|$(CURDIR)|' -e 's|__DATA_DIR__|$(DATA_DIR)|' "$(PLIST_SRC)" > "$(PLIST_DEST)"; \
		launchctl load "$(PLIST_DEST)"; \
		echo "orchestratord installed as a launchd LaunchAgent at $(PLIST_DEST)"; \
		echo "check with: launchctl list | grep com.wk-voice-agent.orchestratord"; \
	elif [ "$$OS" = "Linux" ]; then \
		mkdir -p "$(HOME)/.config/systemd/user"; \
		sed -e 's|__ORCHESTRATORD_BIN__|$(DAEMON_BIN_ABS)|' -e 's|__WORKDIR__|$(CURDIR)|' -e 's|__DATA_DIR__|$(DATA_DIR)|' "$(SYSTEMD_SRC)" > "$(SYSTEMD_DEST)"; \
		systemctl --user daemon-reload && systemctl --user enable --now orchestratord.service; \
		echo "orchestratord installed as a systemd --user service at $(SYSTEMD_DEST)"; \
		echo "check with: systemctl --user status orchestratord.service"; \
	else \
		echo "unsupported OS: $$OS"; \
		exit 1; \
	fi
	$(MAKE) install-cli
	$(MAKE) install-tui

uninstall:
	@OS=$$(uname -s); \
	if [ "$$OS" = "Darwin" ]; then \
		launchctl unload "$(PLIST_DEST)" 2>/dev/null || true; \
		rm -f "$(PLIST_DEST)"; \
		echo "orchestratord LaunchAgent unloaded and removed"; \
	elif [ "$$OS" = "Linux" ]; then \
		systemctl --user disable --now orchestratord.service 2>/dev/null || true; \
		rm -f "$(SYSTEMD_DEST)"; \
		echo "orchestratord systemd --user service disabled and removed"; \
	else \
		echo "unsupported OS: $$OS"; \
		exit 1; \
	fi
	$(MAKE) uninstall-cli
	$(MAKE) uninstall-tui

install-cli:
	$(MAKE) -C apps/orchestrator-cli build
	@mkdir -p $(HOME)/bin
	@cp $(CLI_BIN_SRC) $(CLI_BIN_DEST)
	@echo "orchestrator CLI installed to $(CLI_BIN_DEST)"
	@echo "if your shell already cached a different 'orchestrator' on PATH, run: hash -r  (or open a new shell)"

uninstall-cli:
	@rm -f $(CLI_BIN_DEST)
	@echo "orchestrator CLI removed from $(CLI_BIN_DEST)"

install-tui:
	$(MAKE) -C apps/orchestrator-tui build
	@mkdir -p $(HOME)/bin
	@cp $(TUI_BIN_SRC) $(TUI_BIN_DEST)
	@echo "orchestrator TUI installed to $(TUI_BIN_DEST)"
	@echo "if your shell already cached a different 'orchestrator-tui' on PATH, run: hash -r  (or open a new shell)"

uninstall-tui:
	@rm -f $(TUI_BIN_DEST)
	@echo "orchestrator TUI removed from $(TUI_BIN_DEST)"
