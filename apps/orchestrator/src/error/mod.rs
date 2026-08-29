//! Load-time and dispatch-time error types (D-07 skip-and-warn, D-08
//! required fields).
//!
//! Every variant names the offending file path or param/handler, per
//! D-07/D-08.

use std::path::PathBuf;

use thiserror::Error;

use crate::definition::ParameterType;

#[derive(Error, Debug, PartialEq)]
pub enum LoadError {
    #[error("{path}: IO error: {detail}")]
    Io { path: PathBuf, detail: String },

    #[error("{path}: missing required field `{field}`")]
    MissingField { path: PathBuf, field: String },

    #[error("{path}: no YAML frontmatter found (expected --- delimiters)")]
    NoFrontmatter { path: PathBuf },

    #[error("{path}: YAML parse error: {detail}")]
    ParseError { path: PathBuf, detail: String },

    #[error("{path}: duplicate workflow id `{id}` (already loaded from another file)")]
    DuplicateId { path: PathBuf, id: String },

    #[error("{path}: unknown parameter type `{value}` (expected one of string, int, bool)")]
    UnknownParameterType { path: PathBuf, value: String },

    #[error("{path}: unknown service.mode `{value}` (expected sync or async)")]
    UnknownServiceMode { path: PathBuf, value: String },

    #[error(
        "{path}: workflow declares both `service.command` and `service.agent`; a workflow binds to exactly one"
    )]
    ConflictingServiceBinding { path: PathBuf },
}

/// AI-agent runtime failures (06-01, SVC-03, D-02). `RuntimeError` lives
/// here, not in `runtime/mod.rs`, because every error enum in this crate
/// lives in `error/mod.rs` (a deliberate deviation from 06-RESEARCH.md
/// Pattern 1's sketch, in favour of the in-repo convention). Every variant
/// names the offending binary/path, matching this module's documented
/// style. 06-02/06-04 add variants (envelope-parse retry, staging
/// containment).
#[derive(Error, Debug, Clone, PartialEq)]
pub enum RuntimeError {
    #[error("failed to spawn `{binary}`: {detail}")]
    Spawn { binary: PathBuf, detail: String },

    #[error("invalid agent envelope: {detail}")]
    Envelope { detail: String },

    #[error("{path}: IO error: {detail}")]
    Io { path: PathBuf, detail: String },

    /// D-03/E-03/G-03 staging containment (06-02): a declared file
    /// dependency's source resolved (after canonicalize, following
    /// symlinks) outside the workflow `.md` file's own directory, or its
    /// staged destination resolved outside the per-run scratch directory.
    /// `reason` names which end of the boundary was crossed.
    #[error("{path}: staged dependency resolves outside its allowed directory ({reason})")]
    PathEscape { path: PathBuf, reason: String },

    /// D-03 staging (06-02): a declared file dependency could not be
    /// staged (missing source, canonicalize failure, copy failure) --
    /// always a loud failure, never a silent skip.
    #[error("{path}: could not stage dependency: {detail}")]
    Staging { path: PathBuf, detail: String },

    /// G-01 (06-04): the child exceeded its wall-clock ceiling and was
    /// killed (`kill_on_drop`, `runtime/local.rs`). The sole defence
    /// against Failure Mode #1 -- `claude` exposes no CLI-level timeout or
    /// turn cap, so this handler must own the ceiling.
    #[error("agent run exceeded its {seconds}s ceiling and was terminated")]
    Timeout { seconds: u64 },
}

/// Payload-validation failures (D-07): a JSON payload checked against a
/// workflow's `ParameterSpec` before any handler runs.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    #[error("missing required parameter `{param}`")]
    MissingRequired { param: String },

    #[error("parameter `{param}` has the wrong type (expected {expected:?})")]
    WrongType {
        param: String,
        expected: ParameterType,
    },
}

/// Dispatch-time failures (ORCH-01): either the payload failed validation
/// or the workflow's declared handler could not be resolved.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    #[error("no handler registered for `{handler}`")]
    HandlerNotFound { handler: String },

    #[error("payload validation failed: {0}")]
    Validation(#[from] ValidationError),
}

/// `registry::writer::create_workflow` failures (quick task 260807-shx,
/// T-shx-01/T-shx-04/T-shx-05): every variant names the offending id, value,
/// or path, mirroring `LoadError`'s existing message style. Never
/// constructed from a partially-written state -- every variant here means
/// nothing (or nothing new) was written to disk.
#[derive(Error, Debug, PartialEq)]
pub enum CreateError {
    #[error("invalid workflow id `{id}`: {reason}")]
    InvalidId { id: String, reason: String },

    #[error("invalid workflow name: {reason}")]
    InvalidName { reason: String },

    #[error("content too large: {len} bytes exceeds the {max} byte limit")]
    ContentTooLarge { len: usize, max: usize },

    #[error("workflow `{id}` already exists at {path}")]
    AlreadyExists { id: String, path: PathBuf },

    /// Edit mode's inverse guard (quick task 260812-qpn, T-qpn-02): `id`
    /// does not resolve to an existing workflow, so there is nothing to
    /// overwrite. `path` names where the daemon looked (`<id>.md` under its
    /// workflows dir); the message points the reader at `workflow create`
    /// as the way to add a new definition, mirroring `AlreadyExists`'s
    /// "name the id and the path" style in the opposite direction.
    #[error("workflow `{id}` not found at {path} (use `workflow create` to add a new definition)")]
    NotFound { id: String, path: PathBuf },

    #[error("{path}: IO error: {detail}")]
    Io { path: PathBuf, detail: String },

    /// Quick task 260813-rm5, D-6: a request declared BOTH a script and an
    /// agent block — a workflow binds to exactly one handler shape. This is
    /// the write-side statement of the same rule `LoadError::
    /// ConflictingServiceBinding` already enforces at load time; the writer
    /// must never emit a `.md` the loader would reject. `id` names the
    /// offending request, mirroring this enum's other "name the id" variants.
    #[error("workflow `{id}` declares both a script and an agent block; a workflow binds to exactly one handler shape")]
    ConflictingServiceBinding { id: String },

    /// Quick task 260813-rm5, D-7: an `agent.files` entry failed shape/size
    /// validation (empty, contains a NUL byte, over the per-entry length
    /// cap, or the list is over the count cap) — checked ahead of any
    /// filesystem call, worded after `InvalidName` above.
    #[error("invalid agent file entry: {reason}")]
    InvalidAgentFile { reason: String },
}

/// `registry::writer::delete_workflow` failures (quick task 260812-qp4,
/// D-2/D-6): every variant means nothing was removed. `NotFound` mirrors
/// `CreateError::AlreadyExists`'s "refuse loudly, name the id" posture in
/// the opposite direction (D-2 -- delete is not idempotent). Task 2 adds a
/// `PathEscape` variant reusing `RuntimeError::PathEscape`'s wording pattern
/// (names the offending path and which end of the boundary was crossed)
/// without reusing the type itself, since this enum's contract is "nothing
/// was removed", distinct from `RuntimeError`'s staging-time contract.
#[derive(Error, Debug, PartialEq)]
pub enum DeleteError {
    #[error("workflow `{id}` not found")]
    NotFound { id: String },

    /// T-qp4-02 (Task 2): a resolved removal candidate (the `.md` or its
    /// declared `service.command` script) canonicalized to a path outside
    /// the daemon's own workflows dir. Reuses `RuntimeError::PathEscape`'s
    /// wording pattern (names the offending path and which end of the
    /// boundary was crossed) without reusing the type itself.
    #[error("{path}: staged removal candidate resolves outside its allowed directory ({reason})")]
    PathEscape { path: PathBuf, reason: String },

    #[error("{path}: IO error: {detail}")]
    Io { path: PathBuf, detail: String },
}

/// WS transport frame-decode/routing failures (D-03, ASVS V5 input
/// validation, T-04-01) -- never surfaces as a panic. `server/mod.rs` uses
/// these internally to build a best-effort error `res` describing what went
/// wrong on that one connection; the accept loop and every other connection
/// continue unaffected.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum ServerError {
    #[error("malformed frame: {detail}")]
    MalformedFrame { detail: String },

    #[error("unexpected frame type from client: {got}")]
    UnexpectedFrameType { got: String },

    #[error("empty frame")]
    EmptyFrame,
}
