//! Fail-loud AI-agent startup preflight (06-05, SVC-03, 06-AI-SPEC.md
//! guardrail G-02). Extracted into the library, not left inline in
//! `bin/orchestratord.rs`, for one specific reason: an integration test
//! under this crate's `tests/` directory links only the `orchestrator`
//! library crate, never the `orchestratord` binary crate -- a function this
//! important must live here to be exercised by
//! `tests/agent_startup_integration.rs` without spawning a real daemon
//! process.
//!
//! `preflight_agent` is pure: it never spawns a child, never touches the
//! network, and never prints anything. It only decides whether the daemon
//! MAY register the AI-agent handler, and if not, explains exactly which
//! precondition failed so `orchestratord::main` can print one loud,
//! precondition-naming line and continue starting without that handler
//! (never a half-working handler, never a boot failure).

use std::path::{Path, PathBuf};

/// Resolves `claude_bin` to an absolute, executable path and validates that
/// `api_key` is present and non-empty (after trimming), or returns a
/// `String` naming exactly which precondition failed.
///
/// Binary resolution order:
///   1. `claude_bin` is already absolute -> used as-is.
///   2. `claude_bin` is relative but contains more than one path component
///      (e.g. `./claude`, `bin/claude`) -> joined onto the current working
///      directory. Never passed through relative.
///   3. `claude_bin` is a bare name (e.g. `claude`) -> walked against every
///      `PATH` entry, taking the first executable match. Never spawned as
///      a bare name if this fails -- refused instead.
///
/// Whichever branch resolves the candidate, the result is then checked to
/// exist and have the executable bit set
/// (`std::os::unix::fs::PermissionsExt`).
///
/// The API key check is independent of binary resolution and produces a
/// message distinguishable from every binary-resolution failure -- an
/// unset key names the `ANTHROPIC_API_KEY` variable; a present-but-EMPTY
/// key (after trimming) is called out explicitly, because with the
/// `--bare` authentication mode this is the dangerous case: the CLI reads
/// no keychain or subscription credential and fails in about 150ms with a
/// login-shaped message that is easy to misdiagnose as a bad install
/// (06-AI-SPEC.md Pitfall 3).
pub fn preflight_agent(claude_bin: &Path, api_key: Option<&str>) -> Result<PathBuf, String> {
    let resolved = resolve_claude_bin(claude_bin)?;

    match api_key {
        None => {
            return Err(
                "ANTHROPIC_API_KEY is not set -- the AI-agent handler will not be registered"
                    .to_string(),
            )
        }
        Some(key) if key.trim().is_empty() => {
            return Err(
                "ANTHROPIC_API_KEY is set but EMPTY -- the AI-agent handler will not be \
                 registered (an empty key fails almost instantly with a login-shaped error \
                 that is easy to misdiagnose as a bad install)"
                    .to_string(),
            )
        }
        Some(_) => {}
    }

    Ok(resolved)
}

/// Resolution logic for `claude_bin`, split out of `preflight_agent` purely
/// for readability. See `preflight_agent`'s doc comment for the resolution
/// order this follows.
fn resolve_claude_bin(claude_bin: &Path) -> Result<PathBuf, String> {
    if claude_bin.is_absolute() {
        return check_executable(claude_bin);
    }

    // More than one path component means the caller supplied a separator
    // (e.g. `./claude`, `bin/claude`, `../claude`) even though the whole
    // path is relative -- joined onto the current working directory, never
    // silently passed through relative.
    if claude_bin.components().count() > 1 {
        let cwd = std::env::current_dir()
            .map_err(|e| format!("could not resolve the current directory: {e}"))?;
        return check_executable(&cwd.join(claude_bin));
    }

    // A bare name -- walk PATH entries, take the first executable match.
    // Never spawned/returned as a bare name if this fails.
    resolve_from_path(claude_bin)
}

/// Walks every `PATH` entry (in order) looking for an executable file named
/// `name`, returning the first match as an absolute path. Refuses (never
/// passes `name` through unresolved) when `PATH` is unset or no entry
/// contains an executable match.
fn resolve_from_path(name: &Path) -> Result<PathBuf, String> {
    let path_var = std::env::var_os("PATH").ok_or_else(|| {
        format!(
            "`{}` is a bare binary name and PATH is not set -- cannot resolve it to an \
             absolute path; set --claude-bin / ORCHESTRATOR_CLAUDE_BIN to an absolute path instead",
            name.display()
        )
    })?;

    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            // A PATH entry is conventionally absolute, but is not
            // guaranteed to be -- fall back to joining onto the current
            // directory so every returned path is genuinely absolute
            // (never merely "absolute-looking").
            if candidate.is_absolute() {
                return Ok(candidate);
            }
            if let Ok(cwd) = std::env::current_dir() {
                return Ok(cwd.join(candidate));
            }
            return Ok(candidate);
        }
    }

    Err(format!(
        "could not resolve `{}` against PATH -- set --claude-bin / ORCHESTRATOR_CLAUDE_BIN to \
         an absolute path",
        name.display()
    ))
}

/// Checks that `path` exists and has the executable bit set, naming `path`
/// in either failure message. Never follows a resolution fallback -- a
/// caller that wants PATH resolution must go through `resolve_from_path`.
fn check_executable(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Err(format!("claude binary does not exist at `{}`", path.display()));
    }
    if !is_executable(path) {
        return Err(format!("claude binary at `{}` is not executable", path.display()));
    }
    Ok(path.to_path_buf())
}

/// A path is "executable" here when it is a regular file (never a
/// directory) whose mode has at least one executable bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
        .unwrap_or(false)
}
