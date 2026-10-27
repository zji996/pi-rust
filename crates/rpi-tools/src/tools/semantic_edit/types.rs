//! Shared domain types — port of `src/domain/types.ts` (`pi-semantic-edit`,
//! MIT, (c) 2026 K2). No Pi-specific imports.

use crate::error::{FileError, FileErrorCode};

/// One request: the built-in `edits[]` shape plus the optional `replaceAll`
/// escape hatch. Mirrors TS `EditRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditRequest {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    /// Replace every occurrence of the matched text instead of failing on
    /// ambiguity. Mirrors TS `EditRequest.replaceAll` (default `false`).
    pub replace_all: bool,
}

/// A fuzzy match. `actual` is the **original** substring (never the query), so
/// the replacement preserves the real formatting. Mirrors TS `MatchResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchResult {
    pub actual: String,
    /// Which pass matched (OpenDev parity).
    pub pass_name: &'static str,
}

/// Near-miss candidate for closest-candidate-on-failure feedback. Mirrors TS
/// `ClosestCandidate`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClosestCandidate {
    pub pass_name: &'static str,
    pub similarity: f64,
    pub candidate: String,
    pub start_line: usize,
    pub end_line: usize,
}

/// A matched edit with its located span in the (LF-normalized) content.
/// Mirrors TS `ResolvedEdit`.
#[derive(Debug, Clone)]
pub struct ResolvedEdit {
    pub edit: EditRequest,
    pub matched: MatchResult,
    pub start: usize,
    pub end: usize,
}

/// Which invariant killed an edit during application. Mirrors TS
/// `EditFailureKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditFailureKind {
    Overlap,
    NoOp,
    Invariant,
}

/// An edit that could not be applied. Mirrors TS `FailedEdit`.
#[derive(Debug, Clone)]
pub struct FailedEdit {
    pub edit: EditRequest,
    pub reason: String,
    pub kind: EditFailureKind,
}

/// Error categories. Mirrors TS `EditErrorKind` minus the parser-only
/// `malformed-patch` / `missing-path` kinds (`parser.ts` is not ported).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditErrorKind {
    FileNotFound,
    Ambiguous,
    NotFound,
    Overlapping,
    NoOp,
    Disproportionate,
    Validation,
}

/// A model-facing edit failure. Mirrors TS `EditError`.
#[derive(Debug, Clone)]
pub struct EditError {
    pub kind: EditErrorKind,
    pub message: String,
    /// 1-indexed line positions for ambiguous matches.
    pub line_positions: Option<Vec<usize>>,
    /// Best near-miss when nothing matched.
    pub closest_candidate: Option<ClosestCandidate>,
}

impl EditError {
    /// Stable [`FileErrorCode`] for this failure kind, so callers keep the
    /// code semantics the pre-port matcher used (not-found vs invalid).
    pub fn code(&self) -> FileErrorCode {
        match self.kind {
            EditErrorKind::NotFound | EditErrorKind::FileNotFound => FileErrorCode::NotFound,
            EditErrorKind::Ambiguous
            | EditErrorKind::Overlapping
            | EditErrorKind::NoOp
            | EditErrorKind::Disproportionate
            | EditErrorKind::Validation => FileErrorCode::Invalid,
        }
    }

    /// Convert into the `FileError` the tool layer returns. The TS message
    /// already names the path, so no `with_path` is added (that would append a
    /// second, absolute one to the model-facing text).
    pub fn into_file_error(self) -> FileError {
        FileError::new(self.code(), self.message)
    }
}

impl std::fmt::Display for EditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for EditError {}
