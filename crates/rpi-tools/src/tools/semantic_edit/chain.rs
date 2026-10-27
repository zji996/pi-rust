//! Pass ordering + occurrence positions — port of `src/domain/chain.ts`
//! (`pi-semantic-edit`, MIT, (c) 2026 K2).

use crate::tools::edit_diff::normalize_to_lf;
use crate::tools::semantic_edit::passes::{
    block_anchor_find, context_aware_find, escape_normalized_find, indentation_flexible_find,
    line_trimmed_find, multi_occurrence_find, simple_find, trimmed_boundary_find,
    unicode_normalized_find, whitespace_normalized_find, PassFind,
};
use crate::tools::semantic_edit::types::MatchResult;

/// One entry of the chain. Mirrors TS `Replacer`.
pub struct Replacer {
    pub name: &'static str,
    pub find: PassFind,
}

/// How many lines taller than the query a fuzzy match may be.
///
/// `max(1, ceil(10% of the query's line count))`. Beyond this a pass is treated
/// as "no match" and the chain continues (or ends in not-found, which quotes the
/// closest real text) — a tolerance that swallows a neighbouring block would
/// edit the wrong region, which is worse than making the model retry.
pub fn span_line_budget(old_lines: usize) -> usize {
    // --- ceil(old_lines * 0.10), floored at 1 ---
    old_lines.div_ceil(10).max(1)
}

/// `true` when `actual` is too many lines taller than `old` to be trusted.
pub fn span_is_too_tall(actual: &str, old: &str) -> bool {
    let old_lines = old.split('\n').count();
    let actual_lines = actual.split('\n').count();
    actual_lines > old_lines.saturating_add(span_line_budget(old_lines))
}

/// Pass order — the single source of truth. Mirrors `REPLACER_CHAIN`.
pub const REPLACER_CHAIN: [Replacer; 10] = [
    Replacer {
        name: "simple",
        find: simple_find,
    },
    Replacer {
        name: "line_trimmed",
        find: line_trimmed_find,
    },
    Replacer {
        name: "block_anchor",
        find: block_anchor_find,
    },
    Replacer {
        name: "whitespace_normalized",
        find: whitespace_normalized_find,
    },
    Replacer {
        name: "indentation_flexible",
        find: indentation_flexible_find,
    },
    Replacer {
        name: "escape_normalized",
        find: escape_normalized_find,
    },
    Replacer {
        name: "trimmed_boundary",
        find: trimmed_boundary_find,
    },
    Replacer {
        name: "context_aware",
        find: context_aware_find,
    },
    Replacer {
        name: "multi_occurrence",
        find: multi_occurrence_find,
    },
    Replacer {
        name: "unicode_normalized",
        find: unicode_normalized_find,
    },
];

/// Run the chain in order, short-circuit on the first match. Mirrors
/// `findMatch`, with one Rust-side tightening on top of the TS chain:
///
/// * the returned `actual` is re-verified here: it must be non-empty and must be
///   present in the original text. No pass may legitimately produce anything
///   else, so a pass that does is treated as "no match" instead of being allowed
///   to splice text at a bogus location;
/// * every pass except the exact `simple` one may only answer with a span that
///   is at most [`span_line_budget`] lines taller than the query. A wider span
///   means the pass reached into a neighbouring block, so that pass is skipped
///   and the later (tighter) passes get their turn. When nothing survives, the
///   caller reports not-found with the closest real text.
pub fn find_match(original: &str, old_content: &str) -> Option<MatchResult> {
    let orig = normalize_to_lf(original);
    let old = normalize_to_lf(old_content);

    for replacer in REPLACER_CHAIN.iter() {
        if let Some(actual) = (replacer.find)(&orig, &old) {
            if actual.is_empty() || !orig.contains(actual.as_str()) {
                continue;
            }
            if replacer.name != "simple" && span_is_too_tall(&actual, &old) {
                continue;
            }
            return Some(MatchResult {
                actual,
                pass_name: replacer.name,
            });
        }
    }
    None
}

/// 1-indexed line numbers of every occurrence of `needle` in `haystack`.
/// Mirrors `findOccurrencePositions` (overlapping occurrences included, matching
/// TS `indexOf(needle, idx + 1)`).
pub fn find_occurrence_positions(haystack: &str, needle: &str) -> Vec<usize> {
    let mut positions = Vec::new();
    let mut search_pos = 0usize;
    while search_pos <= haystack.len() {
        let Some(idx) = haystack[search_pos..].find(needle).map(|i| search_pos + i) else {
            break;
        };
        // --- newlines before idx + 1 ---
        positions.push(haystack[..idx].matches('\n').count() + 1);
        search_pos = idx + 1;
        // Advance to the next char boundary when the needle can start mid-char.
        while search_pos < haystack.len() && !haystack.is_char_boundary(search_pos) {
            search_pos += 1;
        }
    }
    positions
}
