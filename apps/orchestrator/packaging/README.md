# Packaging: orchestratord as a background service

This directory contains ready-to-install OS service definitions for
`orchestratord`, the orchestrator daemon (D-05). Copy the file for your
platform, edit the five placeholders, then register it with your OS service
manager.

**`make install` (repo root Makefile) only fills in `__ORCHESTRATORD_BIN__`,
`__WORKDIR__`, and `__DATA_DIR__`** — it does not touch `__CLAUDE_BIN__` or
`__ANTHROPIC_API_KEY__`. If you want the AI-agent handler registered under
an OS-managed `make install`, edit those two placeholders directly in the
SOURCE file under this directory (`apps/orchestrator/packaging/macos/...
.plist` or `apps/orchestrator/packaging/linux/orchestratord.service`)
*before* running `make install`, since it copies, substitutes, and loads
the service in one step with no pause to hand-edit the copy. Otherwise,
run `make install` as-is and the daemon starts with every other handler
working normally, printing one `ERROR` line at boot noting that
`agent.claude` was not registered.

## Placeholders you must edit

Both service files use five placeholders — replace them before installing:

- `__ORCHESTRATORD_BIN__` — absolute path to your built `orchestratord`
  binary (e.g. `/Users/you/wk-voice-agent/apps/orchestrator/target/debug/orchestratord`).
- `__WORKDIR__` — absolute path to the directory whose `workflows/`
  subdirectory the daemon should scan (e.g. `/Users/you/wk-voice-agent`).
- `__DATA_DIR__` — base directory for the daemon's activity history,
  agent-run log, and agent-scratch space (`ORCHESTRATOR_ACTIVITY_DIR`/
  `ORCHESTRATOR_AGENT_RUNS_DIR`/`ORCHESTRATOR_AGENT_SCRATCH_DIR`, each
  suffixed `/activity`, `/agent-runs`, `/agent-scratch`). `make install`
  fills this in as `$XDG_DATA_HOME/orchestratord` (falling back to
  `~/.local/share/orchestratord` per the XDG Base Directory spec) instead
  of the daemon's own CWD-relative `./data` default, so an OS-managed
  instance's data survives independently of the repo checkout. No need to
  exist ahead of time — each store is created lazily on first write.
- `__CLAUDE_BIN__` — absolute path to the `claude` binary the AI-agent
  handler (`agent.claude`, 06-05) spawns. Resolve it with `which claude` on
  the machine that will run the service, and paste the absolute path it
  prints — **do not** leave this as a bare `claude`. A background service
  does not inherit an interactive shell's `PATH` (on this project's own dev
  machine, launchd's is empty), so a bare name will fail to resolve at
  daemon startup.
- `__ANTHROPIC_API_KEY__` — your Anthropic API key
  (console.anthropic.com -> Settings -> API keys). Must be **non-empty**: a
  present-but-empty value fails almost instantly with a login-shaped error
  that is easy to misdiagnose as a bad install. If you don't want to use
  the AI-agent handler yet, leave this placeholder unedited (or any
  obviously-invalid value) — `orchestratord` refuses to register that one
  handler and prints a loud `ERROR` line naming why, but the rest of the
  daemon (workflow list/run for every other service type) starts and serves
  normally.

**Placing a secret in a service definition file means it inherits that
file's permissions.** `~/Library/LaunchAgents/*.plist` and
`~/.config/systemd/user/*.service` are typically readable only by your own
user account, but this is a per-installer responsibility, not something
`orchestratord` enforces — restrict the installed file's permissions
yourself (e.g. `chmod 600`), or provision the key through a different
mechanism (a wrapper script that exports it before exec'ing the daemon, a
secrets manager, etc.) if your threat model requires it. This project
accepts the file-permission risk for a single local user (06-05's threat
register, T-06-21); a secrets manager integration is out of scope for this
milestone.

## macOS (launchd)

Install:

```bash
cp apps/orchestrator/packaging/macos/com.wk-voice-agent.orchestratord.plist ~/Library/LaunchAgents/
# edit the placeholders in the copied file, then:
launchctl load ~/Library/LaunchAgents/com.wk-voice-agent.orchestratord.plist
```

Uninstall:

```bash
launchctl unload ~/Library/LaunchAgents/com.wk-voice-agent.orchestratord.plist
rm ~/Library/LaunchAgents/com.wk-voice-agent.orchestratord.plist
```

Logs are written to `/tmp/orchestratord.out.log` and
`/tmp/orchestratord.err.log`.

## Linux (systemd --user)

Install:

```bash
mkdir -p ~/.config/systemd/user
cp apps/orchestrator/packaging/linux/orchestratord.service ~/.config/systemd/user/
# edit the placeholders in the copied file, then:
systemctl --user enable --now orchestratord.service
```

Uninstall:

```bash
systemctl --user disable --now orchestratord.service
rm ~/.config/systemd/user/orchestratord.service
```

## Loopback-only reachability

Both service files start `orchestratord` bound to `127.0.0.1` on the
default port (`47100`) — neither file passes a bind-address override, so
the daemon is reachable only from the local host. This matches the
project's accepted-risk, single-local-user posture (no auth is introduced
by this service registration).
