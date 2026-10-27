//! Resolve-then-apply pipeline — port of `src/domain/editor.ts`
//! (`pi-semantic-edit`, MIT, (c) 2026 K2).
//!
//! Every request is matched against the ORIGINAL content (never incrementally),
//! then all resolved spans are applied bottom-up. Resolution applies, in order:
//! the 10-pass chain, the disproportionate-span guard, `replaceAll` or the
//! uniqueness guard, and finally a bounded auto-expand when a single occurrence
//! can be identified from surrounding context.

use crate::tools::semantic_edit::apply::apply_edits;
use crate::tools::semantic_edit::chain::find_match;
use crate::tools::semantic_edit::closest::{find_closest_candidate, DEFAULT_MAX_CANDIDATES};
use crate::tools::semantic_edit::errors::{
    ambiguous_error, disproportionate_error, no_op_error, not_found_error, overlapping_error,
    validation_error,
};
use crate::tools::semantic_edit::types::{
    EditError, EditFailureKind, EditRequest, FailedEdit, MatchResult, ResolvedEdit,
};
use crate::tools::semantic_edit::uniqueness::report_occurrences;

/// Auto-expand cap: total lines added around a match (half above, half below).
pub const MAX_EXPAND_LINES: usize = 10;

/// Result of [`apply_blocks`]. `base_content` / `new_content` mirror
/// [`crate::tools::edit_diff::ApplyResult`] so the tool layer can keep diffing
/// exactly as before.
#[derive(Debug, Clone)]
pub struct ApplyBlocksResult {
    pub base_content: String,
    pub new_content: String,
    /// Pass name per applied edit (`replace_all` / `auto_expand` included).
    pub match_passes: Vec<&'static str>,
    /// Total spans replaced (`replaceAll` can consume many for one edit).
    pub replacements: usize,
}

/// Match every request against ORIGINAL content (non-incremental).
/// Mirrors `resolveBlocks`.
pub fn resolve_blocks(
    content: &str,
    blocks: &[EditRequest],
    path: &str,
) -> Result<Vec<ResolvedEdit>, EditError> {
    let mut resolved: Vec<ResolvedEdit> = Vec::new();
    for block in blocks {
        if block.old_text.is_empty() {
            return Err(validation_error(
                "oldText is empty; provide the exact text to find.",
            ));
        }
        if block.old_text == block.new_text {
            return Err(validation_error(
                "oldText and newText are identical; this edit does nothing.",
            ));
        }

        let Some(matched) = find_match(content, &block.old_text) else {
            let closest = find_closest_candidate(content, &block.old_text, DEFAULT_MAX_CANDIDATES);
            return Err(not_found_error(path, closest.as_ref()));
        };

        // --- Refuse fuzzy matches spanning far more than the query — a
        // wrong-edit near-miss ---
        if is_disproportionate_match(&matched.actual, &block.old_text) {
            return Err(disproportionate_error(path));
        }

        // --- replaceAll: every occurrence of the matched actual text; spans
        // never overlap (the scan advances by one) ---
        if block.replace_all {
            let spans = find_all_spans(content, &matched.actual);
            if spans.is_empty() {
                return Err(validation_error(
                    "internal invariant violated: matched text not found in content",
                ));
            }
            let pass_name = if spans.len() > 1 {
                "replace_all"
            } else {
                matched.pass_name
            };
            for (start, end) in spans {
                // --- Fuzzy passes can over-reach even on 1-line queries — guard
                // per span ---
                let actual = &content[start..end];
                if is_disproportionate_match(actual, &block.old_text) {
                    return Err(disproportionate_error(path));
                }
                resolved.push(ResolvedEdit {
                    edit: resolved_request(path, block),
                    matched: MatchResult {
                        actual: matched.actual.clone(),
                        pass_name,
                    },
                    start,
                    end,
                });
            }
            continue;
        }

        let report = report_occurrences(content, &matched.actual);
        if report.ambiguous {
            if let Some(expanded) = try_auto_expand(content, block, path, &matched.actual) {
                resolved.push(expanded);
                continue;
            }
            return Err(ambiguous_error(path, report.count, &report.positions));
        }

        let Some(start) = content.find(&matched.actual) else {
            return Err(validation_error(
                "internal invariant violated: matched text not found in content",
            ));
        };
        resolved.push(ResolvedEdit {
            edit: resolved_request(path, block),
            matched: matched.clone(),
            start,
            end: start + matched.actual.len(),
        });
    }
    Ok(resolved)
}

/// Resolve + apply. Mirrors `applyBlocks`.
pub fn apply_blocks(
    content: &str,
    blocks: &[EditRequest],
    path: &str,
) -> Result<ApplyBlocksResult, EditError> {
    let resolved = resolve_blocks(content, blocks, path)?;
    let outcome = apply_edits(content, &resolved);
    if let Some(first) = outcome.failed.first() {
        return Err(failure_to_error(first));
    }
    Ok(ApplyBlocksResult {
        base_content: content.to_string(),
        new_content: outcome.content,
        match_passes: outcome
            .applied
            .iter()
            .map(|a| a.matched.pass_name)
            .collect(),
        replacements: outcome.applied.len(),
    })
}

fn resolved_request(path: &str, block: &EditRequest) -> EditRequest {
    EditRequest {
        path: path.to_string(),
        old_text: block.old_text.clone(),
        new_text: block.new_text.clone(),
        replace_all: false,
    }
}

fn failure_to_error(f: &FailedEdit) -> EditError {
    match f.kind {
        EditFailureKind::Overlap => overlapping_error(&f.reason),
        EditFailureKind::NoOp => no_op_error(),
        EditFailureKind::Invariant => validation_error(&f.reason),
    }
}

// --- Auto-expand: grow context around each occurrence until exactly ONE is
// unique; expansion only locates, the replacement targets the ORIGINAL span ---

fn try_auto_expand(
    content: &str,
    block: &EditRequest,
    path: &str,
    actual: &str,
) -> Option<ResolvedEdit> {
    let spans = find_all_spans(content, actual);
    if spans.len() < 2 {
        return None;
    }

    let lines: Vec<&str> = content.split('\n').collect();
    let line_ranges: Vec<(usize, usize)> = spans
        .iter()
        .map(|&(s, e)| {
            (
                line_index_at(&lines, s),
                line_index_at(&lines, e.saturating_sub(1)),
            )
        })
        .collect();

    let mut above = 0usize;
    let mut below = 0usize;
    let half = MAX_EXPAND_LINES / 2;

    for level in 0..MAX_EXPAND_LINES {
        // --- Expand alternately: above, below, above, below… ---
        if level % 2 == 0 {
            if above >= half {
                continue;
            }
            above += 1;
        } else {
            if below >= half {
                continue;
            }
            below += 1;
        }

        let expanded_blocks: Vec<String> = line_ranges
            .iter()
            .map(|&(start_line, end_line)| {
                let s = start_line.saturating_sub(above);
                let e = (end_line + 1 + below).min(lines.len());
                lines[s..e].join("\n")
            })
            .collect();

        if let Some(unique_idx) = find_single_unique(&expanded_blocks, content) {
            let (start, end) = spans[unique_idx];
            return Some(ResolvedEdit {
                edit: resolved_request(path, block),
                matched: MatchResult {
                    actual: actual.to_string(),
                    pass_name: "auto_expand",
                },
                start,
                end,
            });
        }
        // --- Multiple uniques stay unique under extension — bail (genuine
        // ambiguity) ---
        if expanded_blocks
            .iter()
            .filter(|b| count_occurrences(content, b) == 1)
            .count()
            > 1
        {
            return None;
        }
    }
    None
}

/// Byte spans of every occurrence of `needle` (overlapping included).
fn find_all_spans(content: &str, needle: &str) -> Vec<(usize, usize)> {
    if needle.is_empty() {
        return Vec::new();
    }
    let mut spans = Vec::new();
    let mut from = 0usize;
    while from <= content.len() {
        let Some(idx) = content[from..].find(needle).map(|i| from + i) else {
            break;
        };
        spans.push((idx, idx + needle.len()));
        from = idx + 1;
        while from < content.len() && !content.is_char_boundary(from) {
            from += 1;
        }
    }
    spans
}

/// OpenCode `isDisproportionateMatch`: the matched span must not dwarf the
/// query; 1-line queries are exempt (over-reaching passes need >= 2 lines).
fn is_disproportionate_match(search: &str, old_text: &str) -> bool {
    let old_lines = old_text.split('\n').count();
    let search_lines = search.split('\n').count();
    if search_lines >= (old_lines + 3).max(old_lines * 2) {
        return true;
    }
    if old_lines == 1 {
        return false;
    }
    let search_len = search.trim().chars().count();
    let old_len = old_text.trim().chars().count();
    search_len > (old_len + 500).max(old_len * 4)
}

fn count_occurrences(content: &str, needle: &str) -> usize {
    find_all_spans(content, needle).len()
}

/// Index of the line containing char offset `offset` in lines joined by `\n`.
fn line_index_at(lines: &[&str], offset: usize) -> usize {
    let mut remaining = offset;
    for (i, line) in lines.iter().enumerate() {
        let len = line.len() + usize::from(i < lines.len() - 1);
        if remaining < len {
            return i;
        }
        remaining -= len;
    }
    lines.len().saturating_sub(1)
}

fn find_single_unique(blocks: &[String], content: &str) -> Option<usize> {
    let mut unique_idx: Option<usize> = None;
    for (i, block) in blocks.iter().enumerate() {
        if count_occurrences(content, block) == 1 {
            if unique_idx.is_some() {
                return None; // more than one unique → ambiguous
            }
            unique_idx = Some(i);
        }
    }
    unique_idx
}
