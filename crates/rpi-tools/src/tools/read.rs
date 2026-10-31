//! Mirrors `packages/agent/src/harness/tools/read.ts` — the built-in `read`
//! `AgentTool`. Reads a file (text or image), applies head-truncation with a
//! continuation hint, and detects supported image MIME types. An optional
//! `ReadImageProcessor` seam supports resize/convert implementations; without
//! one, BMP images yield an omitted notice and other formats are inlined.

use std::sync::Arc;

use async_trait::async_trait;
use rpi_agent::agent_tool::AgentTool;
use rpi_agent::error::AgentError;
use rpi_agent::types::{AgentToolResult, TextContentOrImage, ToolResultPartial};
use rpi_ai::types::{
    ConstrainedSamplingConfig, ConstrainedStrictness, ImageContent, ImageContentType, Tool,
};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::env::ExecutionEnv;
use crate::file_io::{
    resolve_client_path, FileIOOperation, FileIOReadRange, FileIORouter, FileIOSnapshot,
};
use crate::image::{detect_supported_image_mime_type, encode_base64};
use crate::path_utils::resolve_read_tool_path;
use crate::tools::tool_context::ExecutionToolContext;
use crate::truncate::{
    format_size, truncate_head, TruncationOptions, TruncationResult, DEFAULT_MAX_BYTES,
    DEFAULT_MAX_LINES,
};
// Input for the read tool. Mirrors TS `ReadToolInput`. `offset` is 1-indexed.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
pub struct ReadInput {
    /// Path to the file to read (relative or absolute).
    pub path: String,
    /// Line number to start reading from (1-indexed).
    #[serde(default)]
    pub offset: Option<u32>,
    /// Maximum number of lines to read.
    #[serde(default)]
    pub limit: Option<u32>,
    /// `readImageDraft` only: the part of the image to show, as
    /// `[left, top, right, bottom]` in 0-1000 coordinates relative to the whole
    /// image. Parsed by [`ImageBox::parse`] so a malformed value gets one line
    /// saying what to send; left out of the schema here and injected by
    /// [`ReadTool::schema`] when the setting is on.
    #[serde(default, rename = "box")]
    #[schemars(skip)]
    pub r#box: Option<serde_json::Value>,
}

/// Structured details for the read tool. Mirrors TS `ReadToolDetails`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ReadToolDetails {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationResult>,
}

/// Options for `create_read_tool`. Mirrors TS `ReadToolOptions`.
pub struct ReadToolOptions {
    /// Whether an injected image processor should resize images. Default true.
    pub auto_resize_images: bool,
    /// `readImageDraft` (aia): read an image as a ~0.4 MP draft and let `box`
    /// show one region of it at full clarity. Off (the default) the schema,
    /// the description, the result text and the bytes are upstream's.
    pub image_draft: bool,
    /// Optional image conversion/resizing implementation.
    pub image_processor: Option<Arc<dyn ReadImageProcessor>>,
}

impl Clone for ReadToolOptions {
    fn clone(&self) -> Self {
        Self {
            auto_resize_images: self.auto_resize_images,
            image_draft: self.image_draft,
            image_processor: self.image_processor.clone(),
        }
    }
}

impl std::fmt::Debug for ReadToolOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadToolOptions")
            .field("auto_resize_images", &self.auto_resize_images)
            .field("image_draft", &self.image_draft)
            .field("image_processor", &self.image_processor.is_some())
            .finish()
    }
}

impl Default for ReadToolOptions {
    fn default() -> Self {
        Self {
            auto_resize_images: true,
            image_draft: false,
            image_processor: None,
        }
    }
}

/// Optional image-conversion seam. Mirrors TS `ReadImageProcessor`.
pub trait ReadImageProcessor: Send + Sync {
    fn process(
        &self,
        bytes: &[u8],
        mime_type: &str,
        auto_resize: bool,
    ) -> Result<ProcessedImage, String>;

    /// [`ReadImageProcessor::process`] for a draft or a `box` view
    /// (`readImageDraft`). The default refuses rather than answering a `box`
    /// request with the whole image, which would tell the model it saw detail it
    /// did not get.
    fn process_view(
        &self,
        _bytes: &[u8],
        _mime_type: &str,
        _view: ImageView,
    ) -> Result<ProcessedImage, String> {
        Err("this image processor cannot show a draft or a box".to_string())
    }
}

/// A rectangle of an image in 0-1000 coordinates, always relative to the whole
/// (EXIF-oriented) image, the way Norvane's avatar `box` works. `[0,0,1000,1000]`
/// is the whole image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBox {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl ImageBox {
    /// What a `box` argument has to look like, as one line: a malformed `box`
    /// fails the call with exactly this, so it says the shape and gives the
    /// example the model most likely wanted.
    pub const USAGE: &'static str = "box must be [left, top, right, bottom], four whole numbers 0-1000 relative to the whole image with left < right and top < bottom; box=[0,0,1000,1000] is the whole image.";

    /// Parse the `box` argument, or return [`ImageBox::USAGE`].
    pub fn parse(value: &serde_json::Value) -> Result<Self, String> {
        let items = value
            .as_array()
            .filter(|items| items.len() == 4)
            .ok_or(Self::USAGE)?;
        let mut edge = [0u32; 4];
        for (slot, item) in edge.iter_mut().zip(items) {
            *slot = item.as_u64().filter(|v| *v <= 1000).ok_or(Self::USAGE)? as u32;
        }
        let region = Self {
            left: edge[0],
            top: edge[1],
            right: edge[2],
            bottom: edge[3],
        };
        if region.left >= region.right || region.top >= region.bottom {
            return Err(Self::USAGE.to_string());
        }
        Ok(region)
    }

    /// The pixel rectangle `(x, y, width, height)` this box covers, rounded
    /// outwards so the box never cuts off a pixel it names; on a
    /// `width`×`height` image `[0,0,1000,1000]` is exactly the whole image.
    pub fn pixels(&self, width: u32, height: u32) -> (u32, u32, u32, u32) {
        let edge = |value: u32, total: u32, outward: bool| -> u32 {
            let scaled = value as u64 * total as u64;
            let pixels = if outward {
                scaled.div_ceil(1000)
            } else {
                scaled / 1000
            };
            pixels.min(total as u64) as u32
        };
        let x = edge(self.left, width, false);
        let y = edge(self.top, height, false);
        let right = edge(self.right, width, true);
        let bottom = edge(self.bottom, height, true);
        (x, y, right - x, bottom - y)
    }
}

/// Which view of an image `read` returns. Only `Whole` is reachable with
/// `readImageDraft` off, and it is the upstream behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageView {
    /// The whole image under the inline limits (upstream).
    Whole,
    /// The whole image shrunk to at most ~0.4 MP.
    Draft,
    /// The box, cropped from the original resolution, under the inline limits.
    Box(ImageBox),
}

/// Result of a successful [`ReadImageProcessor::process`].
pub struct ProcessedImage {
    pub data: String,
    pub mime_type: String,
    pub hints: Vec<String>,
    /// One line for after the image: the draft/`box` modes report the original
    /// size, what is shown and what to read next. `None` upstream.
    pub note: Option<String>,
}

/// The built-in `read` tool. Holds an `Arc<dyn ExecutionEnv>` (read-only view)
/// and an optional image processor.
pub struct ReadTool {
    schema: Tool,
    env: Arc<dyn ExecutionEnv>,
    file_io: Option<Arc<FileIORouter>>,
    options: ReadToolOptions,
    image_processor: Option<Arc<dyn ReadImageProcessor>>,
}

impl ReadTool {
    /// The tool as the model sees it. `image_draft` off is upstream's schema and
    /// description, byte for byte (a test locks both); on, `box` joins the
    /// parameters and one sentence joins the description.
    fn schema(image_draft: bool) -> Tool {
        let mut params = serde_json::to_value(schemars::schema_for!(ReadInput)).unwrap_or_default();
        let mut description = format!(
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
             Images are sent as attachments. For text files, output is truncated to {DEFAULT_MAX_LINES} \
             lines or {kb}KB (whichever is hit first). Use offset/limit for large files. When you need \
             the full file, continue with offset until complete.",
            kb = DEFAULT_MAX_BYTES / 1024
        );
        if image_draft {
            description.push_str(
                " An image is shown as a whole-view draft; pass box=[left, top, right, bottom] (0-1000, \
                 relative to the whole image) to see that region at full clarity.",
            );
            if let Some(properties) = params.get_mut("properties").and_then(|p| p.as_object_mut()) {
                properties.insert(
                    "box".to_string(),
                    serde_json::json!({
                        "type": "array",
                        "description": "The part of the image to show instead of the whole draft: [left, top, right, bottom], each 0-1000 relative to the whole image (box=[0,0,1000,1000] is the whole image).",
                        "items": { "type": "integer", "minimum": 0, "maximum": 1000 },
                        "minItems": 4,
                        "maxItems": 4,
                    }),
                );
            }
        }
        Tool {
            name: "read".to_string(),
            description,
            parameters: rpi_ai::types::Schema::new(params),
            constrained_sampling: Some(ConstrainedSamplingConfig::JsonSchema {
                strict: ConstrainedStrictness::Prefer,
            }),
        }
    }
}

/// Build the `read` tool from a context. Mirrors TS `createReadTool`.
pub fn create_read_tool(
    context: &ExecutionToolContext,
    options: Option<ReadToolOptions>,
) -> Arc<dyn AgentTool> {
    let options = options.unwrap_or_default();
    let image_processor = options.image_processor.clone();
    let schema = ReadTool::schema(options.image_draft);
    Arc::new(ReadTool {
        schema,
        env: context.env().clone(),
        file_io: context.file_io().cloned(),
        options,
        image_processor,
    })
}

#[async_trait]
impl AgentTool for ReadTool {
    fn schema(&self) -> &Tool {
        &self.schema
    }
    fn label(&self) -> &str {
        "read"
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        params: serde_json::Value,
        signal: CancellationToken,
        _on_update: Arc<dyn Fn(ToolResultPartial) + Send + Sync>,
    ) -> Result<AgentToolResult, AgentError> {
        let input: ReadInput = serde_json::from_value(params)
            .map_err(|e| AgentError::Validation(format!("read input invalid: {e}")))?;

        if let Some(router) = &self.file_io {
            if let Some(client) = router
                .select(FileIOOperation::Read)
                .await
                .map_err(|e| file_err_to_agent(e.into()))?
            {
                let abs = resolve_client_path(self.env.cwd(), &input.path)
                    .map_err(|e| file_err_to_agent(e.into()))?;
                // Text only: never sniff extensions or fetch host image bytes.
                return client_text_result(&client, &abs, &input, &signal).await;
            }
        }
        // `readImageDraft`: an image is read as a whole-view draft and `box`
        // asks for one region of it. Off, `box` is an unknown argument (ignored,
        // like every other unknown one) and the view is upstream's.
        let view = if self.options.image_draft {
            match input.r#box.as_ref() {
                Some(value) => ImageView::Box(ImageBox::parse(value).map_err(AgentError::Tool)?),
                None => ImageView::Draft,
            }
        } else {
            ImageView::Whole
        };

        let abs = resolve_read_tool_path(&*self.env, &input.path, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;
        let bytes = self
            .env
            .read_binary_file(&abs, Some(&signal))
            .await
            .map_err(file_err_to_agent)?;

        // Image path.
        if let Some(mime) = detect_supported_image_mime_type(&bytes) {
            return Ok(image_result(
                mime,
                &bytes,
                &self.options,
                self.image_processor.as_deref(),
                view,
            ));
        }

        // Text path.
        if matches!(view, ImageView::Box(_)) {
            return Err(AgentError::Tool(
                "box applies to images only; drop box to read a text file.".to_string(),
            ));
        }
        let text_content = String::from_utf8_lossy(&bytes).into_owned();
        let all_lines: Vec<&str> = text_content.split('\n').collect();
        // Mirror TS: `textContent.split("\n")` keeps a trailing "" when the
        // content ends with '\n'; the line-count + slice math below uses the raw
        // split (NOT `split_lines_for_counting`), so we keep it as-is.
        let total_file_lines = all_lines.len();
        let start_line = input
            .offset
            .map(|o| (o.saturating_sub(1)) as usize)
            .unwrap_or(0);
        let start_line_display = start_line + 1;
        if start_line >= all_lines.len() {
            return Err(AgentError::Tool(format!(
                "Offset {} is beyond end of file ({} lines total)",
                input.offset.unwrap_or(1),
                all_lines.len()
            )));
        }

        let (selected_content, user_limited_lines): (String, Option<usize>) =
            if let Some(limit) = input.limit {
                let end_line = std::cmp::min(start_line + limit as usize, all_lines.len());
                (
                    all_lines[start_line..end_line].join("\n"),
                    Some(end_line - start_line),
                )
            } else {
                (all_lines[start_line..].join("\n"), None)
            };

        let truncation = truncate_head(&selected_content, TruncationOptions::default());
        let (output_text, details): (String, Option<TruncationResult>) = if truncation
            .first_line_exceeds_limit
        {
            let first_line_size = format_size(all_lines[start_line].as_bytes().len());
            (
                    format!(
                        "[Line {start_line_display} is {first_line_size}, exceeds {} limit. \
                         Use bash: sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                        format_size(DEFAULT_MAX_BYTES),
                        input.path
                    ),
                    Some(truncation),
                )
        } else if truncation.truncated {
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            let mut text = truncation.content.clone();
            match truncation.truncated_by {
                Some(crate::truncate::TruncationLimit::Lines) => {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines}. Use offset={next_offset} to continue.]"
                    ));
                }
                _ => {
                    text.push_str(&format!(
                        "\n\n[Showing lines {start_line_display}-{end_line_display} of \
                             {total_file_lines} ({} limit). Use offset={next_offset} to continue.]",
                        format_size(DEFAULT_MAX_BYTES)
                    ));
                }
            }
            (text, Some(truncation))
        } else if let Some(ull) = user_limited_lines {
            if start_line + ull < all_lines.len() {
                let remaining = all_lines.len() - (start_line + ull);
                let next_offset = start_line + ull + 1;
                (
                        format!(
                            "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                            truncation.content
                        ),
                        None,
                    )
            } else {
                (truncation.content, None)
            }
        } else {
            (truncation.content, None)
        };

        let details_value = match details {
            Some(t) => serde_json::to_value(ReadToolDetails {
                truncation: Some(t),
            })
            .unwrap_or(serde_json::Value::Null),
            None => serde_json::Value::Null,
        };
        Ok(AgentToolResult {
            content: vec![TextContentOrImage::text(output_text)],
            details: details_value,
            ..Default::default()
        })
    }
}

/// The backend has already applied offset. The sentinel proves only that more
/// content exists; neither its length nor truncation totals are whole-file
/// metadata. EOF (including an offset beyond EOF) is a valid empty range.
async fn client_text_result(
    client: &FileIOSnapshot,
    absolute_path: &str,
    input: &ReadInput,
    signal: &CancellationToken,
) -> Result<AgentToolResult, AgentError> {
    let offset = input.offset.unwrap_or(1).max(1);
    let display_lines = input
        .limit
        .map(|n| n as usize)
        .unwrap_or(DEFAULT_MAX_LINES)
        .min(DEFAULT_MAX_LINES);
    let text = client
        .read_text(
            absolute_path,
            FileIOReadRange::Lines {
                offset_1based: offset,
                limit: display_lines as u32 + 1,
            },
            signal,
        )
        .await
        .map_err(|e| file_err_to_agent(e.into()))?;
    let lines = crate::truncate::split_lines_for_counting(&text);
    let has_sentinel = lines.len() > display_lines;
    let selected = if has_sentinel {
        let selected_lines = &lines[..display_lines];
        let mut selected = selected_lines.join("\n");
        // A final blank source line needs its terminator to remain countable.
        // Otherwise a one-blank-line page becomes empty and never advances.
        if selected_lines.last() == Some(&"") {
            selected.push('\n');
        }
        selected
    } else {
        text
    };
    let truncation = truncate_head(
        &selected,
        TruncationOptions {
            max_lines: Some(display_lines),
            max_bytes: Some(DEFAULT_MAX_BYTES),
        },
    );
    let mut output = truncation.content.clone();
    if truncation.first_line_exceeds_limit {
        output = format!(
            "[Line {offset} exceeds {} display limit. Client text was not displayed.]",
            format_size(DEFAULT_MAX_BYTES)
        );
    } else if has_sentinel || truncation.truncated {
        let next = u64::from(offset) + truncation.output_lines as u64;
        output.push_str(&format!("\n\n[More content may remain; total line count unknown. Use offset={next} to continue.]"));
    }
    // Native TruncationResult.total_lines describes its bounded input, not the
    // client file. Do not serialize it as misleading file metadata.
    Ok(AgentToolResult {
        content: vec![TextContentOrImage::text(output)],
        details: serde_json::Value::Null,
        ..Default::default()
    })
}

/// Build the image-path result, honoring an optional processor. Mirrors the TS
/// image branch; `view` other than [`ImageView::Whole`] is the `readImageDraft`
/// case, where the processor's one-line note follows the image.
fn image_result(
    mime: &str,
    bytes: &[u8],
    options: &ReadToolOptions,
    processor: Option<&dyn ReadImageProcessor>,
    view: ImageView,
) -> AgentToolResult {
    // A draft or a box without a processor cannot be honored: upstream would
    // inline the whole image, which the schema has just promised it will not do.
    if processor.is_none() && view != ImageView::Whole {
        return AgentToolResult {
            content: vec![TextContentOrImage::text(format!(
                "Read image file [{mime}]\n[Image omitted: no image processor is configured for drafts or boxes.]"
            ))],
            details: serde_json::Value::Null,
            ..Default::default()
        };
    }
    if let Some(proc) = processor {
        let processed = match view {
            ImageView::Whole => proc.process(bytes, mime, options.auto_resize_images),
            view => proc.process_view(bytes, mime, view),
        };
        match processed {
            Ok(p) => {
                let hints = if p.hints.is_empty() {
                    String::new()
                } else {
                    format!("\n{}", p.hints.join("\n"))
                };
                let mut content = vec![
                    TextContentOrImage::text(format!("Read image file [{}]{}", p.mime_type, hints)),
                    TextContentOrImage::Image(ImageContent {
                        kind: ImageContentType,
                        data: p.data,
                        mime_type: p.mime_type,
                    }),
                ];
                if let Some(note) = p.note {
                    content.push(TextContentOrImage::text(note));
                }
                return AgentToolResult {
                    content,
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
            Err(message) => {
                return AgentToolResult {
                    content: vec![TextContentOrImage::text(format!(
                        "Read image file [{mime}]\n{message}"
                    ))],
                    details: serde_json::Value::Null,
                    ..Default::default()
                };
            }
        }
    }
    // No processor — BMP is omitted; others are inlined.
    if mime == "image/bmp" {
        return AgentToolResult {
            content: vec![TextContentOrImage::text(
                "Read image file [image/bmp]\n[Image omitted: configure an imageProcessor to convert BMP images.]",
            )],
            details: serde_json::Value::Null,
            ..Default::default()
        };
    }
    AgentToolResult {
        content: vec![
            TextContentOrImage::text(format!("Read image file [{mime}]")),
            TextContentOrImage::Image(ImageContent {
                kind: ImageContentType,
                data: encode_base64(bytes),
                mime_type: mime.to_string(),
            }),
        ],
        details: serde_json::Value::Null,
        ..Default::default()
    }
}

/// Map a `FileError` into an `AgentError::Tool`, mirroring the TS `getOrThrow`
/// surface (the message carries the path + code).
fn file_err_to_agent(e: crate::error::FileError) -> AgentError {
    AgentError::Tool(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env::FileSystem;
    use crate::{ExecutionToolContext, InMemoryExecutionEnv};

    /// A tool context rooted at `/tmp/work`, seeded through the in-memory env so
    /// no test writes to the disk.
    async fn context(files: &[(&str, &[u8])]) -> ExecutionToolContext {
        let env = Arc::new(InMemoryExecutionEnv::with_cwd("/tmp/work".into()));
        for (rel, bytes) in files {
            let abs = env
                .absolute_path(rel, None)
                .await
                .expect("absolute_path for seed");
            env.seed_file(&abs.to_string_lossy(), bytes.to_vec()).await;
        }
        let read: Arc<dyn ExecutionEnv> = env.clone();
        let write: Arc<dyn crate::MutatingEnv> = env;
        ExecutionToolContext::new(read, Some(write))
    }

    async fn run(
        tool: Arc<dyn AgentTool>,
        params: serde_json::Value,
    ) -> Result<AgentToolResult, AgentError> {
        tool.execute(
            "read-1",
            params,
            CancellationToken::new(),
            Arc::new(|_p| ()),
        )
        .await
    }

    /// An 8×8 PNG, generated in code: no fixture, and `box` on it still has a
    /// non-trivial coordinate mapping (a quarter of the side is 2 pixels).
    fn png_8x8() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    #[tokio::test]
    async fn box_errors_are_one_line_and_a_valid_box_lands_after_the_image() {
        let ctx = context(&[("notes.txt", b"hello"), ("tiny.png", &png_8x8())]).await;
        let tool = create_read_tool(
            &ctx,
            Some(ReadToolOptions {
                image_draft: true,
                image_processor: Some(Arc::new(crate::image_processing::InlineImageProcessor)),
                ..Default::default()
            }),
        );
        // Not four numbers, out of range, empty, inverted: one line saying what to send.
        for bad in [
            serde_json::json!([0, 0, 1000]),
            serde_json::json!([0, 0, 1001, 1000]),
            serde_json::json!([500, 0, 500, 1000]),
            serde_json::json!([0, 1000, 1000, 0]),
            serde_json::json!([0, 0, 999.5, 1000]),
            serde_json::json!("nope"),
        ] {
            let error = run(
                tool.clone(),
                serde_json::json!({"path": "notes.txt", "box": bad}),
            )
            .await
            .expect_err("a bad box fails the call")
            .to_string();
            assert!(error.contains(ImageBox::USAGE), "{error}");
        }
        // A box on a file that is not an image says where box belongs.
        let error = run(
            tool.clone(),
            serde_json::json!({"path": "notes.txt", "box": [0, 0, 1000, 1000]}),
        )
        .await
        .expect_err("box on a text file fails the call")
        .to_string();
        assert!(error.contains("box applies to images only"), "{error}");

        // A valid box crops, and its one line follows the image so a host can
        // render it under the picture.
        let result = run(
            tool,
            serde_json::json!({"path": "tiny.png", "box": [0, 0, 250, 250]}),
        )
        .await
        .expect("a valid box reads the image");
        assert_eq!(result.content.len(), 3, "{:?}", result.content.len());
        assert!(matches!(&result.content[1], TextContentOrImage::Image(_)));
        assert_eq!(
            text_of(&result.content[2]),
            "Image 8×8, box=[0,0,250,250] = pixels [0,0]-[2,2] (2×2), \
             shown at its original 2×2 resolution."
        );
        // Without a processor a box cannot be cropped, and the image is not
        // quietly sent whole instead.
        let bare = create_read_tool(
            &ctx,
            Some(ReadToolOptions {
                image_draft: true,
                ..Default::default()
            }),
        );
        let result = run(
            bare,
            serde_json::json!({"path": "tiny.png", "box": [0, 0, 250, 250]}),
        )
        .await
        .expect("a box without a processor is reported, not fatal");
        assert!(
            text_of(&result.content[0]).contains("no image processor is configured"),
            "{:?}",
            result.content
        );
    }

    /// The text of a text content block.
    fn text_of(content: &TextContentOrImage) -> String {
        match content {
            TextContentOrImage::Text(text) => text.text.clone(),
            TextContentOrImage::Image(_) => panic!("expected a text block"),
        }
    }

    #[test]
    fn draft_off_keeps_the_upstream_read_schema() {
        // The exact schema upstream sends with `readImageDraft` off: three
        // arguments, no `box`.
        let upstream = ReadTool::schema(false);
        assert_eq!(upstream.name, "read");
        assert_eq!(
            upstream.description,
            "Read the contents of a file. Supports text files and images (jpg, png, gif, webp, bmp). \
             Images are sent as attachments. For text files, output is truncated to 2000 lines or \
             50KB (whichever is hit first). Use offset/limit for large files. When you need the full \
             file, continue with offset until complete."
        );
        assert_eq!(
            upstream.parameters.as_value(),
            &serde_json::json!({
                "$schema": "http://json-schema.org/draft-07/schema#",
                "title": "ReadInput",
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {
                        "description": "Path to the file to read (relative or absolute).",
                        "type": "string"
                    },
                    "offset": {
                        "description": "Line number to start reading from (1-indexed).",
                        "default": null,
                        "type": ["integer", "null"],
                        "format": "uint32",
                        "minimum": 0.0
                    },
                    "limit": {
                        "description": "Maximum number of lines to read.",
                        "default": null,
                        "type": ["integer", "null"],
                        "format": "uint32",
                        "minimum": 0.0
                    }
                }
            })
        );
        // On, `box` joins the parameters and one sentence joins the description.
        let draft = ReadTool::schema(true);
        assert!(draft.description.starts_with(&upstream.description));
        assert!(draft.parameters.as_value()["properties"]["box"].is_object());
    }
}
