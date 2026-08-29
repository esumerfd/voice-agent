//! `orchestrator` binary entry point (D-01/D-03/D-06).
//!
//! Single current_thread runtime: this CLI is short-lived and
//! low-concurrency (RESEARCH.md Pattern 3 / Pitfall 3) — no reason to pay
//! for a multi-threaded scheduler here. Full WS cutover (D-06): the CLI
//! never touches the in-process registry path directly -- it only ever
//! reaches the registry through a `WsOrchestratorClient` speaking to a
//! separately running `orchestratord` (see
//! `apps/orchestrator/src/bin/orchestratord.rs`, built in 04-04). No
//! `--in-process` escape hatch ships.

mod cli;
mod create;
mod delete;
mod list;
mod log;
mod run;
mod ws_client;

use clap::Parser;
use cli::{Cli, Commands, WorkflowCommands};
use shared::{OrchestratorClient, WorkflowCreator, WorkflowDeleter, WorkflowWriteMode};

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let cli = Cli::parse();

    // `Commands::Log` is handled BEFORE the connect call below (load-bearing
    // wiring, quick task 260812-qeg): reading the daemon's log file must
    // never require a live daemon connection -- it is most wanted exactly
    // when the daemon is dead.
    if let Commands::Log { ref file, lines, follow } = cli.command {
        let candidates = log::default_candidates();
        let code = log::run(file.as_deref(), &candidates, lines, follow, &mut std::io::stdout()).await?;
        std::process::exit(code);
    }

    let addr = format!("ws://127.0.0.1:{}", cli.port);
    let client = match ws_client::WsOrchestratorClient::connect(&addr).await {
        Ok(client) => client,
        Err(err) => {
            // D-04: a clear, nonzero-exit error naming the fix -- never an
            // auto-spawn of the daemon.
            eprintln!(
                "cannot reach orchestratord at {addr}: {err}. Start it with `orchestratord --port {}` (see packaging/README.md).",
                cli.port
            );
            std::process::exit(1);
        }
    };
    match cli.command {
        Commands::List => list::run(&client as &dyn OrchestratorClient, &mut std::io::stdout()).await,
        Commands::Run { workflow_id, raw_flags } => {
            let code = run::run(&client as &dyn OrchestratorClient, &workflow_id, &raw_flags, &mut std::io::stdout())
                .await?;
            std::process::exit(code);
        }
        Commands::Workflow {
            command:
                WorkflowCommands::Create {
                    id,
                    source,
                    display_name,
                    description,
                    param,
                    script,
                    markdown,
                    agent,
                    agent_file,
                    timeout_secs,
                    max_budget_usd,
                },
        } => {
            let agent_flags = create::AgentFlags {
                agent,
                agent_file,
                timeout_secs,
                max_budget_usd,
            };
            let code = create::run(
                &client as &dyn WorkflowCreator,
                WorkflowWriteMode::Create,
                &id,
                &source,
                display_name.as_deref(),
                description.as_deref(),
                &param,
                script,
                markdown,
                &agent_flags,
                &mut std::io::stdout(),
            )
            .await?;
            std::process::exit(code);
        }
        Commands::Workflow {
            command: WorkflowCommands::Delete { id },
        } => {
            let code = delete::run(&client as &dyn WorkflowDeleter, &id, &mut std::io::stdout()).await?;
            std::process::exit(code);
        }
        Commands::Workflow {
            command:
                WorkflowCommands::Edit {
                    id,
                    source,
                    display_name,
                    description,
                    param,
                    script,
                    markdown,
                    agent,
                    agent_file,
                    timeout_secs,
                    max_budget_usd,
                },
        } => {
            let agent_flags = create::AgentFlags {
                agent,
                agent_file,
                timeout_secs,
                max_budget_usd,
            };
            let code = create::run(
                &client as &dyn WorkflowCreator,
                WorkflowWriteMode::Edit,
                &id,
                &source,
                display_name.as_deref(),
                description.as_deref(),
                &param,
                script,
                markdown,
                &agent_flags,
                &mut std::io::stdout(),
            )
            .await?;
            std::process::exit(code);
        }
        Commands::Log { .. } => unreachable!("Commands::Log is fully handled above, before connect"),
    }
}
