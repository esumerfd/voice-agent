//! On-disk JSONL rotation store for AI-agent run transitions (06-03,
//! SVC-03, G-07). Structurally mirrors `activity/store.rs`'s append/
//! replay/prune/day-bucketing pattern -- copied, not reused, per
//! 06-RESEARCH.md's "Pitfall 4: One JSONL store, two owners": this store
//! answers a different question ("what is one specific `claude` subprocess's
//! own state") than `ActivityRegistry` ("what did this WS client's request
//! do"), keyed by the handler's own `RunHandle` rather than the server-minted
//! `run_id`. One append-only file per UTC day
//! (`agent-runs-YYYY-MM-DD.jsonl`), opened in append mode and flushed after
//! each write -- the flush is what makes the log-before-memory ordering
//! invariant (G-07) meaningful in `handlers/ai_agent.rs`. A malformed/
//! truncated last line is skipped with a warning rather than failing the
//! whole file, mirroring `activity/store.rs`'s skip-and-warn discipline.
//! `prune` deletes files whose date is older than the retention window.
//!
//! Dates use the same Howard Hinnant civil-calendar integer arithmetic as
//! `activity/store.rs` (copied verbatim per 06-RESEARCH.md's "Don't
//! Hand-Roll" table), deliberately avoiding a `chrono`/`time` dependency.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// FNV-1a 64-bit offset basis / prime (see `prompt_digest`).
const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A single agent-run transition (06-03, AI-SPEC Section 7). `Started` is
/// recorded inside `invoke()` before the handle is returned; `Completed`/
/// `Failed` are recorded by the spawned task; `Interrupted` is never written
/// by `append` directly -- it is synthesized by `agent_log::replay_runs`
/// (Task 2) for a run whose last recorded phase was `Started` when the
/// daemon restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentRunPhase {
    Started,
    Completed,
    Failed,
    Interrupted,
}

/// One persisted AI-agent run transition -- everything the guardrails
/// (G-07) and offline metrics (06-AI-SPEC.md Section 7) need. **Never
/// written to this record:** the value of `ANTHROPIC_API_KEY`, the child
/// process's environment, or the full prompt text -- only a digest and a
/// byte count (`prompt_digest`/`prompt_bytes`) survive to disk.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentRunRecord {
    pub run_id: String,
    pub workflow_id: String,
    pub phase: AgentRunPhase,
    pub at_ms: u64,
    pub claude_version: Option<String>,
    pub model: Option<String>,
    pub max_budget_usd: Option<f64>,
    pub scratch_dir: Option<String>,
    /// Relative paths only -- never an absolute path, so this never leaks
    /// scratch-root layout beyond what `scratch_dir` already records.
    pub staged_files: Vec<String>,
    /// Hex-encoded FNV-1a 64-bit digest of the prompt bytes (see
    /// `prompt_digest`). 06-AI-SPEC.md Section 7 names this field
    /// `prompt_sha256`; it is renamed and re-based to FNV-1a here because
    /// this digest's job is reproducibility correlation across runs, not a
    /// security property -- adding a cryptographic hash crate would
    /// introduce this phase's only new dependency (and a
    /// package-legitimacy checkpoint) for no security benefit.
    /// The standard library's built-in `HashMap` hasher (SipHash-based) is
    /// deliberately NOT used here: its output is explicitly not stable
    /// across Rust releases, which would break correlation across a
    /// toolchain upgrade -- FNV-1a's output is stable by construction
    /// (fixed constants, no per-process random seed).
    pub prompt_digest: String,
    pub prompt_bytes: usize,
    /// The verbatim D-06 parameter block, kept for injection review (F-06).
    pub params: Option<serde_json::Value>,
    /// Subset of the CLI's result envelope: `is_error`, `subtype`,
    /// `terminal_reason`, `stop_reason`, `session_id`, `num_turns`,
    /// `duration_ms`, `duration_api_ms`, `total_cost_usd`, `usage`, and
    /// `permission_denials`, verbatim.
    pub envelope: Option<serde_json::Value>,
    pub detail: Option<String>,
}

/// Hex-encoded FNV-1a 64-bit digest of `bytes` (see `AgentRunRecord`'s
/// `prompt_digest` doc comment for why FNV-1a over a cryptographic hash).
/// Deterministic for identical input; a one-byte difference anywhere in
/// `bytes` changes the output.
pub fn prompt_digest(bytes: &[u8]) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= byte as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

/// Appends `record` to today's (UTC) rotation file inside `dir`, creating
/// the directory and file as needed. Opened in append mode and flushed
/// after every write -- callers (Task 3) rely on this flush completing
/// before the in-memory state map is updated (G-07).
pub fn append(dir: &Path, record: &AgentRunRecord) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    let path = file_path_for(dir, &day_bucket(SystemTime::now()));
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string(record)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    writeln!(file, "{line}")?;
    file.flush()
}

/// Replays every record from files within `retention_days` of "today"
/// (UTC), oldest first. A malformed/truncated line is skipped with a
/// warning (`eprintln!`), never propagated as a fatal error for the rest of
/// the file. A nonexistent `dir` returns an empty vector, not an error.
pub fn replay(dir: &Path, retention_days: u64) -> std::io::Result<Vec<AgentRunRecord>> {
    let mut files = files_within_window(dir, retention_days)?;
    files.sort();

    let mut records = Vec::new();
    for path in files {
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("agent log: could not open {path:?} during replay: {err}, skipping");
                continue;
            }
        };
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    eprintln!("agent log: unreadable line {line_no} in {path:?}: {err}, skipping");
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<AgentRunRecord>(&line) {
                Ok(record) => records.push(record),
                Err(err) => {
                    eprintln!(
                        "agent log: skipping malformed/truncated line {line_no} in {path:?}: {err}"
                    );
                }
            }
        }
    }
    Ok(records)
}

/// Deletes every rotation file in `dir` whose date is older than
/// `retention_days` relative to today (UTC). Files within the window
/// (including today) are retained untouched. A missing `dir` is treated as
/// "nothing to prune", not an error. A file not matching this store's
/// rotation naming is left untouched.
pub fn prune(dir: &Path, retention_days: u64) -> std::io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(()),
    };
    let today = day_bucket_index(SystemTime::now());
    for entry in entries {
        let entry = entry?;
        let Some(date) = parse_date_from_filename(&entry.file_name()) else {
            continue; // not one of our rotation files -- leave it alone
        };
        if !within_window(today, date, retention_days) {
            let _ = fs::remove_file(entry.path());
        }
    }
    Ok(())
}

fn files_within_window(dir: &Path, retention_days: u64) -> std::io::Result<Vec<PathBuf>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return Ok(Vec::new()),
    };
    let today = day_bucket_index(SystemTime::now());
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if let Some(date) = parse_date_from_filename(&entry.file_name()) {
            if within_window(today, date, retention_days) {
                files.push(entry.path());
            }
        }
    }
    Ok(files)
}

fn within_window(today: i64, date: i64, retention_days: u64) -> bool {
    today.saturating_sub(date) <= retention_days as i64
}

fn file_path_for(dir: &Path, date: &str) -> PathBuf {
    dir.join(format!("agent-runs-{date}.jsonl"))
}

/// Days since the Unix epoch (1970-01-01), per UTC, for `time`.
fn day_bucket_index(time: SystemTime) -> i64 {
    let secs = time.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    (secs / SECONDS_PER_DAY) as i64
}

/// Formats a day-bucket index as `YYYY-MM-DD` (UTC).
fn day_bucket(time: SystemTime) -> String {
    let (y, m, d) = civil_from_days(day_bucket_index(time));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Extracts the day-bucket index encoded in a rotation filename
/// (`agent-runs-YYYY-MM-DD.jsonl`), or `None` if `name` isn't one of ours.
fn parse_date_from_filename(name: &OsStr) -> Option<i64> {
    let name = name.to_str()?;
    let date_part = name.strip_prefix("agent-runs-")?.strip_suffix(".jsonl")?;
    let mut parts = date_part.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // extra segment -- not a well-formed date
    }
    Some(days_from_civil(y, m, d))
}

/// Howard Hinnant's `civil_from_days`, copied verbatim from
/// `activity/store.rs` (06-RESEARCH.md's "Don't Hand-Roll" table): converts
/// a day count since the Unix epoch into a proleptic-Gregorian `(year,
/// month, day)` triple.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Howard Hinnant's `days_from_civil`, copied verbatim from
/// `activity/store.rs`: converts a proleptic-Gregorian `(year, month, day)`
/// triple into a day count since the Unix epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64; // [0, 399]
    let month_index = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * month_index + 2) / 5 + d as u64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe as i64 - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_date_round_trips() {
        let cases = [
            (1970, 1, 1),
            (2020, 1, 1),
            (2026, 7, 13),
            (2000, 2, 29), // leap day
            (1969, 12, 31),
        ];
        for (y, m, d) in cases {
            let days = days_from_civil(y, m, d);
            let (ry, rm, rd) = civil_from_days(days);
            assert_eq!((ry, rm, rd), (y, m, d), "round-trip mismatch for {y}-{m}-{d}");
        }
    }

    #[test]
    fn parse_date_from_filename_rejects_non_rotation_files() {
        assert_eq!(parse_date_from_filename(OsStr::new("readme.txt")), None);
        assert_eq!(
            parse_date_from_filename(OsStr::new("agent-runs-2020-01-01.jsonl")),
            Some(days_from_civil(2020, 1, 1))
        );
    }
}
