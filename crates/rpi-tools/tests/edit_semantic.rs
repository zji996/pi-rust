//! Tool-level coverage of the `edit` tool's matching contract and of its
//! compiler-style failure reports.
//!
//! The contract is upstream Pi's two steps — exact text, then
//! `normalize_for_fuzzy_match` (trailing whitespace, typographic quotes/dashes)
//! — and nothing else. These tests pin the consequences: an `oldText` with
//! different indentation, escapes or middle lines never edits the file, an
//! ambiguous `oldText` is never resolved by guessing, and every failing edit is
//! reported in one round trip with line numbers, the kind of difference, and the
//! real text to copy.

use std::sync::Arc;

use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_tools::{ExecutionToolContext, FileSystem, InMemoryExecutionEnv};
use tokio_util::sync::CancellationToken;

fn fresh_context() -> (Arc<InMemoryExecutionEnv>, ExecutionToolContext) {
    let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
    let env_dyn: Arc<dyn rpi_tools::ExecutionEnv> = env.clone();
    let mut_env: Arc<dyn rpi_tools::MutatingEnv> = env.clone();
    let ctx = ExecutionToolContext::new(env_dyn, Some(mut_env));
    (env, ctx)
}

async fn seed(env: &InMemoryExecutionEnv, rel: &str, bytes: Vec<u8>) {
    let abs = env.absolute_path(rel, None).await.expect("abs");
    env.seed_file(&abs.to_string_lossy(), bytes).await;
}

async fn read_back(env: &InMemoryExecutionEnv, rel: &str) -> String {
    let abs = env.absolute_path(rel, None).await.expect("abs");
    env.read_text_file(&abs.to_string_lossy(), None)
        .await
        .expect("read")
}

async fn run_edit(
    tool: Arc<dyn AgentTool>,
    params: serde_json::Value,
) -> Result<rpi_agent::types::AgentToolResult, AgentError> {
    let signal = CancellationToken::new();
    let on_update = Arc::new(|_p| ());
    let prepared = tool.prepare_arguments(params.clone()).unwrap_or(params);
    tool.execute("edit-1", prepared, signal, on_update).await
}

fn tool_error(err: AgentError) -> String {
    match err {
        AgentError::Tool(m) => m,
        other => panic!("expected Tool error, got {other:?}"),
    }
}

fn text_output(r: &rpi_agent::types::AgentToolResult) -> String {
    use rpi_agent::types::TextContentOrImage;
    r.content
        .iter()
        .filter_map(|c| match c {
            TextContentOrImage::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One `differing lines` entry, laid out the way the reports do it:
/// `<line> <sign> <label padded to 11><text>`.
fn differing_entry(line: usize, sign: &str, label: &str, text: &str) -> String {
    format!("{line:>5} {sign} {label:<11}{text}")
}

/// Fail an edit and return the model-facing message, asserting that the file is
/// byte-identical afterwards.
async fn failed_edit(name: &str, original: &[u8], edits: serde_json::Value) -> String {
    let (env, ctx) = fresh_context();
    seed(&env, name, original.to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);
    let msg = tool_error(
        run_edit(tool, serde_json::json!({ "path": name, "edits": edits }))
            .await
            .expect_err("the edit must fail"),
    );
    assert_eq!(
        read_back(&env, name).await.as_bytes(),
        original,
        "the file must be untouched after a failed edit"
    );
    msg
}

// --- Matching is exact, plus the upstream normalization layer only -----------

#[tokio::test]
async fn indentation_only_difference_is_reported_with_lines_and_help_text() {
    let msg = failed_edit(
        "f.rs",
        b"fn a() {\n        let x = 1;\n}\n",
        serde_json::json!([{
            "oldText": "fn a() {\n    let x = 1;\n}",
            "newText": "fn a() {\n    let x = 2;\n}"
        }]),
    )
    .await;

    assert!(
        msg.contains("error: edits[0].oldText not found in f.rs"),
        "got: {msg}"
    );
    assert!(
        msg.contains(
            "  --> lines 1-3 are the closest match: only indentation differs \
             (file uses 8 spaces, oldText 4 spaces)"
        ),
        "got: {msg}"
    );
    // Differing lines, with file line numbers.
    let expected_old = format!(
        "  {}",
        differing_entry(2, "-", "oldText:", "    let x = 1;")
    );
    let expected_file = format!(
        "  {}",
        differing_entry(2, "+", "file:", "        let x = 1;")
    );
    assert!(
        msg.contains(&expected_old),
        "missing {expected_old:?} in: {msg}"
    );
    assert!(
        msg.contains(&expected_file),
        "missing {expected_file:?} in: {msg}"
    );
    // The current text, verbatim, ready to be copied into oldText.
    assert!(
        msg.contains("  | fn a() {\n  |         let x = 1;\n  | }"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn middle_line_difference_names_the_differing_line() {
    let msg = failed_edit(
        "f.rs",
        b"fn main() {\n    let x = 1;\n    println!(\"x\");\n}\n",
        serde_json::json!([{
            "oldText": "fn main() {\n    let x = 2;\n    println!(\"x\");\n}",
            "newText": "fn main() {\n    let x = 3;\n    println!(\"x\");\n}"
        }]),
    )
    .await;

    assert!(
        msg.contains("lines 1-4 are the closest match: first and last lines match, line 2 differs"),
        "got: {msg}"
    );
    assert!(
        msg.contains(&format!(
            "  {}",
            differing_entry(2, "-", "oldText:", "    let x = 2;")
        )),
        "got: {msg}"
    );
    assert!(
        msg.contains(&format!(
            "  {}",
            differing_entry(2, "+", "file:", "    let x = 1;")
        )),
        "got: {msg}"
    );
    assert!(
        msg.contains("  |     println!(\"x\");"),
        "the help must quote the real text, got: {msg}"
    );
}

#[tokio::test]
async fn escape_sequence_difference_is_named() {
    let msg = failed_edit(
        "f.txt",
        b"first\tsecond\n",
        serde_json::json!([{ "oldText": "first\\tsecond", "newText": "third" }]),
    )
    .await;

    assert!(
        msg.contains("the closest match: only escape sequences differ"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn unrelated_old_text_points_at_the_first_line_and_asks_for_a_reread() {
    // The file is too short for any candidate window, and nothing in the chain
    // matches: the only useful fact left is where the first line occurs.
    let content = b"fn main() {\n    let total = compute();\n}\n";
    let msg = failed_edit(
        "f.rs",
        content,
        serde_json::json!({
            "oldText": "fn main() {\nlet total = compute();\nreport(total);\nassert!(total > 0);\nreturn total;\nfinalize(total);",
            "newText": "fn main() {\n    report(compute());\n}"
        }),
    )
    .await;

    assert!(
        msg.contains("  --> no text in the file is similar to oldText"),
        "got: {msg}"
    );
    assert!(
        msg.contains("oldText's first line \"fn main() {\" appears at line 1"),
        "got: {msg}"
    );
    assert!(
        msg.contains("read the file again and copy its current text into oldText"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn long_regions_are_pointed_at_instead_of_quoted() {
    let content: String = (1..=80)
        .map(|i| format!("    call_{i}();\n"))
        .collect::<String>();
    let old: String = (1..=40)
        .map(|i| format!("    call_{}();\n", i + 100))
        .collect::<String>();
    let msg = failed_edit(
        "f.rs",
        content.as_bytes(),
        serde_json::json!([{ "oldText": old, "newText": "replaced();" }]),
    )
    .await;

    assert!(msg.contains("is too long to quote here"), "got: {msg}");
    assert!(msg.contains("offset 1, limit 40"), "got: {msg}");
}

// --- Ambiguity is reported, never resolved by guessing ----------------------

#[tokio::test]
async fn ambiguous_old_text_reports_every_line_number() {
    let msg = failed_edit(
        "f.txt",
        b"x\nold\ny\nold\nx\n",
        serde_json::json!([{ "oldText": "old", "newText": "new" }]),
    )
    .await;

    assert!(
        msg.contains("error: edits[0].oldText occurs 2 times in f.txt (lines 2, 4)"),
        "got: {msg}"
    );
    assert!(
        msg.contains("or set replaceAll: true to change every occurrence"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn ambiguous_edit_is_not_saved_by_surrounding_context() {
    // Context would single out the last "foo" upstream (auto-expand); here the
    // tool refuses to decide.
    let msg = failed_edit(
        "f.txt",
        b"foo\nfoo\nbar\n",
        serde_json::json!([{ "oldText": "foo", "newText": "FOO" }]),
    )
    .await;

    assert!(
        msg.contains("edits[0].oldText occurs 2 times in f.txt (lines 1, 2)"),
        "got: {msg}"
    );
}

#[tokio::test]
async fn replace_all_replaces_every_occurrence() {
    let (env, ctx) = fresh_context();
    seed(&env, "f.txt", b"old\nkeep\nold\nold\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "f.txt",
            "edits": [{ "oldText": "old", "newText": "new", "replaceAll": true }]
        }),
    )
    .await
    .expect("replaceAll should succeed");

    assert_eq!(read_back(&env, "f.txt").await, "new\nkeep\nnew\nnew\n");
    assert_eq!(
        text_output(&result),
        "Successfully replaced 3 block(s) in f.txt."
    );
}

// --- All failing edits are reported in one round trip -----------------------

#[tokio::test]
async fn every_failing_edit_is_reported_together() {
    let msg = failed_edit(
        "f.rs",
        b"fn a() {\n        let x = 1;\n}\n\nold\nmid\nold\n",
        serde_json::json!([
            {
                "oldText": "fn a() {\n    let x = 1;\n}",
                "newText": "fn a() {\n    let x = 2;\n}"
            },
            { "oldText": "old", "newText": "new" },
            { "oldText": "nothing like this", "newText": "x" }
        ]),
    )
    .await;

    for expected in [
        "error: edits[0].oldText not found in f.rs",
        "error: edits[1].oldText occurs 2 times in f.rs (lines 5, 7)",
        "error: edits[2].oldText not found in f.rs",
    ] {
        assert!(msg.contains(expected), "missing {expected:?} in: {msg}");
    }
}

#[tokio::test]
async fn overlapping_edits_name_both_regions() {
    let msg = failed_edit(
        "f.txt",
        b"one\ntwo\nthree\n",
        serde_json::json!([
            { "oldText": "one\ntwo\n", "newText": "ONE\nTWO\n" },
            { "oldText": "two\nthree\n", "newText": "TWO\nTHREE\n" }
        ]),
    )
    .await;

    assert!(
        msg.contains("error: edits[0] (lines 1-2) and edits[1] (lines 2-3) overlap in f.txt"),
        "got: {msg}"
    );
    assert!(msg.contains("merge them into one edit"), "got: {msg}");
}

#[tokio::test]
async fn empty_and_unnecessary_edits_are_one_liners() {
    let empty = failed_edit(
        "f.txt",
        b"alpha\n",
        serde_json::json!([{ "oldText": "", "newText": "x" }]),
    )
    .await;
    assert_eq!(
        empty,
        "error: edits[0].oldText is empty in f.txt - provide the exact text to replace"
    );

    let no_op = failed_edit(
        "f.txt",
        b"alpha\n",
        serde_json::json!([{ "oldText": "alpha", "newText": "alpha" }]),
    )
    .await;
    assert_eq!(
        no_op,
        "error: edits[0].oldText and newText are identical in f.txt - this edit would change nothing"
    );
}

// --- Success path: notes and warnings -------------------------------------

#[tokio::test]
async fn normalized_match_succeeds_with_a_note() {
    let (env, ctx) = fresh_context();
    // The file uses a typographic apostrophe and trailing spaces.
    seed(
        &env,
        "f.txt",
        "it\u{2019}s a file  \nkeep\n".as_bytes().to_vec(),
    )
    .await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "f.txt",
            "edits": [{ "oldText": "it's a file", "newText": "it is a file" }]
        }),
    )
    .await
    .expect("normalized match is upstream behavior and must still succeed");

    assert_eq!(read_back(&env, "f.txt").await, "it is a file\nkeep\n");
    assert_eq!(
        text_output(&result),
        "Successfully replaced 1 block(s) in f.txt.\n\
         note: edits[0] matched after normalizing trailing whitespace/quotes (line 1)"
    );
}

#[tokio::test]
async fn normalized_match_preserves_bom_and_crlf() {
    let (env, ctx) = fresh_context();
    let body = "\u{FEFF}old line\r\nkeep\r\nold line\r\n";
    seed(&env, "f.txt", body.as_bytes().to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    run_edit(
        tool,
        serde_json::json!({
            "path": "f.txt",
            "edits": [{ "oldText": "old line", "newText": "new line", "replaceAll": true }]
        }),
    )
    .await
    .expect("replaceAll ok");

    assert_eq!(
        read_back(&env, "f.txt").await,
        "\u{FEFF}new line\r\nkeep\r\nnew line\r\n"
    );
}

#[tokio::test]
async fn brace_balance_change_produces_a_warning() {
    let (env, ctx) = fresh_context();
    seed(&env, "f.rs", b"fn a() {\n    body();\n}\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "f.rs",
            "edits": [{ "oldText": "fn a() {\n    body();\n}", "newText": "body();\n}" }]
        }),
    )
    .await
    .expect("the edit applies; the imbalance is only a warning");

    assert_eq!(read_back(&env, "f.rs").await, "body();\n}\n");
    assert_eq!(
        text_output(&result),
        "Successfully replaced 1 block(s) in f.rs.\n\
         warning: edits[0] changes brace balance by -1 '}' (lines 1-2)"
    );
}

#[tokio::test]
async fn balanced_edit_produces_no_warning() {
    let (env, ctx) = fresh_context();
    seed(&env, "f.rs", b"let a = 1;\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let result = run_edit(
        tool,
        serde_json::json!({
            "path": "f.rs",
            "edits": [{ "oldText": "let a = 1;", "newText": "let a = compute(1, 2);" }]
        }),
    )
    .await
    .expect("edit ok");

    assert_eq!(
        text_output(&result),
        "Successfully replaced 1 block(s) in f.rs."
    );
}

// --- The tool description states the contract the model can rely on --------

#[tokio::test]
async fn tool_description_documents_exactness_and_replace_all() {
    let (_env, ctx) = fresh_context();
    let tool = rpi_tools::create_edit_tool(&ctx);
    let schema = tool.schema();
    let description = &schema.description;

    // The description says how to build an edit; each field states its own
    // constraint once.
    let fields = serde_json::to_string(&schema.parameters).expect("schema serializes");
    assert!(
        description.contains("Copy each oldText from read output, preserving whitespace"),
        "got: {description}"
    );
    assert!(
        fields.contains("It must occur exactly once in the file unless replaceAll is set"),
        "got: {fields}"
    );
    assert!(
        fields.contains("Replace every occurrence of oldText"),
        "got: {fields}"
    );
    for relaxed in ["tolerates", "fuzzy", "indentation drift", "10-pass"] {
        assert!(
            !description.contains(relaxed),
            "description must not promise relaxed matching ({relaxed}): {description}"
        );
    }

}
