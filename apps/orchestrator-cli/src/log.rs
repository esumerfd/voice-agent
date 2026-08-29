//! `log` subcommand entry point (quick task 260812-qeg).
//!
//! Deliberately daemon-independent: reading the daemon's log file must work
//! even when `orchestratord` is dead or was never started, which is exactly
//! when an operator most wants it. `main.rs` therefore dispatches
//! `Commands::Log` BEFORE `WsOrchestratorClient::connect` -- this module
//! never takes an `OrchestratorClient`, only filesystem paths.
//!
//! No `thiserror` (D-3 -- this crate has no such dependency and this quick
//! task adds none): `resolve_log_path` returns `Result<PathBuf, String>`
//! where the `String` is the finished, ready-to-print operator-facing
//! message.
//!
//! Default is one-shot (D-1): `run` prints the tail and returns; `--follow`
//! (Task 2) additionally streams appended bytes via `follow_from`.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// D-4 candidate resolution order when `--file` is absent: the dev
/// pidfile-backgrounded log first (an operator actively running `make
/// start` is the one most likely watching it), then the launchd stderr log
/// (`apps/orchestrator/packaging/macos/*.plist` `StandardErrorPath`) and, on
/// Linux, the same path once the systemd unit redirects to it (see
/// `apps/orchestrator/packaging/linux/orchestratord.service`).
pub fn default_candidates() -> Vec<PathBuf> {
    vec![
        PathBuf::from("/tmp/orchestratord.dev.log"),
        PathBuf::from("/tmp/orchestratord.err.log"),
    ]
}

/// Splits `content` into lines and returns the last `n`, borrowing from the
/// input. Uses `str::lines()`, which already drops the artifact of a
/// trailing newline (no empty trailing element) -- `n` of 0 or content of
/// `""` both yield an empty `Vec`.
pub fn tail_lines(content: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].to_vec()
}

/// Resolves the log file to read: `explicit` (from `--file`) wins outright
/// if given, otherwise the first existing path in `candidates` (D-4 order)
/// wins. On failure, the returned `String` is the finished D-5
/// operator-facing message -- every path tried, plus `make start` and the
/// Linux `journalctl` hint -- never a panic, never a silent empty success.
pub fn resolve_log_path(explicit: Option<&Path>, candidates: &[PathBuf]) -> Result<PathBuf, String> {
    let tried: Vec<PathBuf> = match explicit {
        Some(path) => vec![path.to_path_buf()],
        None => candidates.to_vec(),
    };

    for path in &tried {
        if path.exists() {
            return Ok(path.clone());
        }
    }

    Err(not_found_message(&tried))
}

fn not_found_message(tried: &[PathBuf]) -> String {
    let tried_list = tried
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no orchestratord log file found (tried: {tried_list}). Start the daemon with `make start`, or on Linux check `journalctl --user -u orchestratord.service -f`."
    )
}

/// Runs the `log` subcommand: resolves the file (`explicit` wins outright;
/// otherwise the first existing path in `candidates` -- `main.rs` passes
/// `default_candidates()` for real D-4 discovery, tests inject their own
/// list), reads it, writes the last `lines` lines to `out`, and returns the
/// process exit code (0 = success, 1 = resolution or read failure, D-5).
/// Never panics on a missing/unreadable file -- both are reported via
/// `eprintln!` to real stderr (mirroring how `list.rs` routes warnings
/// independently of the `Write` parameter) and returned as a nonzero code
/// instead.
///
/// When `follow` is set, after the one-shot tail is written and flushed
/// (D-1: a piped consumer sees it immediately, not at process exit), this
/// continues into `follow_from` starting at the file length just read, so
/// no line is duplicated or skipped between the two phases. The follow
/// phase polls until interrupted (`max_polls: None`) -- only exits via the
/// process being killed/Ctrl-C'd, matching `tail -f`.
pub async fn run<W: Write>(
    explicit: Option<&Path>,
    candidates: &[PathBuf],
    lines: usize,
    follow: bool,
    out: &mut W,
) -> std::io::Result<i32> {
    let path = match resolve_log_path(explicit, candidates) {
        Ok(path) => path,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(err) => {
            eprintln!("failed to read log file {}: {err}", path.display());
            return Ok(1);
        }
    };

    for line in tail_lines(&content, lines) {
        writeln!(out, "{line}")?;
    }
    out.flush()?;

    if follow {
        let offset = content.len() as u64;
        follow_from(&path, offset, out, Duration::from_millis(500), None).await?;
    }

    Ok(0)
}

/// Streams bytes appended to `path` past `offset`, tick by tick: seeks to
/// the current offset, reads whatever has been appended since, writes and
/// flushes it to `out`, advances the offset, then sleeps `poll` via
/// `tokio::time::sleep` (D-6: an injectable async sleep, not a real blocking
/// `tail -f`). `max_polls` of `None` loops until the process is
/// interrupted; `Some(n)` stops after exactly `n` polls, which is what
/// keeps tests deterministic instead of racing a real-time loop.
///
/// If the file shrinks underneath the loop (truncated or replaced -- e.g. a
/// daemon restart rewriting the log), the offset resets to 0 rather than
/// erroring, so the next poll picks up the new file's content from its
/// start instead of failing on a now-invalid seek target.
pub async fn follow_from<W: Write>(
    path: &Path,
    offset: u64,
    out: &mut W,
    poll: Duration,
    max_polls: Option<usize>,
) -> std::io::Result<()> {
    let mut offset = offset;
    let mut polls_done = 0usize;

    loop {
        let current_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let read_from = if current_len < offset { 0 } else { offset };

        if current_len > read_from {
            let mut file = std::fs::File::open(path)?;
            file.seek(SeekFrom::Start(read_from))?;
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)?;
            out.write_all(&buf)?;
            out.flush()?;
        }
        offset = current_len;

        polls_done += 1;
        if let Some(max) = max_polls {
            if polls_done >= max {
                break;
            }
        }
        tokio::time::sleep(poll).await;
    }

    Ok(())
}
