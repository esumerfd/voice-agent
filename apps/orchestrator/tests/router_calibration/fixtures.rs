//! Shared known-positive / known-negative calibration corpus (Phase 9,
//! ROUT-02, D-04/D-05). Lives in a nested directory, never directly under
//! `tests/`, for the same reason `tests/live_agent_eval/support.rs` does:
//! any `*.rs` file directly under `tests/` is autodiscovered by Cargo as
//! its OWN test binary, and this module declares zero `#[test]` functions
//! -- it must never become one.
//!
//! Both `router_fixtures.rs` (this plan, non-live corpus-integrity gate)
//! and `router_calibration.rs` (plan 09-05, live-gated threshold
//! calibration) include this exact file via
//! `#[path = "router_calibration/fixtures.rs"] mod fixtures;` so the
//! corpus has exactly one definition -- never duplicated, never drifting
//! between the two consumers.
//!
//! Every utterance here is a plausible real phrasing a person would
//! actually speak at this assistant (D-04), paraphrasing one of the four
//! shipped workflows' authored `intent:` sentences (see
//! `workflows/*.md`), never a synthetic/generic template. Negatives cover
//! both D-05 kinds: `Unrelated` (obviously off-domain, proves basic
//! rejection) and `CloseButWrong` (adjacent to a real workflow's domain,
//! proves the threshold doesn't over-match on superficial keyword
//! overlap). `set_timer` and `countdown` are the hardest real pair --
//! both involve counting down out loud -- so the corpus proves separation
//! on that pair in both directions, not only on easy cases.
//!
//! A fixture utterance is never reworded to make a future calibration run
//! pass; a corpus that cannot be separated is a signal about the
//! `intent:` sentences, not a test to soften (plan prohibition).

/// The four workflow ids shipped in `workflows/` today. A corpus case
/// naming any other id is a bug in the corpus itself, not a routing
/// failure -- `router_fixtures.rs` asserts every case stays inside this
/// set.
pub const REAL_WORKFLOW_IDS: [&str; 4] =
    ["calendar_today", "set_timer", "countdown", "ai_summarize"];

/// One utterance a person might plausibly speak, paired with the workflow
/// id it should route to.
#[derive(Debug, Clone, Copy)]
pub struct PositiveCase {
    pub utterance: &'static str,
    pub expected_workflow_id: &'static str,
}

/// D-05's two negative kinds: obviously off-domain vs. deliberately
/// adjacent-but-wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NegativeKind {
    Unrelated,
    CloseButWrong,
}

/// One utterance that must NOT route to `must_not_match`, tagged with
/// which D-05 kind it exercises.
#[derive(Debug, Clone, Copy)]
pub struct NegativeCase {
    pub utterance: &'static str,
    pub must_not_match: &'static str,
    pub kind: NegativeKind,
}

/// Known-positive utterances: at least 3 per shipped workflow, varying
/// sentence shape (question / imperative / casual fragment) rather than
/// swapping one synonym.
const POSITIVES: &[PositiveCase] = &[
    // calendar_today -- "check what is on today's calendar and what is
    // scheduled for the rest of the day"
    PositiveCase {
        utterance: "what's on my calendar today?",
        expected_workflow_id: "calendar_today",
    },
    PositiveCase {
        utterance: "check today's schedule for me",
        expected_workflow_id: "calendar_today",
    },
    PositiveCase {
        utterance: "anything else coming up for the rest of today",
        expected_workflow_id: "calendar_today",
    },
    // set_timer -- "set a timer for a given number of minutes and notify
    // me when the time is up"
    PositiveCase {
        utterance: "can you set a timer for ten minutes?",
        expected_workflow_id: "set_timer",
    },
    PositiveCase {
        utterance: "set a timer for five minutes",
        expected_workflow_id: "set_timer",
    },
    PositiveCase {
        utterance: "timer for twenty minutes, let me know when it's done",
        expected_workflow_id: "set_timer",
    },
    // countdown -- "count down out loud from three to go as a spoken
    // start signal"
    PositiveCase {
        utterance: "give me a countdown to start",
        expected_workflow_id: "countdown",
    },
    PositiveCase {
        utterance: "can you count me down from three?",
        expected_workflow_id: "countdown",
    },
    PositiveCase {
        utterance: "count down and say go",
        expected_workflow_id: "countdown",
    },
    // ai_summarize -- "read the staged reference material and write a
    // concise written summary of it"
    PositiveCase {
        utterance: "can you summarize the reference material for me?",
        expected_workflow_id: "ai_summarize",
    },
    PositiveCase {
        utterance: "write me a concise summary of that document",
        expected_workflow_id: "ai_summarize",
    },
    PositiveCase {
        utterance: "summarize it for me, focusing on the key points",
        expected_workflow_id: "ai_summarize",
    },
];

pub fn known_positives() -> &'static [PositiveCase] {
    POSITIVES
}

/// Known-negative utterances: both D-05 kinds, including the hardest
/// real pair (`set_timer`/`countdown`) covered in both directions, plus a
/// close-but-wrong case adjacent to `ai_summarize`'s domain (the sole
/// confirmation-required workflow -- Pitfall C turns on exactly this
/// separation).
const NEGATIVES: &[NegativeCase] = &[
    // Unrelated: obviously off-domain, proves basic rejection.
    NegativeCase {
        utterance: "what's the weather like tomorrow?",
        must_not_match: "calendar_today",
        kind: NegativeKind::Unrelated,
    },
    NegativeCase {
        utterance: "play some music",
        must_not_match: "set_timer",
        kind: NegativeKind::Unrelated,
    },
    NegativeCase {
        utterance: "tell me a joke",
        must_not_match: "countdown",
        kind: NegativeKind::Unrelated,
    },
    // CloseButWrong: adjacent to calendar_today's domain -- next week is
    // not today.
    NegativeCase {
        utterance: "what do I have scheduled next week?",
        must_not_match: "calendar_today",
        kind: NegativeKind::CloseButWrong,
    },
    NegativeCase {
        utterance: "what time is it right now?",
        must_not_match: "calendar_today",
        kind: NegativeKind::CloseButWrong,
    },
    // CloseButWrong: the hardest real pair, direction one -- this
    // utterance actually describes countdown's spoken-start-signal
    // behaviour, not set_timer's duration-and-notification behaviour.
    NegativeCase {
        utterance: "count down out loud from three and say go",
        must_not_match: "set_timer",
        kind: NegativeKind::CloseButWrong,
    },
    // CloseButWrong: the hardest real pair, direction two (mirror) --
    // this utterance actually describes set_timer's duration-and-
    // notification behaviour, not countdown's spoken-start-signal.
    NegativeCase {
        utterance: "set a timer for ten minutes and notify me when it's done",
        must_not_match: "countdown",
        kind: NegativeKind::CloseButWrong,
    },
    // CloseButWrong: adjacent to ai_summarize's domain -- sounds
    // summarization-shaped but asks for a verbatim reading, not a
    // concise written summary.
    NegativeCase {
        utterance: "read the reference material out loud to me, word for word",
        must_not_match: "ai_summarize",
        kind: NegativeKind::CloseButWrong,
    },
];

pub fn known_negatives() -> &'static [NegativeCase] {
    NEGATIVES
}
