use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::platform_extensions::developer::edit::resolve_path;
use crate::agents::tool_execution::ToolCallContext;
use anyhow::{Context, Result};
use async_trait::async_trait;
use base64::Engine;
use indoc::indoc;
use rmcp::model::{
    CallToolResult, Content, Implementation, InitializeResult, JsonObject, ListToolsResult,
    ServerCapabilities, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

pub static EXTENSION_NAME: &str = "office";

const DEFAULT_MAX_CHARS: usize = 24_000;
const MAX_TEXT_CHARS: usize = 200_000;
const PDF_COMMAND_TIMEOUT_SECS: u64 = 60;
const EXPORT_COMMAND_TIMEOUT_SECS: u64 = 180;
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;

#[derive(Debug, Deserialize, JsonSchema)]
struct PdfReadParams {
    /// PDF file path. Relative paths are resolved from the current session working directory.
    path: String,
    /// First page to read, 1-based. Defaults to 1.
    #[serde(default)]
    start_page: Option<u32>,
    /// Last page to read, inclusive. Defaults to start_page.
    #[serde(default)]
    end_page: Option<u32>,
    /// Preserve visual line layout when extracting text. Defaults to true.
    #[serde(default = "default_true")]
    layout: bool,
    /// Maximum characters to return inline. Full text is saved as a session artifact.
    #[serde(default)]
    max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PdfSearchParams {
    /// PDF file path. Relative paths are resolved from the current session working directory.
    path: String,
    /// Case-insensitive search query.
    query: String,
    /// First page to search, 1-based. Defaults to 1.
    #[serde(default)]
    start_page: Option<u32>,
    /// Last page to search, inclusive. Omit to let pdftotext read through the document.
    #[serde(default)]
    end_page: Option<u32>,
    /// Maximum number of line matches to return. Defaults to 20, max 100.
    #[serde(default)]
    max_matches: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PdfRenderParams {
    /// PDF file path. Relative paths are resolved from the current session working directory.
    path: String,
    /// Page to render, 1-based.
    page: u32,
    /// Render DPI. Defaults to 160, max 300.
    #[serde(default)]
    dpi: Option<u32>,
    /// Include the rendered PNG as image content when it is below the image size limit.
    #[serde(default = "default_true")]
    include_image: bool,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
struct DeckSlide {
    /// Slide title.
    title: String,
    /// Optional short subtitle or section label.
    #[serde(default)]
    subtitle: Option<String>,
    /// Main bullet points. Keep concise; the linter flags dense slides.
    #[serde(default)]
    bullets: Vec<String>,
    /// Optional speaker notes.
    #[serde(default)]
    notes: Option<String>,
    /// Optional local image path or URL to reference in the generated HTML.
    #[serde(default)]
    image: Option<String>,
    /// Layout hint: title, section, bullets, image_left, image_right, quote.
    #[serde(default)]
    layout: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PptCreateParams {
    /// Deck title.
    title: String,
    /// Optional subtitle.
    #[serde(default)]
    subtitle: Option<String>,
    /// Output directory for generated deck files. Relative paths use the session working directory.
    output_dir: String,
    /// Slides to generate.
    slides: Vec<DeckSlide>,
    /// Optional theme: executive, product, research, minimal. Defaults to executive.
    #[serde(default)]
    theme: Option<String>,
    /// Overwrite an existing output directory.
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PptLintParams {
    /// Directory created by ppt_create.
    deck_dir: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PptExportParams {
    /// Directory created by ppt_create.
    deck_dir: String,
    /// Export format: pdf, pptx, png, or html.
    format: String,
    /// Output file path. Defaults inside deck_dir/export.
    #[serde(default)]
    output: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeckManifest {
    title: String,
    subtitle: Option<String>,
    theme: String,
    slides: Vec<DeckSlide>,
}

pub struct OfficeClient {
    info: InitializeResult,
    context: PlatformExtensionContext,
}

impl OfficeClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Office"),
            )
            .with_instructions(
                indoc! {r#"
                Office tools provide document and presentation workflows.

                Use pdf_read/pdf_search before asking the model to reason over PDF content.
                Use pdf_render when visual inspection of a page matters.
                Use ppt_create to generate a controlled HTML/Markdown deck, then run ppt_lint before exporting.
                For production slides, prefer iterating until ppt_lint passes and screenshot/export checks are clean.
            "#}
                .to_string(),
            );

        Ok(Self { info, context })
    }

    fn schema<T: JsonSchema>() -> JsonObject {
        serde_json::to_value(schema_for!(T))
            .expect("schema serialization should succeed")
            .as_object()
            .expect("schema should serialize to an object")
            .clone()
    }

    fn tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "pdf_read".to_string(),
                "Extract bounded text from a PDF page range using the local pdftotext backend. Full output is saved as a session artifact.".to_string(),
                Self::schema::<PdfReadParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Read PDF".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "pdf_search".to_string(),
                "Search a PDF's extracted text and return line-level matches with page-range context.".to_string(),
                Self::schema::<PdfSearchParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Search PDF".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "pdf_render".to_string(),
                "Render one PDF page to a PNG session artifact using the local pdftoppm backend.".to_string(),
                Self::schema::<PdfRenderParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Render PDF Page".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "ppt_create".to_string(),
                "Create a presentation deck project with guarded 16:9 HTML, Marp-compatible Markdown, manifest JSON, and lint instructions.".to_string(),
                Self::schema::<PptCreateParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Create Deck".to_string()),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            )),
            Tool::new(
                "ppt_lint".to_string(),
                "Run static layout/content lint checks against a deck created by ppt_create.".to_string(),
                Self::schema::<PptLintParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Lint Deck".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "ppt_export".to_string(),
                "Export or prepare a deck created by ppt_create. Uses local marp/decktape when available; html export always works.".to_string(),
                Self::schema::<PptExportParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Export Deck".to_string()),
                Some(false),
                Some(true),
                Some(false),
                Some(false),
            )),
        ]
    }

    fn parse_args<T: serde::de::DeserializeOwned>(
        arguments: Option<JsonObject>,
    ) -> Result<T, String> {
        let value = arguments
            .map(Value::Object)
            .ok_or_else(|| "Missing arguments".to_string())?;
        serde_json::from_value(value).map_err(|e| format!("Failed to parse arguments: {e}"))
    }

    fn office_artifact_dir(&self, session_id: &str, kind: &str) -> Result<PathBuf> {
        let dir = self
            .context
            .session_manager
            .artifact_dir(session_id)
            .join("office")
            .join(kind);
        fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create artifact dir {}", dir.display()))?;
        Ok(dir)
    }

    async fn handle_pdf_read(
        &self,
        ctx: &ToolCallContext,
        params: PdfReadParams,
    ) -> Result<CallToolResult, String> {
        let pdf_path = resolve_existing_file(&params.path, ctx.working_dir.as_deref())?;
        let start = params.start_page.unwrap_or(1).max(1);
        let end = params.end_page.unwrap_or(start).max(start);
        let text = extract_pdf_text(&pdf_path, start, Some(end), params.layout).await?;
        let max_chars = params
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .min(MAX_TEXT_CHARS);
        let artifact_path = self
            .write_text_artifact(
                &ctx.session_id,
                "pdf_text",
                &pdf_path,
                &format!("pages-{start}-{end}"),
                &text,
            )
            .map_err(|e| e.to_string())?;
        let (preview, truncated) = truncate_chars(&text, max_chars);
        let summary = format!(
            "pdf_path: {}\npages: {}-{}\nchars: {}\ntruncated_inline: {}\nartifact_path: {}\n\n{}",
            pdf_path.display(),
            start,
            end,
            text.chars().count(),
            truncated,
            artifact_path.display(),
            preview
        );

        let mut result = CallToolResult::success(vec![Content::text(summary)]);
        result.structured_content = Some(json!({
            "pdfPath": pdf_path,
            "startPage": start,
            "endPage": end,
            "chars": text.chars().count(),
            "charCount": text.chars().count(),
            "preview": preview,
            "truncatedInline": truncated,
            "artifactPath": artifact_path,
        }));
        Ok(result)
    }

    async fn handle_pdf_search(
        &self,
        ctx: &ToolCallContext,
        params: PdfSearchParams,
    ) -> Result<CallToolResult, String> {
        let pdf_path = resolve_existing_file(&params.path, ctx.working_dir.as_deref())?;
        let query = params.query.trim();
        if query.is_empty() {
            return Err("query must not be empty".to_string());
        }
        let start = params.start_page.unwrap_or(1).max(1);
        let text = extract_pdf_text(&pdf_path, start, params.end_page, true).await?;
        let artifact_path = self
            .write_text_artifact(
                &ctx.session_id,
                "pdf_search",
                &pdf_path,
                &format!("query-{}", short_hash(query)),
                &text,
            )
            .map_err(|e| e.to_string())?;
        let matches = search_lines(&text, query, params.max_matches.unwrap_or(20).min(100));
        let mut out = format!(
            "pdf_path: {}\nquery: {}\nstart_page: {}\nend_page: {}\nmatches_returned: {}\nartifact_path: {}\n\n",
            pdf_path.display(),
            query,
            start,
            params
                .end_page
                .map(|p| p.to_string())
                .unwrap_or_else(|| "document-end".to_string()),
            matches.len(),
            artifact_path.display()
        );
        if matches.is_empty() {
            out.push_str("No matches found.");
        } else {
            for (idx, item) in matches.iter().enumerate() {
                out.push_str(&format!(
                    "--- match {} at extracted line {} ---\n{}\n\n",
                    idx + 1,
                    item.line,
                    item.snippet
                ));
            }
        }

        let mut result = CallToolResult::success(vec![Content::text(out)]);
        result.structured_content = Some(json!({
            "pdfPath": pdf_path,
            "query": query,
            "matchesReturned": matches.len(),
            "matchCount": matches.len(),
            "matches": matches,
            "artifactPath": artifact_path,
        }));
        Ok(result)
    }

    async fn handle_pdf_render(
        &self,
        ctx: &ToolCallContext,
        params: PdfRenderParams,
    ) -> Result<CallToolResult, String> {
        if params.page == 0 {
            return Err("page must be 1 or greater".to_string());
        }
        let pdf_path = resolve_existing_file(&params.path, ctx.working_dir.as_deref())?;
        let dpi = params.dpi.unwrap_or(160).clamp(72, 300);
        ensure_program(
            "pdftoppm",
            "Install poppler (macOS: brew install poppler) to render PDFs.",
        )?;
        let output_dir = self
            .office_artifact_dir(&ctx.session_id, "pdf_pages")
            .map_err(|e| e.to_string())?;
        let prefix = output_dir.join(format!(
            "{}-page-{}-{}",
            safe_stem(&pdf_path),
            params.page,
            short_hash(&pdf_path.to_string_lossy())
        ));

        let args = vec![
            OsString::from("-png"),
            OsString::from("-f"),
            OsString::from(params.page.to_string()),
            OsString::from("-l"),
            OsString::from(params.page.to_string()),
            OsString::from("-singlefile"),
            OsString::from("-r"),
            OsString::from(dpi.to_string()),
            pdf_path.as_os_str().to_os_string(),
            prefix.as_os_str().to_os_string(),
        ];
        run_command("pdftoppm", args, PDF_COMMAND_TIMEOUT_SECS).await?;
        let png_path = prefix.with_extension("png");
        if !png_path.is_file() {
            return Err(format!("pdftoppm did not create {}", png_path.display()));
        }
        let bytes = fs::read(&png_path)
            .map_err(|e| format!("failed to read rendered image {}: {e}", png_path.display()))?;
        let summary = format!(
            "pdf_path: {}\npage: {}\ndpi: {}\nimage_path: {}\nbytes: {}",
            pdf_path.display(),
            params.page,
            dpi,
            png_path.display(),
            bytes.len()
        );
        let mut content = vec![Content::text(summary)];
        if params.include_image && bytes.len() as u64 <= MAX_IMAGE_BYTES {
            content.push(Content::image(
                base64::prelude::BASE64_STANDARD.encode(&bytes),
                "image/png".to_string(),
            ));
        }
        let mut result = CallToolResult::success(content);
        result.structured_content = Some(json!({
            "pdfPath": pdf_path,
            "page": params.page,
            "dpi": dpi,
            "imagePath": png_path,
            "bytes": bytes.len(),
        }));
        Ok(result)
    }

    fn write_text_artifact(
        &self,
        session_id: &str,
        kind: &str,
        source: &Path,
        label: &str,
        text: &str,
    ) -> Result<PathBuf> {
        let dir = self.office_artifact_dir(session_id, kind)?;
        let path = dir.join(format!(
            "{}-{}-{}.txt",
            safe_stem(source),
            sanitize_segment(label),
            short_hash(&format!("{}:{label}:{text}", source.display()))
        ));
        fs::write(&path, text).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(path)
    }

    fn handle_ppt_create(
        &self,
        ctx: &ToolCallContext,
        params: PptCreateParams,
    ) -> Result<CallToolResult, String> {
        if params.title.trim().is_empty() {
            return Err("title must not be empty".to_string());
        }
        if params.slides.is_empty() {
            return Err("slides must not be empty".to_string());
        }
        let deck_dir = resolve_path(&params.output_dir, ctx.working_dir.as_deref());
        if deck_dir.exists() {
            if !params.overwrite {
                return Err(format!(
                    "output_dir already exists: {}. Set overwrite=true to replace generated deck files.",
                    deck_dir.display()
                ));
            }
            if !deck_dir.is_dir() {
                return Err(format!(
                    "output_dir exists but is not a directory: {}",
                    deck_dir.display()
                ));
            }
        }
        fs::create_dir_all(&deck_dir)
            .map_err(|e| format!("failed to create {}: {e}", deck_dir.display()))?;
        let manifest = DeckManifest {
            title: params.title,
            subtitle: params.subtitle,
            theme: params.theme.unwrap_or_else(|| "executive".to_string()),
            slides: params.slides,
        };
        let manifest_json = serde_json::to_string_pretty(&manifest)
            .map_err(|e| format!("failed to serialize deck manifest: {e}"))?;
        let html = render_deck_html(&manifest);
        let marp = render_marp_markdown(&manifest);
        let readme = render_deck_readme(&manifest);
        fs::write(deck_dir.join("deck.json"), manifest_json)
            .map_err(|e| format!("failed to write deck.json: {e}"))?;
        fs::write(deck_dir.join("slides.html"), html)
            .map_err(|e| format!("failed to write slides.html: {e}"))?;
        fs::write(deck_dir.join("slides.md"), marp)
            .map_err(|e| format!("failed to write slides.md: {e}"))?;
        fs::write(deck_dir.join("README.md"), readme)
            .map_err(|e| format!("failed to write README.md: {e}"))?;

        let lint = lint_manifest(&manifest);
        let lint_json = serde_json::to_string_pretty(&lint)
            .map_err(|e| format!("failed to serialize lint report: {e}"))?;
        fs::write(deck_dir.join("lint.json"), &lint_json)
            .map_err(|e| format!("failed to write lint.json: {e}"))?;

        let issue_count = lint["issues"].as_array().map_or(0, Vec::len);
        let summary = format!(
            "deck_dir: {}\nfiles: deck.json, slides.html, slides.md, README.md, lint.json\nlint_status: {}\nissue_count: {}\n\nOpen slides.html for visual review, then run ppt_lint and ppt_export.",
            deck_dir.display(),
            lint["status"].as_str().unwrap_or("unknown"),
            issue_count
        );
        let mut result = CallToolResult::success(vec![Content::text(summary)]);
        result.structured_content = Some(json!({
            "deckDir": deck_dir,
            "files": ["deck.json", "slides.html", "slides.md", "README.md", "lint.json"],
            "lint": lint,
        }));
        Ok(result)
    }

    fn handle_ppt_lint(
        &self,
        ctx: &ToolCallContext,
        params: PptLintParams,
    ) -> Result<CallToolResult, String> {
        let deck_dir = resolve_existing_dir(&params.deck_dir, ctx.working_dir.as_deref())?;
        let manifest = read_manifest(&deck_dir)?;
        let lint = lint_manifest(&manifest);
        let lint_json = serde_json::to_string_pretty(&lint)
            .map_err(|e| format!("failed to serialize lint report: {e}"))?;
        fs::write(deck_dir.join("lint.json"), &lint_json)
            .map_err(|e| format!("failed to write lint.json: {e}"))?;
        let issue_count = lint["issues"].as_array().map_or(0, Vec::len);
        let summary = format!(
            "deck_dir: {}\nstatus: {}\nissue_count: {}\nlint_path: {}\n\n{}",
            deck_dir.display(),
            lint["status"].as_str().unwrap_or("unknown"),
            issue_count,
            deck_dir.join("lint.json").display(),
            lint_json
        );
        let mut result = CallToolResult::success(vec![Content::text(summary)]);
        result.structured_content = Some(lint);
        Ok(result)
    }

    async fn handle_ppt_export(
        &self,
        ctx: &ToolCallContext,
        params: PptExportParams,
    ) -> Result<CallToolResult, String> {
        let deck_dir = resolve_existing_dir(&params.deck_dir, ctx.working_dir.as_deref())?;
        let format = params.format.trim().to_ascii_lowercase();
        let export_dir = deck_dir.join("export");
        fs::create_dir_all(&export_dir)
            .map_err(|e| format!("failed to create {}: {e}", export_dir.display()))?;
        let output = params
            .output
            .as_deref()
            .map(|path| resolve_path(path, ctx.working_dir.as_deref()))
            .unwrap_or_else(|| export_dir.join(default_export_name(&format)));

        match format.as_str() {
            "html" => {
                fs::copy(deck_dir.join("slides.html"), &output)
                    .map_err(|e| format!("failed to copy html export: {e}"))?;
            }
            "pptx" => {
                ensure_program(
                    "marp",
                    "Install Marp CLI (for example: npm install -g @marp-team/marp-cli) to export PPTX.",
                )?;
                run_command(
                    "marp",
                    vec![
                        OsString::from("--pptx"),
                        OsString::from("-o"),
                        output.as_os_str().to_os_string(),
                        deck_dir.join("slides.md").as_os_str().to_os_string(),
                    ],
                    EXPORT_COMMAND_TIMEOUT_SECS,
                )
                .await?;
            }
            "pdf" => {
                if which::which("decktape").is_ok() {
                    run_command(
                        "decktape",
                        vec![
                            deck_dir.join("slides.html").as_os_str().to_os_string(),
                            output.as_os_str().to_os_string(),
                        ],
                        EXPORT_COMMAND_TIMEOUT_SECS,
                    )
                    .await?;
                } else {
                    ensure_program(
                        "marp",
                        "Install DeckTape or Marp CLI to export PDF from generated decks.",
                    )?;
                    run_command(
                        "marp",
                        vec![
                            OsString::from("--pdf"),
                            OsString::from("-o"),
                            output.as_os_str().to_os_string(),
                            deck_dir.join("slides.md").as_os_str().to_os_string(),
                        ],
                        EXPORT_COMMAND_TIMEOUT_SECS,
                    )
                    .await?;
                }
            }
            "png" => {
                ensure_program(
                    "decktape",
                    "Install DeckTape to export slide screenshots as PNG images.",
                )?;
                run_command(
                    "decktape",
                    vec![
                        OsString::from("--screenshots"),
                        OsString::from("--screenshots-directory"),
                        output.as_os_str().to_os_string(),
                        deck_dir.join("slides.html").as_os_str().to_os_string(),
                        export_dir.join("slides.pdf").as_os_str().to_os_string(),
                    ],
                    EXPORT_COMMAND_TIMEOUT_SECS,
                )
                .await?;
            }
            _ => {
                return Err("format must be one of: html, pdf, pptx, png".to_string());
            }
        }

        let summary = format!(
            "deck_dir: {}\nformat: {}\noutput: {}\n\nRun ppt_lint after content changes, and visually inspect exported slides before delivery.",
            deck_dir.display(),
            format,
            output.display()
        );
        let mut result = CallToolResult::success(vec![Content::text(summary)]);
        result.structured_content = Some(json!({
            "deckDir": deck_dir,
            "format": format,
            "output": output,
        }));
        Ok(result)
    }
}

#[async_trait]
impl McpClientTrait for OfficeClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        Ok(ListToolsResult {
            tools: Self::tools(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _cancel_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let result = match name {
            "pdf_read" => match Self::parse_args::<PdfReadParams>(arguments) {
                Ok(params) => self.handle_pdf_read(ctx, params).await,
                Err(error) => Err(error),
            },
            "pdf_search" => match Self::parse_args::<PdfSearchParams>(arguments) {
                Ok(params) => self.handle_pdf_search(ctx, params).await,
                Err(error) => Err(error),
            },
            "pdf_render" => match Self::parse_args::<PdfRenderParams>(arguments) {
                Ok(params) => self.handle_pdf_render(ctx, params).await,
                Err(error) => Err(error),
            },
            "ppt_create" => match Self::parse_args::<PptCreateParams>(arguments) {
                Ok(params) => self.handle_ppt_create(ctx, params),
                Err(error) => Err(error),
            },
            "ppt_lint" => match Self::parse_args::<PptLintParams>(arguments) {
                Ok(params) => self.handle_ppt_lint(ctx, params),
                Err(error) => Err(error),
            },
            "ppt_export" => match Self::parse_args::<PptExportParams>(arguments) {
                Ok(params) => self.handle_ppt_export(ctx, params).await,
                Err(error) => Err(error),
            },
            _ => Err(format!("Unknown tool: {name}")),
        };

        match result {
            Ok(result) => Ok(result),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {error}"
            ))])),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }
}

fn default_true() -> bool {
    true
}

fn resolve_existing_file(path: &str, working_dir: Option<&Path>) -> Result<PathBuf, String> {
    let resolved = resolve_path(path, working_dir);
    if !resolved.is_file() {
        return Err(format!("file not found: {}", resolved.display()));
    }
    Ok(resolved)
}

fn resolve_existing_dir(path: &str, working_dir: Option<&Path>) -> Result<PathBuf, String> {
    let resolved = resolve_path(path, working_dir);
    if !resolved.is_dir() {
        return Err(format!("directory not found: {}", resolved.display()));
    }
    Ok(resolved)
}

fn ensure_program(name: &str, hint: &str) -> Result<(), String> {
    which::which(name)
        .map(|_| ())
        .map_err(|_| format!("required command `{name}` was not found on PATH. {hint}"))
}

async fn extract_pdf_text(
    pdf_path: &Path,
    start_page: u32,
    end_page: Option<u32>,
    layout: bool,
) -> Result<String, String> {
    ensure_program(
        "pdftotext",
        "Install poppler (macOS: brew install poppler) to read PDFs.",
    )?;
    let mut args = Vec::new();
    if layout {
        args.push(OsString::from("-layout"));
    }
    args.push(OsString::from("-enc"));
    args.push(OsString::from("UTF-8"));
    args.push(OsString::from("-f"));
    args.push(OsString::from(start_page.to_string()));
    if let Some(end) = end_page {
        args.push(OsString::from("-l"));
        args.push(OsString::from(end.max(start_page).to_string()));
    }
    args.push(pdf_path.as_os_str().to_os_string());
    args.push(OsString::from("-"));
    run_command("pdftotext", args, PDF_COMMAND_TIMEOUT_SECS).await
}

async fn run_command(
    program: &str,
    args: Vec<OsString>,
    timeout_secs: u64,
) -> Result<String, String> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| format!("command `{program}` timed out after {timeout_secs}s"))?
        .map_err(|e| format!("failed to run `{program}`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(format!(
            "`{program}` failed with status {}.\nstdout:\n{}\nstderr:\n{}",
            output.status, stdout, stderr
        ))
    }
}

fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    let mut out = String::new();
    let mut count = 0;
    for ch in text.chars() {
        if count >= max_chars {
            return (out, true);
        }
        out.push(ch);
        count += 1;
    }
    (out, false)
}

#[derive(Debug, Serialize)]
struct SearchMatch {
    line: usize,
    snippet: String,
}

fn search_lines(text: &str, query: &str, max_matches: usize) -> Vec<SearchMatch> {
    let query = query.to_lowercase();
    let mut matches = Vec::new();
    for (line_idx, line) in text.lines().enumerate() {
        if line.to_lowercase().contains(&query) {
            matches.push(SearchMatch {
                line: line_idx + 1,
                snippet: line.trim().to_string(),
            });
            if matches.len() >= max_matches {
                break;
            }
        }
    }
    matches
}

fn short_hash(input: &str) -> String {
    blake3::hash(input.as_bytes()).to_hex()[..12].to_string()
}

fn sanitize_segment(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch.to_ascii_lowercase());
        } else if ch.is_whitespace() || ch == '.' {
            out.push('-');
        }
    }
    while out.contains("--") {
        out = out.replace("--", "-");
    }
    out.trim_matches('-').chars().take(48).collect::<String>()
}

fn safe_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(sanitize_segment)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "document".to_string())
}

fn escape(text: &str) -> String {
    v_htmlescape::escape_fmt(text).to_string()
}

fn render_deck_html(manifest: &DeckManifest) -> String {
    let mut slides = String::new();
    for (idx, slide) in manifest.slides.iter().enumerate() {
        let layout = slide.layout.as_deref().unwrap_or("bullets");
        let bullets = slide
            .bullets
            .iter()
            .map(|item| format!("<li>{}</li>", escape(item)))
            .collect::<Vec<_>>()
            .join("\n");
        let subtitle = slide
            .subtitle
            .as_ref()
            .map(|s| format!("<p class=\"subtitle\">{}</p>", escape(s)))
            .unwrap_or_default();
        let image = slide
            .image
            .as_ref()
            .map(|src| format!("<figure><img src=\"{}\" alt=\"\" /></figure>", escape(src)))
            .unwrap_or_default();
        let notes = slide
            .notes
            .as_ref()
            .map(|s| format!("<aside class=\"notes\">{}</aside>", escape(s)))
            .unwrap_or_default();
        slides.push_str(&format!(
            r#"
<section class="slide layout-{layout}">
  <div class="slide-number">{}/{}</div>
  <div class="content">
    <header>
      <h1>{}</h1>
      {}
    </header>
    <main>
      <ul>{}</ul>
      {}
    </main>
  </div>
  {}
</section>
"#,
            idx + 1,
            manifest.slides.len(),
            escape(&slide.title),
            subtitle,
            bullets,
            image,
            notes
        ));
    }

    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>{}</title>
  <style>
    :root {{
      --ink: #17202a;
      --muted: #5c6670;
      --accent: #006d77;
      --accent-2: #8a4fff;
      --paper: #fbfcfd;
      --band: #e7f5f3;
      --line: rgba(23, 32, 42, 0.14);
      font-family: Inter, "SF Pro Display", "Segoe UI", "PingFang SC", "Hiragino Sans GB", "Microsoft YaHei", sans-serif;
    }}
    * {{ box-sizing: border-box; }}
    body {{ margin: 0; background: #d9dee4; color: var(--ink); }}
    .deck {{ display: grid; gap: 28px; padding: 28px; justify-items: center; }}
    .slide {{
      width: min(1280px, 100vw - 56px);
      aspect-ratio: 16 / 9;
      position: relative;
      overflow: hidden;
      background: var(--paper);
      border: 1px solid var(--line);
      box-shadow: 0 24px 70px rgba(15, 23, 42, 0.18);
    }}
    .slide::before {{
      content: "";
      position: absolute;
      inset: 0 0 auto 0;
      height: 14px;
      background: linear-gradient(90deg, var(--accent), var(--accent-2));
    }}
    .content {{
      position: absolute;
      inset: 8.5% 8% 8%;
      display: grid;
      grid-template-rows: auto 1fr;
      gap: 4.5%;
    }}
    h1 {{
      margin: 0;
      max-width: 980px;
      font-size: clamp(34px, 4.1vw, 64px);
      line-height: 1.03;
      letter-spacing: 0;
    }}
    .subtitle {{
      margin: 18px 0 0;
      max-width: 900px;
      color: var(--muted);
      font-size: clamp(20px, 1.8vw, 30px);
      line-height: 1.25;
    }}
    main {{
      display: grid;
      grid-template-columns: minmax(0, 1fr);
      align-items: start;
      gap: 32px;
      min-height: 0;
    }}
    ul {{
      margin: 0;
      padding-left: 1.15em;
      display: grid;
      gap: 18px;
      max-width: 920px;
      font-size: clamp(22px, 2.05vw, 34px);
      line-height: 1.28;
    }}
    li::marker {{ color: var(--accent); }}
    figure {{
      margin: 0;
      min-width: 0;
      border: 1px solid var(--line);
      background: white;
      overflow: hidden;
    }}
    img {{ display: block; width: 100%; height: 100%; object-fit: cover; }}
    .layout-title .content {{ place-content: center; grid-template-rows: auto; }}
    .layout-title main {{ display: none; }}
    .layout-section {{ background: linear-gradient(135deg, var(--band), var(--paper) 55%); }}
    .layout-image_left main, .layout-image_right main {{ grid-template-columns: 0.95fr 1.05fr; align-items: stretch; }}
    .layout-image_left figure {{ grid-column: 1; grid-row: 1; }}
    .layout-image_left ul {{ grid-column: 2; }}
    .layout-image_right figure {{ grid-column: 2; grid-row: 1; }}
    .layout-image_right ul {{ grid-column: 1; }}
    .layout-quote ul {{ list-style: none; padding-left: 0; font-size: clamp(30px, 3vw, 50px); line-height: 1.18; }}
    .slide-number {{ position: absolute; right: 28px; bottom: 22px; color: var(--muted); font-size: 18px; }}
    .notes {{ display: none; }}
    @media print {{
      body {{ background: white; }}
      .deck {{ padding: 0; gap: 0; }}
      .slide {{ width: 100vw; height: 100vh; box-shadow: none; border: 0; page-break-after: always; }}
    }}
  </style>
</head>
<body>
  <main class="deck">
    {}
  </main>
</body>
</html>
"#,
        escape(&manifest.title),
        slides
    )
}

fn render_marp_markdown(manifest: &DeckManifest) -> String {
    let mut out = format!(
        "---\nmarp: true\ntheme: default\npaginate: true\nsize: 16:9\ntitle: \"{}\"\n---\n\n",
        manifest.title.replace('"', "\\\"")
    );
    for slide in &manifest.slides {
        out.push_str(&format!("# {}\n\n", slide.title));
        if let Some(subtitle) = &slide.subtitle {
            out.push_str(&format!("{}\n\n", subtitle));
        }
        for bullet in &slide.bullets {
            out.push_str(&format!("- {}\n", bullet));
        }
        if let Some(image) = &slide.image {
            out.push_str(&format!("\n![bg right:40%]({})\n", image));
        }
        if let Some(notes) = &slide.notes {
            out.push_str(&format!("\n<!--\n{}\n-->\n", notes));
        }
        out.push_str("\n---\n\n");
    }
    out
}

fn render_deck_readme(manifest: &DeckManifest) -> String {
    format!(
        "# {}\n\nGenerated by goose office tools.\n\nFiles:\n\n- `slides.html`: high-fidelity local preview.\n- `slides.md`: Marp-compatible source for PDF/PPTX export.\n- `deck.json`: structured deck manifest.\n- `lint.json`: static layout report.\n\nRecommended workflow:\n\n1. Run `ppt_lint`.\n2. Open `slides.html` and inspect visually.\n3. Export with `ppt_export`.\n\nTheme: `{}`\nSlide count: {}\n",
        manifest.title,
        manifest.theme,
        manifest.slides.len()
    )
}

fn read_manifest(deck_dir: &Path) -> Result<DeckManifest, String> {
    let path = deck_dir.join("deck.json");
    let text =
        fs::read_to_string(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("failed to parse {}: {e}", path.display()))
}

fn lint_manifest(manifest: &DeckManifest) -> Value {
    let mut issues = Vec::new();
    for (idx, slide) in manifest.slides.iter().enumerate() {
        let slide_no = idx + 1;
        let layout = slide.layout.as_deref().unwrap_or("bullets");
        let title_chars = slide.title.chars().count();
        let bullet_count = slide.bullets.len();
        let bullet_chars: usize = slide.bullets.iter().map(|b| b.chars().count()).sum();
        let total_chars =
            title_chars + slide.subtitle.as_ref().map_or(0, |s| s.chars().count()) + bullet_chars;

        if title_chars > 42 {
            issues.push(json!({
                "slide": slide_no,
                "severity": "warning",
                "code": "title_too_long",
                "message": "Title is likely to wrap too much; keep it under 42 CJK/Latin characters."
            }));
        }
        if bullet_count > 5 {
            issues.push(json!({
                "slide": slide_no,
                "severity": "error",
                "code": "too_many_bullets",
                "message": "More than 5 bullets often causes crowding and visual hierarchy failure."
            }));
        }
        if slide.bullets.iter().any(|b| b.chars().count() > 54) {
            issues.push(json!({
                "slide": slide_no,
                "severity": "warning",
                "code": "bullet_too_long",
                "message": "At least one bullet is longer than 54 characters and may overflow."
            }));
        }
        if total_chars > 280 {
            issues.push(json!({
                "slide": slide_no,
                "severity": "error",
                "code": "slide_too_dense",
                "message": "Slide has too much text for stable 16:9 rendering."
            }));
        }
        if matches!(layout, "image_left" | "image_right") && slide.image.is_none() {
            issues.push(json!({
                "slide": slide_no,
                "severity": "warning",
                "code": "image_layout_without_image",
                "message": "Image layout selected but no image source was provided."
            }));
        }
        if !matches!(
            layout,
            "title" | "section" | "bullets" | "image_left" | "image_right" | "quote"
        ) {
            issues.push(json!({
                "slide": slide_no,
                "severity": "warning",
                "code": "unknown_layout",
                "message": "Unknown layout hint; generated HTML falls back to bullet layout styling."
            }));
        }
    }

    let has_error = issues
        .iter()
        .any(|issue| issue["severity"].as_str() == Some("error"));
    json!({
        "status": if has_error { "fail" } else { "pass" },
        "slideCount": manifest.slides.len(),
        "issues": issues,
        "guardrails": {
            "aspectRatio": "16:9",
            "safeMargin": "8%",
            "maxBullets": 5,
            "maxSlideChars": 280,
            "maxTitleChars": 42,
            "requiresVisualReview": true
        }
    })
}

fn default_export_name(format: &str) -> String {
    match format {
        "html" => "slides.html".to_string(),
        "pdf" => "slides.pdf".to_string(),
        "pptx" => "slides.pptx".to_string(),
        "png" => "screenshots".to_string(),
        other => format!("slides.{other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_manifest() -> DeckManifest {
        DeckManifest {
            title: "测试演示".to_string(),
            subtitle: Some("副标题".to_string()),
            theme: "executive".to_string(),
            slides: vec![
                DeckSlide {
                    title: "标题页".to_string(),
                    subtitle: Some("清晰的信息层级".to_string()),
                    bullets: vec![],
                    notes: None,
                    image: None,
                    layout: Some("title".to_string()),
                },
                DeckSlide {
                    title: "关键结论".to_string(),
                    subtitle: None,
                    bullets: vec!["第一点".to_string(), "第二点".to_string()],
                    notes: Some("speaker notes".to_string()),
                    image: None,
                    layout: Some("bullets".to_string()),
                },
            ],
        }
    }

    #[test]
    fn deck_html_contains_guarded_layout() {
        let html = render_deck_html(&sample_manifest());
        assert!(html.contains("aspect-ratio: 16 / 9"));
        assert!(html.contains("layout-title"));
        assert!(html.contains("测试演示"));
    }

    #[test]
    fn lint_fails_dense_slide() {
        let mut manifest = sample_manifest();
        manifest.slides.push(DeckSlide {
            title: "过密页面".to_string(),
            subtitle: None,
            bullets: vec![
                "很长的项目符号内容会导致视觉拥挤，需要被静态检查提前发现".to_string(),
                "第二条".to_string(),
                "第三条".to_string(),
                "第四条".to_string(),
                "第五条".to_string(),
                "第六条".to_string(),
            ],
            notes: None,
            image: None,
            layout: Some("bullets".to_string()),
        });
        let lint = lint_manifest(&manifest);
        assert_eq!(lint["status"], "fail");
        assert!(lint["issues"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| { issue["code"].as_str() == Some("too_many_bullets") }));
    }
}
