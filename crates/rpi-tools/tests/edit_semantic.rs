//! Tool-level coverage of the ported `pi-semantic-edit` matcher behind the
//! built-in `edit` tool: `replaceAll`, ambiguity line positions, closest-text
//! errors, and BOM/CRLF preservation when many spans are replaced.
//!
//! Mirrors the behaviors documented in `pi-semantic-edit`'s README
//! (`src/domain/{editor,errors,apply}.ts`) at the `AgentTool` boundary, where
//! BOM/EOL handling and error surfacing actually happen.

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
    // The summary counts replaced spans, not edits.
    assert_eq!(
        text_output(&result),
        "Successfully replaced 3 block(s) in f.txt."
    );
    // All three spans are in the diff (the second edit's `old` and `new` have
    // the same length, so the diff carries one line per changed line).
    let diff = result.details["diff"].as_str().expect("diff");
    assert!(diff.contains("new"), "got: {diff}");
}

#[tokio::test]
async fn without_replace_all_multiple_matches_fail_with_line_positions() {
    let (env, ctx) = fresh_context();
    // Two occurrences with distinguishable context on each side: context
    // expansion cannot single one out, so this is a genuine ambiguity.
    seed(&env, "f.txt", b"x\nold\ny\nold\nx\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let msg = tool_error(
        run_edit(
            tool,
            serde_json::json!({
                "path": "f.txt",
                "edits": [{ "oldText": "old", "newText": "new" }]
            }),
        )
        .await
        .expect_err("ambiguous should fail"),
    );

    assert!(
        msg.contains("Text found 2 times at line 2, line 4 in f.txt."),
        "got: {msg}"
    );
    // The file is untouched.
    assert_eq!(read_back(&env, "f.txt").await, "x\nold\ny\nold\nx\n");
}

#[tokio::test]
async fn fuzzy_match_preserves_bom_and_crlf_with_replace_all() {
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
async fn fuzzy_whitespace_drift_is_absorbed_and_original_formatting_replaced() {
    let (env, ctx) = fresh_context();
    // The file uses deeper indentation than the edit's oldText.
    seed(&env, "f.rs", b"fn a() {\n        let x = 1;\n}\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    run_edit(
        tool,
        serde_json::json!({
            "path": "f.rs",
            "edits": [{ "oldText": "fn a() {\n    let x = 1;\n}", "newText": "fn a() {\n    let x = 2;\n}" }]
        }),
    )
    .await
    .expect("fuzzy edit ok");

    assert_eq!(
        read_back(&env, "f.rs").await,
        "fn a() {\n    let x = 2;\n}\n"
    );
}

#[tokio::test]
async fn no_match_error_quotes_the_closest_real_text() {
    let (env, ctx) = fresh_context();
    seed(&env, "f.rs", b"fn a() {\n    let x = 1;\n}\n".to_vec()).await;
    let tool = rpi_tools::create_edit_tool(&ctx);

    let msg = tool_error(
        run_edit(
            tool,
            serde_json::json!({
                "path": "f.rs",
                "edits": [{ "oldText": "    let x = 99;", "newText": "    let x = 2;" }]
            }),
        )
        .await
        .expect_err("no match should fail"),
    );

    assert!(
        msg.contains("Text not found in f.rs. The 10-pass fuzzy matcher found no match."),
        "got: {msg}"
    );
    assert!(msg.contains("Closest match ("), "got: {msg}");
    assert!(msg.contains("similar) at line 2"), "got: {msg}");
    assert!(msg.contains("let x = 1;"), "got: {msg}");
}

#[tokio::test]
async fn tool_description_and_replace_all_schema_field_are_the_ts_ones() {
    let (_env, ctx) = fresh_context();
    let tool = rpi_tools::create_edit_tool(&ctx);
    let schema = tool.schema();
    assert!(
        schema
            .description
            .starts_with("Edit a file by providing exact text replacements (edits[])"),
        "got: {}",
        schema.description
    );
    assert!(
        schema
            .description
            .contains("Set replaceAll: true on an edit to replace"),
        "got: {}",
        schema.description
    );
    let params = serde_json::to_string(&schema.parameters).expect("schema json");
    assert!(params.contains("replaceAll"), "got: {params}");
    assert!(
        params.contains("must be unique in the original file unless replaceAll is set"),
        "got: {params}"
    );
}
