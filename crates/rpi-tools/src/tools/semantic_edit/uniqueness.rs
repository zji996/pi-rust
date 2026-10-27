//! Uniqueness guard — port of `src/domain/uniqueness.ts` (`pi-semantic-edit`,
//! MIT, (c) 2026 K2).
//!
//! Counts occurrences of the **actual** matched text (not the query); more than
//! one means ambiguous — never guess.

use crate::tools::semantic_edit::chain::find_occurrence_positions;

/// Mirrors TS `OccurrenceReport`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceReport {
    pub count: usize,
    pub positions: Vec<usize>,
    pub ambiguous: bool,
}

/// Mirrors `reportOccurrences`.
pub fn report_occurrences(content: &str, actual: &str) -> OccurrenceReport {
    let positions = find_occurrence_positions(content, actual);
    let count = positions.len();
    OccurrenceReport {
        count,
        positions,
        ambiguous: count > 1,
    }
}

/// `[3, 7]` → `"line 3, line 7"`. Mirrors `formatLinePositions`.
pub fn format_line_positions(positions: &[usize]) -> String {
    positions
        .iter()
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}
