//! Non-blocking structural warnings — port of `src/domain/coherence.ts`
//! (`pi-semantic-edit`, MIT, (c) 2026 K2).
//!
//! Brace/bracket balance plus drastic indent jumps (>4 columns) versus the
//! previous non-empty line. Warnings never block an edit; the tool layer
//! surfaces them in the result text.

/// Mirrors `coherenceCheck`.
pub fn coherence_check(content: &str) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    let lines: Vec<&str> = content.split('\n').collect();

    let mut balance: i64 = 0;
    for ch in content.chars() {
        if ch == '{' || ch == '(' || ch == '[' {
            balance += 1;
        }
        if ch == '}' || ch == ')' || ch == ']' {
            balance -= 1;
        }
    }

    if balance > 0 {
        warnings.push(format!("Unclosed {balance} brace(s)/paren(s)/bracket(s)."));
    } else if balance < 0 {
        warnings.push(format!(
            "Too many closing braces/parens/brackets (excess: {}).",
            -balance
        ));
    }

    // --- Flag drastic indent jumps (>4 spaces) vs. previous non-empty line ---
    for i in 1..lines.len() {
        let Some(indent) = first_non_whitespace_col(lines[i]) else {
            continue;
        };
        let prev = find_prev_non_empty_line(&lines, i);
        if prev == -1 {
            continue;
        }
        if let Some(prev_indent) = first_non_whitespace_col(lines[prev as usize]) {
            if (indent as i64 - prev_indent as i64).abs() > 4 {
                warnings.push(format!(
                    "Line {} has suspicious indentation jump (from {prev_indent} to {indent} spaces).",
                    i + 1
                ));
            }
        }
    }

    warnings
}

/// Column of the first non-whitespace char, `None` for blank lines. Mirrors
/// `line.search(/\S/)` (which returns `-1` for none).
fn first_non_whitespace_col(line: &str) -> Option<usize> {
    line.chars().position(|c| !c.is_whitespace())
}

fn find_prev_non_empty_line(lines: &[&str], current_idx: usize) -> i64 {
    let mut i = current_idx as i64 - 1;
    while i >= 0 {
        if !lines[i as usize].trim().is_empty() {
            return i;
        }
        i -= 1;
    }
    -1
}
