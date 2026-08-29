//! Real `Service` implementations (SVC-01/SVC-02). Phase 2's D-08 —
//! "no `Service` implementation lives under `src/`" — was explicitly scoped
//! to Phase 2 only; this module is where that restriction is lifted. Every
//! handler here implements the same 4-method `Service` trait
//! (`service/mod.rs`) as the tests-only `StubService`, registered into
//! `InProcessOrchestrator` via `with_handlers`, keyed by the
//! `service.handler` string a workflow declares — never by workflow id.

pub mod ai_agent;
pub mod immediate;
pub mod process;
pub mod timers;
