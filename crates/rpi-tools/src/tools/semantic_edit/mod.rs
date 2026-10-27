//! Semantic matcher for the built-in `edit` tool.
//!
//! Rust port of `pi-semantic-edit` (`src/domain/*.ts`), the drop-in `edit`
//! replacement authored by k3-2o / K2:
//! <https://github.com/k3-2o/pi-semantic-edit>.
//!
//! ```text
//! pi-semantic-edit — Copyright (c) 2026 K2 — MIT License
//! ```
//!
//! What is ported, file by file (the TS module in parentheses):
//!
//! | Rust            | TS                    | content                                   |
//! |-----------------|-----------------------|-------------------------------------------|
//! | [`types`]       | `domain/types.ts`     | request/match/resolve/error shapes         |
//! | [`similarity`]  | `domain/similarity.ts`| LCS ratio (2·LCS/(|a|+|b|))                |
//! | [`passes`]      | `domain/passes.ts`    | the 10 matcher passes                      |
//! | [`chain`]       | `domain/chain.ts`     | pass order, occurrence line positions      |
//! | [`uniqueness`]  | `domain/uniqueness.ts`| ambiguity report + "line N, line M" format |
//! | [`closest`]     | `domain/closest.ts`   | closest near-miss on no-match              |
//! | [`coherence`]   | `domain/coherence.ts` | non-blocking brace/indent warnings         |
//! | [`errors`]      | `domain/errors.ts`    | model-facing error strings, verbatim       |
//! | [`apply`]       | `domain/apply.ts`     | overlap/no-op/invariant checks, splice     |
//! | [`editor`]      | `domain/editor.ts`    | resolve-against-original + apply pipeline  |
//!
//! Deliberately *not* ported: `domain/parser.ts` (deprecated aider
//! SEARCH/REPLACE patch input) and `pi/render.ts` (TUI rendering). The
//! `pi/normalize.ts` argument shapes are handled by
//! [`crate::tools::edit::EditInput`] / `prepare_edit_arguments`; the byte-level
//! BOM / line-ending helpers (`pi/domain/utils.ts`) plus diff generation stay in
//! [`crate::tools::edit_diff`], which is upstream code kept as-is.
//!
//! Behavior notes (mirrors the TS contract):
//!
//! * Every `edits[]` entry is matched against the **original** content — never
//!   incrementally — and the resolved span is re-verified against that content
//!   before it is spliced in ([`apply::apply_edits`]).
//! * A match is only accepted from a pass if the returned text is actually
//!   present in the original file; [`chain::find_match`] re-checks that (and
//!   drops empty matches, which no pass may legitimately produce).
//! * Rust-side tightening on top of the TS chain: a fuzzy pass may not answer
//!   with a span more than `max(1, ceil(10%))` lines taller than the query
//!   ([`chain::span_line_budget`]). Such a span reached into a neighbouring
//!   block; the pass is skipped and, when nothing tighter matches, the edit
//!   fails as not-found with the closest real text instead of editing the wrong
//!   region.
//! * Multiple occurrences of the matched text fail with the line positions
//!   unless the edit sets `replaceAll`, or unless context expansion can
//!   identify exactly one occurrence ([`editor::resolve_blocks`]).
//! * Errors are the TS strings, unchanged, because they are what the model
//!   retries against.

pub mod apply;
pub mod chain;
pub mod closest;
pub mod coherence;
pub mod editor;
pub mod errors;
pub mod passes;
pub mod similarity;
pub mod types;
pub mod uniqueness;

pub use apply::{apply_edits, AppliedEdit};
pub use chain::{
    find_match, find_occurrence_positions, span_is_too_tall, span_line_budget, Replacer,
    REPLACER_CHAIN,
};
pub use closest::find_closest_candidate;
pub use coherence::coherence_check;
pub use editor::{apply_blocks, resolve_blocks, ApplyBlocksResult, MAX_EXPAND_LINES};
pub use errors::{
    ambiguous_error, disproportionate_error, no_op_error, not_found_error, overlapping_error,
    validation_error,
};
pub use types::{
    ClosestCandidate, EditError, EditErrorKind, EditFailureKind, EditRequest, FailedEdit,
    MatchResult, ResolvedEdit,
};
pub use uniqueness::{format_line_positions, report_occurrences, OccurrenceReport};

#[cfg(test)]
mod tests;
