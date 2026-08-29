//! `orchestratord` binary entry point (D-01/D-02/D-06): the long-lived
//! daemon that owns the workflow registry and the real `Service` handler
//! registry, serving the D-03 WS envelope over a loopback TCP listener.
//!
//! Multi-thread runtime (Pitfall 5): unlike `orchestrator-cli/src/main.rs`'s
//! deliberate `current_thread` choice (a short-lived, low-concurrency single
//! invocation), this daemon services concurrent WS connections PLUS
//! background `tokio::spawn`'d async-mode dispatch tasks (D-08/D-09) running
//! alongside request handling for the process's entire lifetime -- a
//! conscious choice for this workload shape, not a copy-paste of the CLI's
//! runtime flavor.
//!
//! Phase 5 (plan 05-05): `server::serve` now also takes an `ActivityRegistry`
//! (05-03). Startup wires the registry from a durable 7-day on-disk store
//! (D-12) via `ActivityRegistry::from_store` + `activity::store::prune`, so
//! a restarted daemon rebuilds its activity history from disk instead of
//! starting with an empty in-memory registry.
//!
//! 06-05 (SVC-03, AI-SPEC G-02): the AI-agent handler (`agent.claude`) is
//! registered here, last of the four handlers, gated behind
//! `orchestrator::startup::preflight_agent`. Three new explicit flags
//! (`--claude-bin`/`--agent-runs-dir`/`--agent-scratch-dir`, each with an
//! `env = "..."` fallback and a fixed default, matching `--workflows-dir`/
//! `--activity-dir`'s shape exactly) mean every path this handler touches
//! is reachable by flag or environment variable -- never a new implicit
//! working-directory-relative resolution, per this project's own
//! three-strikes history with that failure mode. A failed preflight prints
//! one loud `ERROR` line naming the failed precondition and the daemon
//! keeps starting WITHOUT that handler -- never a boot failure, never a
//! half-working handler that fails per-run instead of at startup.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use orchestrator::activity::ActivityRegistry;
use orchestrator::handlers::ai_agent::{AiAgentService, AGENT_HANDLER_KEY};
use orchestrator::runtime::local::LocalRuntime;
use orchestrator::runtime::AgentRuntime;
use orchestrator::startup::preflight_agent;
use orchestrator::{InProcessOrchestrator, Service};

/// `orchestratord`'s own CLI surface -- distinct from `orchestrator-cli`'s
/// `Cli` struct; D-06 moves registry/handler ownership here.
#[derive(Parser)]
#[command(name = "orchestratord")]
struct Cli {
    /// WS listener port (D-02: flag-only override of `DEFAULT_PORT`).
    /// Deliberately NO `env = "..."` attribute here -- unlike
    /// `--workflows-dir` below, D-02 requires the daemon's bind port to
    /// never be silently overridden by an inherited environment variable.
    #[arg(long, default_value_t = orchestrator::DEFAULT_PORT)]
    port: u16,

    /// Directory to scan for workflow definitions. The registry now lives
    /// with the daemon, not the CLI (D-06).
    #[arg(long, env = "ORCHESTRATOR_WORKFLOWS_DIR", default_value = "./workflows")]
    workflows_dir: PathBuf,

    /// Directory for the durable activity-history JSONL rotation store
    /// (D-12). `ActivityRegistry::from_store` rebuilds the last
    /// `ACTIVITY_RETENTION_DAYS` of history from here on every startup.
    #[arg(long, env = "ORCHESTRATOR_ACTIVITY_DIR", default_value = "./data/activity")]
    activity_dir: PathBuf,

    /// Path to the `claude` binary the AI-agent handler spawns (06-05,
    /// SVC-03). The bare `claude` default is only a resolution SEED for
    /// `preflight_agent` -- it is never spawned as a bare name. A
    /// background service does not inherit an interactive shell's PATH
    /// (on this machine, launchd's is empty), so production deployments
    /// should set this to an absolute path via `--claude-bin` or this env
    /// var, resolved once from `which claude` on the daemon's host.
    #[arg(long, env = "ORCHESTRATOR_CLAUDE_BIN", default_value = "claude")]
    claude_bin: PathBuf,

    /// Directory the AI-agent handler's durable JSONL run log is written
    /// into (06-03/06-05, G-07). Mirrors `--activity-dir`'s shape exactly
    /// -- an explicit flag/env with a fixed default, never CWD-relative by
    /// implication.
    #[arg(long, env = "ORCHESTRATOR_AGENT_RUNS_DIR", default_value = "./data/agent-runs")]
    agent_runs_dir: PathBuf,

    /// Parent directory the AI-agent handler's per-run scratch directories
    /// are allocated inside (06-01/06-05).
    #[arg(long, env = "ORCHESTRATOR_AGENT_SCRATCH_DIR", default_value = "./data/agent-scratch")]
    agent_scratch_dir: PathBuf,
}

/// How many days of on-disk activity history are rebuilt/retained across a
/// restart (D-12). Applied both to `ActivityRegistry::from_store` (rebuild)
/// and `activity::store::prune` (startup cleanup) so the two always agree.
const ACTIVITY_RETENTION_DAYS: u64 = 7;

/// Resolves `path` to an absolute, canonicalized form for a startup log
/// line (WR-01, 06-REVIEW.md) -- makes an unexpectedly-resolved daemon CWD
/// (e.g. a systemd/launchd unit with a missing or wrong `WorkingDirectory=`)
/// visible immediately at startup, rather than only discoverable later as
/// run logs/scratch data silently scattered under the wrong directory.
/// Falls back to the original (possibly relative) `path` unresolved when
/// canonicalize fails -- most commonly because the directory does not
/// exist YET: `agent_log::store::append`/`LocalRuntime::prepare` create it
/// lazily on first write, so a fresh install must never fail startup, or
/// even print a misleading resolution, over this.
fn resolve_path_for_display(path: &PathBuf) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.clone())
}

/// Runs a synchronous, blocking closure on tokio's dedicated blocking
/// thread pool via `tokio::task::spawn_blocking` (WR-02, 06-REVIEW.md) --
/// startup's synchronous filesystem calls (`agent_log::store::prune`,
/// `LocalRuntime::prune_scratch`) must never run directly on an async
/// worker thread, the same anti-pattern CR-01 fixed for the `--version`
/// subprocess capture. Panics if the blocking closure itself panics
/// (mirrors `spawn_blocking`'s own panic-propagation contract) -- a
/// panicking prune sweep should surface loudly, not be silently absorbed.
async fn run_blocking_io<F>(f: F) -> std::io::Result<()>
where
    F: FnOnce() -> std::io::Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.expect("blocking startup I/O task panicked")
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    // Real Service handler registration (Pattern 3), relocated verbatim from
    // `orchestrator-cli/src/main.rs` per D-06 -- the daemon is now the sole
    // place any real `Service` impl is wired in.
    let mut handlers: HashMap<String, Box<dyn Service>> = HashMap::new();
    handlers.insert(
        "timers.start".to_string(),
        Box::new(orchestrator::handlers::timers::TimerService::new()),
    );
    let calendar_script = std::env::var("ORCHESTRATOR_CALENDAR_SCRIPT")
        .unwrap_or_else(|_| "workflows/scripts/calendar_today.sh".to_string());
    handlers.insert(
        "process.calendar_today".to_string(),
        Box::new(orchestrator::handlers::process::ProcessService::new(
            calendar_script,
            Vec::new(),
        )),
    );
    // Generic "run now" fallback (quick task 260720-r7n): keyed off the SAME
    // DEFAULT_HANDLER constant the loader defaults an absent/empty
    // service.handler to, so the defaulted key and the registered key can
    // never drift apart.
    handlers.insert(
        orchestrator::definition::DEFAULT_HANDLER.to_string(),
        Box::new(orchestrator::handlers::immediate::ImmediateService::new()),
    );

    // Prune stale rotation files before rebuilding (D-12) so from_store's
    // replay never folds in history already outside the retention window.
    orchestrator::activity::store::prune(&cli.activity_dir, ACTIVITY_RETENTION_DAYS)?;
    let activity_registry = Arc::new(ActivityRegistry::from_store(
        &cli.activity_dir,
        ACTIVITY_RETENTION_DAYS,
    )?);

    // AI-agent handler startup sequence (06-05, SVC-03) -- mirrors the
    // activity-store sequence directly above: prune both of this handler's
    // own stores first, THEN validate preconditions, THEN register (or
    // refuse loudly and continue without it). Must run BEFORE
    // `InProcessOrchestrator::with_handlers` below, since that call moves
    // `handlers` -- this is the last point the agent handler can still be
    // inserted into the map.
    //
    // WR-01 (06-REVIEW.md): unlike `--claude-bin`/`ANTHROPIC_API_KEY`,
    // `--agent-runs-dir`/`--agent-scratch-dir` are never validated or
    // resolved to an absolute path -- they are used as-is (CWD-relative
    // defaults). Logging the fully resolved path here makes a wrong daemon
    // CWD (e.g. a systemd/launchd unit with a missing `WorkingDirectory=`)
    // visible immediately at startup instead of only discoverable later as
    // scattered run logs/scratch data.
    eprintln!(
        "orchestratord: agent runs dir resolved to {:?}",
        resolve_path_for_display(&cli.agent_runs_dir)
    );
    eprintln!(
        "orchestratord: agent scratch dir resolved to {:?}",
        resolve_path_for_display(&cli.agent_scratch_dir)
    );
    // WR-02 (06-REVIEW.md): both prune sweeps are synchronous filesystem
    // walks -- run via `run_blocking_io` (tokio::task::spawn_blocking) so
    // daemon startup never ties up an async worker thread with them, the
    // same discipline CR-01 already applies to the `--version` subprocess
    // capture.
    let agent_runs_dir_for_prune = cli.agent_runs_dir.clone();
    run_blocking_io(move || {
        orchestrator::agent_log::store::prune(
            &agent_runs_dir_for_prune,
            orchestrator::agent_log::AGENT_LOG_RETENTION_DAYS,
        )
    })
    .await?;
    let agent_scratch_dir_for_prune = cli.agent_scratch_dir.clone();
    run_blocking_io(move || {
        LocalRuntime::prune_scratch(
            &agent_scratch_dir_for_prune,
            orchestrator::runtime::local::SCRATCH_RETAIN_HOURS,
        )
    })
    .await?;

    // Read once, here, never logged, never interpolated into any output
    // line at any severity (T-06-20) -- `preflight_agent` only ever sees a
    // borrowed `&str`, and the only thing derived from it below is a
    // Some/None-shaped ERROR line naming the PRECONDITION, never the value.
    let api_key = std::env::var("ANTHROPIC_API_KEY").ok();
    match preflight_agent(&cli.claude_bin, api_key.as_deref()) {
        Ok(resolved_claude_bin) => {
            let runtime: Arc<dyn AgentRuntime> = Arc::new(
                LocalRuntime::new(
                    resolved_claude_bin,
                    api_key.expect("preflight_agent only succeeds with a validated, non-empty key"),
                    cli.agent_scratch_dir.clone(),
                )
                .await,
            );
            let agent_service = AiAgentService::from_store(
                runtime,
                &cli.agent_runs_dir,
                orchestrator::agent_log::AGENT_LOG_RETENTION_DAYS,
            )?;
            handlers.insert(AGENT_HANDLER_KEY.to_string(), Box::new(agent_service));
        }
        Err(reason) => {
            eprintln!(
                "ERROR: AI-agent handler ({AGENT_HANDLER_KEY}) not registered -- {reason}"
            );
        }
    }

    let orchestrator = Arc::new(InProcessOrchestrator::with_handlers(
        &cli.workflows_dir,
        handlers,
    ));

    // Loopback-only bind (T-04-05) -- the listener address is never derived
    // from any caller-supplied host, only the fixed loopback interface plus
    // the D-02 port.
    let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, cli.port)).await?;
    eprintln!("orchestratord listening on {}", listener.local_addr()?);

    orchestrator::server::serve(listener, orchestrator, activity_registry).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_path_for_display;
    use std::path::PathBuf;

    #[test]
    fn resolve_path_for_display_returns_the_canonicalized_absolute_path_for_an_existing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_path_for_display(&dir.path().to_path_buf());
        assert_eq!(
            resolved,
            dir.path().canonicalize().expect("canonicalize should succeed for an existing directory")
        );
        assert!(resolved.is_absolute(), "expected the resolved path to be absolute, got: {resolved:?}");
    }

    #[test]
    fn resolve_path_for_display_falls_back_to_the_original_path_when_it_does_not_exist_yet() {
        // Mirrors the real startup sequence (WR-01, 06-REVIEW.md): a fresh
        // install's agent-runs/scratch directory does not exist yet at the
        // point this resolves it -- `agent_log::store::append`/
        // `LocalRuntime::prepare` create it lazily on first write.
        let missing = PathBuf::from("./this-directory-does-not-exist-yet-06-review-wr-01");
        let resolved = resolve_path_for_display(&missing);
        assert_eq!(
            resolved, missing,
            "expected a fallback to the original (possibly relative) path when canonicalize fails"
        );
    }

    // -----------------------------------------------------------------
    // WR-02 (06-REVIEW.md): `main()`'s synchronous startup prune calls
    // (`agent_log::store::prune`, `LocalRuntime::prune_scratch`) must run
    // off the async worker thread via `tokio::task::spawn_blocking`, never
    // directly inside `async fn main()`.
    // -----------------------------------------------------------------

    #[test]
    fn run_blocking_io_propagates_the_closures_ok_and_err_results() {
        let rt = tokio::runtime::Builder::new_current_thread().build().expect("build runtime");
        rt.block_on(async {
            let ok: std::io::Result<()> = super::run_blocking_io(|| Ok(())).await;
            assert!(ok.is_ok(), "expected an Ok closure result to propagate as Ok");

            let err: std::io::Result<()> =
                super::run_blocking_io(|| Err(std::io::Error::other("boom"))).await;
            assert!(err.is_err(), "expected an Err closure result to propagate as Err");
        });
    }

    #[test]
    fn run_blocking_io_never_blocks_the_current_thread_runtimes_only_worker_thread() {
        // A `current_thread` runtime has exactly ONE worker thread -- if
        // `run_blocking_io`'s closure ran directly on that thread (the
        // pre-fix behavior), a concurrently-spawned tokio task could make
        // ZERO progress while the closure's `std::thread::sleep` was
        // running, because nothing else could ever be scheduled. Proving
        // the counter task DOES advance while the "blocking I/O" sleeps is
        // exactly what proves the work was actually offloaded to tokio's
        // separate blocking thread pool via `spawn_blocking`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("build runtime");
        rt.block_on(async {
            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counter_task = counter.clone();
            tokio::spawn(async move {
                loop {
                    counter_task.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            });

            // Yield once so the counting task gets its first tick queued
            // before the blocking call starts.
            tokio::task::yield_now().await;

            let result: std::io::Result<()> = super::run_blocking_io(|| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(())
            })
            .await;
            assert!(result.is_ok());

            assert!(
                counter.load(std::sync::atomic::Ordering::SeqCst) > 1,
                "expected the concurrently-spawned counting task to keep advancing while the \
                 150ms blocking closure ran, proving it was offloaded off this current_thread \
                 runtime's only worker thread rather than blocking it directly"
            );
        });
    }
}
