//! On-disk JSONL rotation store (D-12): `std::fs` + `serde_json` only, no
//! new crate. One append-only file per UTC day
//! (`activities-YYYY-MM-DD.jsonl`), opened in append mode and flushed
//! after each write. `replay` rebuilds history from every file within the
//! retention window (oldest first); a malformed/truncated last line is
//! skipped with a warning rather than failing the whole file (Pitfall 6),
//! mirroring `registry::loader`'s skip-and-warn discipline for bad
//! workflow definitions. `prune` deletes files whose date is older than
//! the retention window.
//!
//! Dates are represented as `YYYY-MM-DD` filename suffixes computed via
//! Howard Hinnant's civil-calendar integer arithmetic
//! (<https://howardhinnant.github.io/date_algorithms.html>) directly over
//! `SystemTime`/`UNIX_EPOCH` -- deliberately avoiding a `chrono`/`time`
//! dependency for a value this module only ever formats/compares, never
//! displays with locale-aware formatting.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use shared::ActivityPhase;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// One persisted lifecycle transition -- enough to rebuild an
/// `ActivityEvent` by folding records grouped by `run_id`
/// (`ActivityRegistry::from_store`, Task 3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityRecord {
    pub run_id: String,
    pub workflow_id: String,
    pub client_name: String,
    pub phase: ActivityPhase,
    pub at_ms: u64,
    pub detail: Option<serde_json::Value>,
}

/// Appends `record` to today's (UTC) rotation file inside `dir`, creating
/// the directory and file as needed. Opened in append mode and flushed
/// after every write.
pub fn append(dir: &Path, record: &ActivityRecord) -> std::io::Result<()> {
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
/// warning (`eprintln!`), never propagated as a fatal error for the rest
/// of the file (Pitfall 6).
pub fn replay(dir: &Path, retention_days: u64) -> std::io::Result<Vec<ActivityRecord>> {
    let mut files = files_within_window(dir, retention_days)?;
    files.sort();

    let mut records = Vec::new();
    for path in files {
        let file = match fs::File::open(&path) {
            Ok(f) => f,
            Err(err) => {
                eprintln!("activity store: could not open {path:?} during replay: {err}, skipping");
                continue;
            }
        };
        for (line_no, line) in BufReader::new(file).lines().enumerate() {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    eprintln!(
                        "activity store: unreadable line {line_no} in {path:?}: {err}, skipping"
                    );
                    continue;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<ActivityRecord>(&line) {
                Ok(record) => records.push(record),
                Err(err) => {
                    eprintln!(
                        "activity store: skipping malformed/truncated line {line_no} in {path:?}: {err}"
                    );
                }
            }
        }
    }
    Ok(records)
}

/// Deletes every rotation file in `dir` whose date is older than
/// `retention_days` relative to today (UTC). Files within the window
/// (including today) are retained untouched. A missing `dir` is treated
/// as "nothing to prune", not an error.
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
    dir.join(format!("activities-{date}.jsonl"))
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
/// (`activities-YYYY-MM-DD.jsonl`), or `None` if `name` isn't one of ours.
fn parse_date_from_filename(name: &OsStr) -> Option<i64> {
    let name = name.to_str()?;
    let date_part = name.strip_prefix("activities-")?.strip_suffix(".jsonl")?;
    let mut parts = date_part.splitn(3, '-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None; // extra segment -- not a well-formed date
    }
    Some(days_from_civil(y, m, d))
}

/// Howard Hinnant's `civil_from_days`: converts a day count since the Unix
/// epoch into a proleptic-Gregorian `(year, month, day)` triple.
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

/// Howard Hinnant's `days_from_civil`: converts a proleptic-Gregorian
/// `(year, month, day)` triple into a day count since the Unix epoch.
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
            parse_date_from_filename(OsStr::new("activities-2020-01-01.jsonl")),
            Some(days_from_civil(2020, 1, 1))
        );
    }
}
