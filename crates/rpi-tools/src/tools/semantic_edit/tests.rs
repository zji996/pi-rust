//! Unit tests for the ported semantic matcher. One case per pass, plus the
//! safety behaviors: ambiguity with line positions, `replaceAll`,
//! closest-candidate reporting, overlap/no-op rejection, and matching every
//! edit against the original file.

use crate::tools::semantic_edit::chain::{
    find_match, find_occurrence_positions, span_is_too_tall, span_line_budget,
};
use crate::tools::semantic_edit::closest::find_closest_candidate;
use crate::tools::semantic_edit::coherence::coherence_check;
use crate::tools::semantic_edit::editor::{apply_blocks, resolve_blocks};
use crate::tools::semantic_edit::passes::{block_anchor_find, multi_occurrence_find};
use crate::tools::semantic_edit::similarity::similarity;
use crate::tools::semantic_edit::types::{EditErrorKind, EditRequest};
use crate::tools::semantic_edit::uniqueness::{format_line_positions, report_occurrences};

fn req(old: &str, new: &str) -> EditRequest {
    EditRequest {
        path: "/file".to_string(),
        old_text: old.to_string(),
        new_text: new.to_string(),
        replace_all: false,
    }
}

fn req_all(old: &str, new: &str) -> EditRequest {
    EditRequest {
        replace_all: true,
        ..req(old, new)
    }
}

// --- Passes 1..=10: each case is matched by exactly one pass ---

#[test]
fn pass1_simple_matches_exact_text() {
    let m = find_match("let x = 1;\n", "let x = 1;").expect("match");
    assert_eq!(m.pass_name, "simple");
    assert_eq!(m.actual, "let x = 1;");
}

#[test]
fn pass2_line_trimmed_ignores_line_indentation() {
    let original = "  alpha\n    beta\n";
    let m = find_match(original, "alpha\nbeta").expect("match");
    assert_eq!(m.pass_name, "line_trimmed");
    // --- actual is the ORIGINAL substring, indentation preserved ---
    assert_eq!(m.actual, "  alpha\n    beta");
}

#[test]
fn pass3_block_anchor_matches_with_different_middle() {
    let original = "fn main() {\n    let x = 1;\n    println!(\"x\");\n}\n";
    let old = "fn main() {\n    let x = 2;\n    println!(\"x!\");\n}";
    let m = find_match(original, old).expect("match");
    assert_eq!(m.pass_name, "block_anchor");
    assert!(m.actual.starts_with("fn main() {"));
    assert!(m.actual.ends_with('}'));
}

#[test]
fn pass4_whitespace_normalized_collapses_runs() {
    let original = "let a  =  1; let b = 2;";
    let m = find_match(original, "let a = 1; let b = 2;").expect("match");
    assert_eq!(m.pass_name, "whitespace_normalized");
    assert_eq!(m.actual, original);
}

#[test]
fn pass5_indentation_flexible_skips_blank_lines() {
    let original = "    fn foo() {\n\n        body();\n    }\n";
    let m = find_match(original, "fn foo() {\nbody();").expect("match");
    assert_eq!(m.pass_name, "indentation_flexible");
    assert_eq!(m.actual, "    fn foo() {\n\n        body();");
}

#[test]
fn pass6_escape_normalized_unescapes_sequences() {
    let original = "a\tb\n";
    let m = find_match(original, "a\\tb").expect("match");
    assert_eq!(m.pass_name, "escape_normalized");
    assert_eq!(m.actual, "a\tb");
}

#[test]
fn pass7_trimmed_boundary_strips_outer_whitespace() {
    let original = "before foo(1,\n 2) after\n";
    let m = find_match(original, "  foo(1,\n 2)  ").expect("match");
    assert_eq!(m.pass_name, "trimmed_boundary");
    assert_eq!(m.actual, "foo(1,\n 2)");
}

#[test]
fn pass8_context_aware_uses_first_last_line_anchors() {
    let original = "    resolve the target now\n    and then write it out\n";
    let m = find_match(original, "resolve the target\nand then write it").expect("match");
    assert_eq!(m.pass_name, "context_aware");
    assert_eq!(
        m.actual,
        "    resolve the target now\n    and then write it out"
    );
}

#[test]
fn pass9_multi_occurrence_matches_trimmed_lines() {
    // --- Reached only as a last resort: earlier passes cover every case where
    // the trimmed line sequence is contiguous, so exercise the pass directly
    // (same situation as the TS chain). ---
    let original = "  alpha\n\n  beta\n";
    assert_eq!(
        multi_occurrence_find(original, "alpha\n\nbeta"),
        Some("  alpha\n\n  beta".to_string())
    );
    assert_eq!(multi_occurrence_find(original, "gamma"), None);
}

#[test]
fn pass10_unicode_normalized_maps_typographic_chars_back_to_original() {
    let original = "it\u{2019}s \u{FB01}ne\n";
    let m = find_match(original, "it's fine").expect("match");
    assert_eq!(m.pass_name, "unicode_normalized");
    // --- ligature U+FB01 expands to "fi": the span must cover the real bytes ---
    assert_eq!(m.actual, "it\u{2019}s \u{FB01}ne");
}

#[test]
fn chain_falls_through_every_pass_when_nothing_matches() {
    assert!(find_match("alpha\nbeta\n", "gamma\ndelta").is_none());
    assert!(find_match("alpha\n", "alpha").is_some());
}

// --- Occurrence positions / uniqueness ---

#[test]
fn occurrence_positions_are_one_indexed_lines() {
    assert_eq!(
        find_occurrence_positions("dup\nmid\ndup\n", "dup"),
        vec![1, 3]
    );
    assert_eq!(format_line_positions(&[3, 7]), "line 3, line 7");
    let report = report_occurrences("dup\nmid\ndup\n", "dup");
    assert!(report.ambiguous);
    assert_eq!(report.count, 2);
}

#[test]
fn ambiguous_edit_fails_with_every_line_position() {
    // --- Both occurrences look the same with one line of context on each side,
    // so context expansion cannot pick one: report every position. ---
    let content = "x\ndup\ny\ndup\nx\n";
    let err = resolve_blocks(content, &[req("dup", "x")], "/file").expect_err("ambiguous");
    assert_eq!(err.kind, EditErrorKind::Ambiguous);
    assert_eq!(err.line_positions.as_deref(), Some(&[2usize, 4][..]));
    assert!(
        err.message
            .contains("Text found 2 times at line 2, line 4 in /file."),
        "got: {}",
        err.message
    );
    assert!(
        err.message.contains("set replaceAll: true"),
        "got: {}",
        err.message
    );
}

#[test]
fn ambiguous_edit_fails_when_context_expansion_cannot_disambiguate() {
    // --- Both occurrences are on the same line; expanding context leaves more
    // than one unique block, so this is genuine ambiguity. ---
    let content = "foo foo foo";
    let err = resolve_blocks(content, &[req("foo", "bar")], "/file").expect_err("ambiguous");
    assert_eq!(err.kind, EditErrorKind::Ambiguous);
    assert_eq!(err.line_positions.as_deref(), Some(&[1usize, 1, 1][..]));
}

#[test]
fn auto_expand_locates_the_one_occurrence_context_can_identify() {
    // --- The first "foo" is indistinguishable; only the last one can be made
    // unique by extension, mirroring the TS auto-expand. ---
    let content = "foo\nfoo\nbar\n";
    let outcome = apply_blocks(content, &[req("foo", "FOO")], "/file").expect("resolved");
    assert_eq!(outcome.new_content, "foo\nFOO\nbar\n");
    assert_eq!(outcome.match_passes, vec!["auto_expand"]);
}

// --- replaceAll ---

#[test]
fn replace_all_replaces_every_occurrence() {
    let content = "foo\nbar\nfoo\n";
    let outcome = apply_blocks(content, &[req_all("foo", "baz")], "/file").expect("resolved");
    assert_eq!(outcome.new_content, "baz\nbar\nbaz\n");
    assert_eq!(outcome.replacements, 2);
    assert_eq!(outcome.match_passes, vec!["replace_all", "replace_all"]);
}

#[test]
fn replace_all_defaults_to_unique_only() {
    let content = "foo\nbar\nfoo\nbaz\nfoo\n";
    let err = resolve_blocks(content, &[req("foo", "baz")], "/file").expect_err("ambiguous");
    assert_eq!(err.kind, EditErrorKind::Ambiguous);
    assert_eq!(err.line_positions.as_deref(), Some(&[1usize, 3, 5][..]));
}

// --- Closest candidate on no-match ---

#[test]
fn not_found_reports_closest_text_and_similarity() {
    let content = "fn main() {\n    let value = 42;\n}\n";
    let err =
        resolve_blocks(content, &[req("    let value = 43;", "x")], "/file").expect_err("no match");
    assert_eq!(err.kind, EditErrorKind::NotFound);
    assert!(
        err.message
            .contains("Text not found in /file. The 10-pass fuzzy matcher found no match."),
        "got: {}",
        err.message
    );
    assert!(
        err.message
            .contains("Closest match (93% similar) at line 2:"),
        "got: {}",
        err.message
    );
    assert!(
        err.message.contains("let value = 42;"),
        "got: {}",
        err.message
    );
    let closest = err.closest_candidate.expect("closest candidate");
    assert_eq!(closest.start_line, 2);
    assert_eq!(closest.end_line, 2);
    assert_eq!(closest.candidate, "    let value = 42;");
}

#[test]
fn not_found_without_any_similar_text_says_so() {
    let content = "alpha\nbeta\n";
    let err = resolve_blocks(content, &[req("zzzz", "x")], "/file").expect_err("no match");
    assert_eq!(err.kind, EditErrorKind::NotFound);
    assert!(
        err.message.contains("No similar text found either."),
        "got: {}",
        err.message
    );
    assert!(find_closest_candidate(content, "zzzz", 50).is_none());
}

#[test]
fn closest_candidate_similarity_matches_ts_ratio() {
    // --- 2 * LCS / (len_a + len_b) ---
    assert_eq!(similarity("abcd", "abcd"), 1.0);
    assert_eq!(similarity("abcd", ""), 0.0);
    assert_eq!(similarity("abcd", "abce"), 0.75);
}

// --- Overlap / no-op ---

#[test]
fn overlapping_edits_are_rejected() {
    let content = "one\ntwo\nthree\n";
    let err = apply_blocks(
        content,
        &[req("one\ntwo\n", "A"), req("two\nthree\n", "B")],
        "/file",
    )
    .expect_err("overlap");
    assert_eq!(err.kind, EditErrorKind::Overlapping);
    assert!(
        err.message
            .contains("Overlapping edits: overlaps the edit at 0..8"),
        "got: {}",
        err.message
    );
}

#[test]
fn nested_edits_are_rejected() {
    let content = "abc\n";
    let err =
        apply_blocks(content, &[req("abc", "x"), req("ab", "y")], "/file").expect_err("nested");
    assert_eq!(err.kind, EditErrorKind::Overlapping);
    assert!(
        err.message
            .contains("Each edits[].oldText must target a disjoint region"),
        "got: {}",
        err.message
    );
}

#[test]
fn identical_old_and_new_text_is_rejected() {
    let err = resolve_blocks("alpha\n", &[req("alpha", "alpha")], "/file").expect_err("no-op");
    assert_eq!(err.kind, EditErrorKind::Validation);
    assert!(
        err.message
            .contains("oldText and newText are identical; this edit does nothing."),
        "got: {}",
        err.message
    );
}

#[test]
fn empty_old_text_is_rejected() {
    let err = resolve_blocks("alpha\n", &[req("", "x")], "/file").expect_err("empty");
    assert_eq!(err.kind, EditErrorKind::Validation);
    assert!(
        err.message
            .contains("oldText is empty; provide the exact text to find."),
        "got: {}",
        err.message
    );
}

// --- Span-height budget: a fuzzy pass may not reach into a neighbour ---

#[test]
fn span_line_budget_is_ten_percent_floored_at_one() {
    assert_eq!(span_line_budget(1), 1);
    assert_eq!(span_line_budget(5), 1);
    assert_eq!(span_line_budget(10), 1);
    assert_eq!(span_line_budget(11), 2);
    assert_eq!(span_line_budget(20), 2);
    assert_eq!(span_line_budget(21), 3);
    assert!(!span_is_too_tall("a\nb", "a\nb"));
    assert!(span_is_too_tall("a\nb\nc\nd", "a\nb"));
}

#[test]
fn fuzzy_pass_may_not_swallow_an_adjacent_function() {
    // --- The edit remembers a longer `alpha`, so `block_anchor`'s only
    // first/last-line candidate starts at `alpha` and ends at the closing brace
    // of the *next* function — a span covering both. ---
    let content = "fn alpha() {\n    a1();\n}\n\nfn beta() {\n    b1();\n    b2();\n}\n\nfn gamma() {\n    c1();\n}\n";
    let old = "fn alpha() {\n    a1();\n    a2();\n    a3();\n}";

    // The pass itself still offers the over-wide span; that is the hazard the
    // chain has to reject.
    let wide = block_anchor_find(content, old).expect("block_anchor offers a span");
    assert!(
        wide.contains("fn beta()"),
        "pass-level span should swallow the neighbour: {wide:?}"
    );
    assert!(span_is_too_tall(&wide, old));

    // The chain refuses it and the tighter `context_aware` pass answers with the
    // alpha block only.
    let m = find_match(content, old).expect("a tighter pass still matches");
    assert_eq!(m.pass_name, "context_aware");
    assert_eq!(m.actual, "fn alpha() {\n    a1();\n}");

    let outcome = apply_blocks(
        content,
        &[req(old, "fn alpha() {\n    a1_new();\n}")],
        "/file",
    )
    .expect("edit ok");
    assert_eq!(
        outcome.new_content,
        "fn alpha() {\n    a1_new();\n}\n\nfn beta() {\n    b1();\n    b2();\n}\n\nfn gamma() {\n    c1();\n}\n"
    );
    // The neighbours are untouched byte for byte.
    assert!(outcome
        .new_content
        .contains("fn beta() {\n    b1();\n    b2();\n}"));
    assert!(outcome.new_content.contains("fn gamma() {\n    c1();\n}"));
}

#[test]
fn over_wide_only_candidate_reports_not_found_and_leaves_the_file_alone() {
    // --- Here the only candidate is the wide span (alpha's closing brace is
    // missing, so the next `}` is beta's); no tighter pass matches. The edit
    // must fail as not-found — never touch beta. ---
    let content = "fn alpha() {\n    a1();\n    a2();\n\nfn beta() {\n    b1();\n}\n";
    let old = "fn alpha() {\n    x1();\n    x2();\n}";

    let wide = block_anchor_find(content, old).expect("block_anchor offers a span");
    assert!(wide.contains("fn beta()"), "{wide:?}");
    assert!(span_is_too_tall(&wide, old));

    assert!(
        find_match(content, old).is_none(),
        "nothing tighter matches"
    );

    let err = apply_blocks(content, &[req(old, "fn alpha() {\n    y1();\n}")], "/file")
        .expect_err("must not edit");
    assert_eq!(err.kind, EditErrorKind::NotFound);
    assert!(
        err.message.contains("Closest match ("),
        "got: {}",
        err.message
    );
    // Still-true invariant: the neighbour was never a target.
    assert!(content.contains("fn beta() {\n    b1();\n}"));
}

// --- Every edit is matched against the ORIGINAL content ---

#[test]
fn edits_match_the_original_not_the_intermediate_result() {
    let content = "a\nb\n";
    // --- If edits were applied incrementally, the second edit could target the
    // "b" that the first edit introduces. Both must resolve against the
    // original instead. ---
    let outcome =
        apply_blocks(content, &[req("a", "x\nb"), req("b", "c")], "/file").expect("resolved");
    assert_eq!(outcome.new_content, "x\nb\nc\n");
}

#[test]
fn crlf_in_new_text_is_normalized_before_splicing() {
    let content = "a\nb\n";
    let outcome = apply_blocks(content, &[req("b", "B\r\nC")], "/file").expect("resolved");
    assert_eq!(outcome.new_content, "a\nB\nC\n");
}

// --- Coherence warnings (non-blocking) ---

#[test]
fn coherence_check_flags_unbalanced_braces_and_indent_jumps() {
    let warnings = coherence_check("fn main() {\n            body();\n");
    assert!(
        warnings
            .iter()
            .any(|w| w == "Unclosed 1 brace(s)/paren(s)/bracket(s)."),
        "got: {warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("suspicious indentation jump")),
        "got: {warnings:?}"
    );
    assert!(coherence_check("fn main() {\n    body();\n}\n").is_empty());
}

#[test]
fn balanced_content_produces_no_warnings() {
    assert!(coherence_check("a\nb\n").is_empty());
}
