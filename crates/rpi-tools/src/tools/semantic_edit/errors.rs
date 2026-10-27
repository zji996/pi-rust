//! Model-facing error builders — port of `src/domain/errors.ts`
//! (`pi-semantic-edit`, MIT, (c) 2026 K2).
//!
//! The strings are the TS ones, unchanged: they are what the model reads and
//! retries against, so rewording them would silently change behavior.
//!
//! Not ported, because their inputs are not ported: `malformedPatchError` and
//! `missingPathError` (aider `patch` parsing / multi-path blocks — `parser.ts`),
//! and `fileNotFoundError` (the tool layer reports unreadable files itself).

use crate::tools::semantic_edit::types::{ClosestCandidate, EditError, EditErrorKind};
use crate::tools::semantic_edit::uniqueness::format_line_positions;

/// Mirrors `ambiguousError`.
pub fn ambiguous_error(path: &str, count: usize, positions: &[usize]) -> EditError {
    EditError {
        kind: EditErrorKind::Ambiguous,
        message: format!(
            "Text found {count} times at {} in {path}. Provide more surrounding context to make \
             the match unique, or set replaceAll: true to replace every occurrence.",
            format_line_positions(positions)
        ),
        line_positions: Some(positions.to_vec()),
        closest_candidate: None,
    }
}

/// Mirrors `notFoundError`.
pub fn not_found_error(path: &str, closest: Option<&ClosestCandidate>) -> EditError {
    let base = format!("Text not found in {path}. The 10-pass fuzzy matcher found no match.");
    let Some(closest) = closest else {
        return EditError {
            kind: EditErrorKind::NotFound,
            message: format!(
                "{base}\nNo similar text found either. Re-read the file to see its current \
                 content, then retry with the exact text to replace."
            ),
            line_positions: None,
            closest_candidate: None,
        };
    };
    let pct = (closest.similarity * 100.0).round() as i64;
    let lines = if closest.start_line == closest.end_line {
        format!("line {}", closest.start_line)
    } else {
        format!("lines {}-{}", closest.start_line, closest.end_line)
    };
    let preview = abbreviate(&closest.candidate);
    EditError {
        kind: EditErrorKind::NotFound,
        message: format!(
            "{base}\nClosest match ({pct}% similar) at {lines}:\n{preview}\nCompare against the \
             actual file content and retry with the correct text."
        ),
        line_positions: None,
        closest_candidate: Some(closest.clone()),
    }
}

/// Mirrors `disproportionateError`.
pub fn disproportionate_error(path: &str) -> EditError {
    EditError {
        kind: EditErrorKind::Disproportionate,
        message: format!(
            "Refusing to edit {path}: the matched span is much larger than the text to find. \
             Re-read the file and provide the full exact text for the intended replacement."
        ),
        line_positions: None,
        closest_candidate: None,
    }
}

/// Mirrors `overlappingError`.
pub fn overlapping_error(description: &str) -> EditError {
    EditError {
        kind: EditErrorKind::Overlapping,
        message: format!(
            "Overlapping edits: {description}. Each edits[].oldText must target a disjoint region \
             of the original file. Merge overlapping or adjacent changes into one edit and retry."
        ),
        line_positions: None,
        closest_candidate: None,
    }
}

/// Mirrors `noOpError`.
pub fn no_op_error() -> EditError {
    EditError {
        kind: EditErrorKind::NoOp,
        message: "Edit results in no change: the replacement text is identical to the matched \
                  text. Change newText to actually modify the content, or drop this edit."
            .to_string(),
        line_positions: None,
        closest_candidate: None,
    }
}

/// Mirrors `validationError`.
pub fn validation_error(message: impl Into<String>) -> EditError {
    EditError {
        kind: EditErrorKind::Validation,
        message: message.into(),
        line_positions: None,
        closest_candidate: None,
    }
}

/// Mirrors TS `abbreviate`: first `max_lines` lines, each capped at `max_cols`
/// chars (ellipsis appended), plus a trailing ellipsis line when truncated.
fn abbreviate(text: &str) -> String {
    const MAX_LINES: usize = 8;
    const MAX_COLS: usize = 80;
    let all: Vec<&str> = text.split('\n').collect();
    let mut lines: Vec<String> = all
        .iter()
        .take(MAX_LINES)
        .map(|l| {
            if l.chars().count() > MAX_COLS {
                let cut: String = l.chars().take(MAX_COLS).collect();
                format!("{cut}…")
            } else {
                (*l).to_string()
            }
        })
        .collect();
    if all.len() > MAX_LINES {
        lines.push("…".to_string());
    }
    lines.join("\n")
}
