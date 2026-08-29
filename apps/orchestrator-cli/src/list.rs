//! `list` subcommand entry point (CLI-01).
//!
//! Deliberately thin: scanning/loading stays behind the `OrchestratorClient`
//! seam -- now exclusively the WS-backed `WsOrchestratorClient` reaching
//! `orchestratord` (D-06 full cutover; the daemon owns the registry) -- this
//! module only renders the response. Generic over `Write` for stdout so the
//! integration test can capture output in a `Vec<u8>` instead of racing real
//! process stdout across parallel test threads; `main.rs` passes
//! `std::io::stdout()`. Warnings always go to the real process stderr via
//! `eprintln!` (D-10) — never silently dropped, and kept separate from
//! stdout so pipes stay clean (D-11: human-only output, no `--json`). The
//! DESCRIPTION column is single-line and width-bounded (quick task
//! 260815-mfp): `render_description` collapses the raw Markdown body into
//! one line and caps it to the per-row budget before it ever reaches `out`.

use std::io::Write;

use shared::{ListWorkflowsRequest, OrchestratorClient};

/// Target total row width in columns. Fixed, never queried from the
/// invoking terminal (D-2): a `terminal_size`-style dependency would make
/// `list_integration.rs`'s byte-identical-stdout assertions untestable and
/// would make piped output differ from interactive output. 80 is what the
/// originating todo asked for.
const TARGET_ROW_WIDTH: usize = 80;

/// Minimum DESCRIPTION column budget, overriding the 80-column target when
/// pathologically long ids/names would otherwise derive a near-zero or
/// negative budget (D-4). A row may exceed `TARGET_ROW_WIDTH` only in this
/// documented case -- a useless empty/all-ellipsis column is a worse
/// failure than a wrapped row.
const MIN_DESCRIPTION_BUDGET: usize = 20;

/// Renders a workflow's raw `description` (the loader reads the entire
/// Markdown body verbatim as `description` -- see
/// `apps/shared/src/client.rs:153`) into a single line bounded by `budget`
/// characters.
///
/// This is the load-bearing half of the fix: for an action-type workflow
/// the body is one short sentence, but for an agent-type workflow (e.g.
/// `workflows/ai_summarize.md`) the body IS the Claude prompt -- several
/// hard-wrapped paragraphs separated by blank lines. Collapsing every
/// whitespace run (newlines, blank-line paragraph breaks, tabs, runs of
/// spaces) to a single space, not just capping the width, is what keeps
/// that workflow to one row instead of 20+.
///
/// Collapses all whitespace runs to single spaces (D-1, also trims both
/// ends). If the collapsed value already fits `budget` characters it is
/// returned unchanged -- no ellipsis, no rewritten spacing (D-5). Otherwise
/// it is truncated to `budget - 1` characters (by `char`, never by byte, so
/// a cut can never split a multi-byte UTF-8 character -- D-6), trailing
/// whitespace is trimmed off that prefix, and a single ellipsis character
/// (`…`, D-5) is appended so the total rendered length stays `<= budget`.
fn render_description(raw: &str, budget: usize) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");

    if collapsed.chars().count() <= budget {
        return collapsed;
    }

    let truncate_at = budget.saturating_sub(1);
    let prefix: String = collapsed.chars().take(truncate_at).collect();
    format!("{}…", prefix.trim_end())
}

/// Runs the `list` subcommand against `client`, printing one aligned row per
/// loaded workflow to `out` — deterministically sorted by id (D-09, via
/// `Registry::enumerate()`) — and one `warning: ...` line per rejected file
/// to stderr (D-10). Never panics on load errors (D-07).
pub async fn run<W: Write>(client: &dyn OrchestratorClient, out: &mut W) -> std::io::Result<()> {
    let response = client.list_workflows(ListWorkflowsRequest::default()).await;

    let id_width = response
        .workflows
        .iter()
        .map(|s| s.id.len())
        .max()
        .unwrap_or(0)
        .max("ID".len());
    let name_width = response
        .workflows
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0)
        .max("NAME".len());

    // The description budget is derived FROM the already-computed column
    // widths (D-3), not hardcoded, so widening either column automatically
    // narrows the description rather than pushing the row past
    // TARGET_ROW_WIDTH. `id_width + name_width + 4` accounts for the two
    // column widths plus the two two-space gaps preceding DESCRIPTION.
    let description_budget = TARGET_ROW_WIDTH
        .saturating_sub(id_width + name_width + 4)
        .max(MIN_DESCRIPTION_BUDGET);

    writeln!(out, "{:<id_width$}  {:<name_width$}  DESCRIPTION", "ID", "NAME")?;
    for summary in &response.workflows {
        writeln!(
            out,
            "{:<id_width$}  {:<name_width$}  {}",
            summary.id,
            summary.name,
            render_description(&summary.description, description_budget)
        )?;
    }

    for warning in &response.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}
