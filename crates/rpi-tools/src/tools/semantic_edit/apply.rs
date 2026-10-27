//! Apply resolved edits against the ORIGINAL content — port of
//! `src/domain/apply.ts` (`pi-semantic-edit`, MIT, (c) 2026 K2).

use crate::tools::edit_diff::normalize_to_lf;
use crate::tools::semantic_edit::types::{
    EditFailureKind, EditRequest, FailedEdit, MatchResult, ResolvedEdit,
};

/// A successfully applied edit. Mirrors TS `AppliedEdit`.
#[derive(Debug, Clone)]
pub struct AppliedEdit {
    pub edit: EditRequest,
    pub matched: MatchResult,
}

/// Outcome of applying every resolved edit to the original content.
/// Mirrors TS `ApplyResult`.
#[derive(Debug, Clone)]
pub struct ApplyResult {
    /// Resulting content on success, original content on failure.
    pub content: String,
    pub applied: Vec<AppliedEdit>,
    pub failed: Vec<FailedEdit>,
}

/// Apply `resolved` to `content`, bottom-up (higher spans first).
/// Mirrors `applyEdits`.
///
/// Overlap is checked on ascending spans, then every piece is re-verified
/// against the content it is about to be spliced into — a stale or wrong span
/// fails as `invariant` instead of corrupting the file.
pub fn apply_edits(content: &str, resolved: &[ResolvedEdit]) -> ApplyResult {
    let mut applied: Vec<AppliedEdit> = Vec::new();
    let mut failed: Vec<FailedEdit> = Vec::new();

    if resolved.is_empty() {
        return ApplyResult {
            content: content.to_string(),
            applied,
            failed,
        };
    }

    // --- Overlap detection needs ascending spans ---
    let mut sorted: Vec<&ResolvedEdit> = resolved.iter().collect();
    sorted.sort_by_key(|r| r.start);
    for pair in sorted.windows(2) {
        let prev = pair[0];
        let curr = pair[1];
        if curr.start < prev.end {
            failed.push(FailedEdit {
                edit: curr.edit.clone(),
                kind: EditFailureKind::Overlap,
                reason: format!(
                    "overlaps the edit at {}..{} (both matched against the original file)",
                    prev.start, prev.end
                ),
            });
        }
    }
    if !failed.is_empty() {
        return ApplyResult {
            content: content.to_string(),
            applied,
            failed,
        };
    }

    // --- Descending by start so earlier offsets stay valid ---
    let mut pieces: Vec<&ResolvedEdit> = resolved.iter().collect();
    pieces.sort_by_key(|piece| std::cmp::Reverse(piece.start));

    let mut result = content.to_string();
    for piece in pieces {
        let replacement = normalize_to_lf(&piece.edit.new_text);
        if replacement == piece.matched.actual {
            failed.push(FailedEdit {
                edit: piece.edit.clone(),
                kind: EditFailureKind::NoOp,
                reason:
                    "edit results in no change (old and new text identical after normalization)"
                        .to_string(),
            });
            continue;
        }
        if result.get(piece.start..piece.end) != Some(piece.matched.actual.as_str()) {
            failed.push(FailedEdit {
                edit: piece.edit.clone(),
                kind: EditFailureKind::Invariant,
                reason: "internal invariant violated: actual text not found at resolved span"
                    .to_string(),
            });
            continue;
        }
        result.replace_range(piece.start..piece.end, &replacement);
        applied.push(AppliedEdit {
            edit: piece.edit.clone(),
            matched: piece.matched.clone(),
        });
    }

    ApplyResult {
        content: result,
        applied,
        failed,
    }
}
