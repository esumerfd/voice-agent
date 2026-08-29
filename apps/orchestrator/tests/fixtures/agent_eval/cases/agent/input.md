# Staged Input Document

This is the staged input document used by the `happy-staged-file` eval case
(06-06). A real agent run would read this file and summarize it into
`summary.md`; the fixture double never reads the prompt, so this content
only needs to keep the case legible to a human reading it later. Living
directly under an `agent/` subdirectory keeps it excluded from the
registry's own workflow scan (`registry/loader.rs`'s existing reserved-
directory-name exclusion), exactly as it already does for
`workflows/agent/ai_summarize_reference.md`.
