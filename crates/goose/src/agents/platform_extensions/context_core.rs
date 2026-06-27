use crate::agents::extension::PlatformExtensionContext;
use crate::agents::mcp_client::{Error, McpClientTrait};
use crate::agents::tool_execution::ToolCallContext;
use anyhow::{Context, Result};
use async_trait::async_trait;
use indoc::indoc;
use rmcp::model::{
    CallToolResult, Content, Implementation, InitializeResult, JsonObject, ListToolsResult,
    ServerCapabilities, Tool, ToolAnnotations,
};
use schemars::{schema_for, JsonSchema};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tokio_util::sync::CancellationToken;

pub static EXTENSION_NAME: &str = "context_core";

const DEFAULT_READ_MAX_CHARS: usize = 16_000;
const MAX_READ_CHARS: usize = 100_000;
const DEFAULT_SEARCH_CONTEXT_CHARS: usize = 240;
const MAX_SEARCH_CONTEXT_CHARS: usize = 2_000;
const DEFAULT_SEARCH_MATCHES: usize = 20;
const MAX_SEARCH_MATCHES: usize = 100;

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ContextCoreReadParams {
    /// Path from a compacted tool output's artifact_path line.
    artifact_path: String,
    /// Character offset to begin reading from. Defaults to 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    offset: Option<usize>,
    /// Maximum characters to return. Defaults to 16000, max 100000.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_chars: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
struct ContextCoreSearchParams {
    /// Path from a compacted tool output's artifact_path line.
    artifact_path: String,
    /// Case-insensitive text query to find in the artifact.
    query: String,
    /// Max matches to return. Defaults to 20, max 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    max_matches: Option<usize>,
    /// Characters of context to include before and after each match. Defaults to 240, max 2000.
    #[serde(skip_serializing_if = "Option::is_none")]
    context_chars: Option<usize>,
}

pub struct ContextCoreClient {
    info: InitializeResult,
    context: PlatformExtensionContext,
}

impl ContextCoreClient {
    pub fn new(context: PlatformExtensionContext) -> Result<Self> {
        let info = InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(
                Implementation::new(EXTENSION_NAME.to_string(), "1.0.0".to_string())
                    .with_title("Context Core"),
            )
            .with_instructions(
                indoc! {r#"
                Context Core provides read-only access to raw tool outputs that were compacted into local session artifacts.

                Use context_core_read when a compact tool summary says exact raw output is needed.
                Use context_core_search to find specific lines or snippets inside a large artifact before reading a slice.
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

    fn get_tools() -> Vec<Tool> {
        vec![
            Tool::new(
                "context_core_read".to_string(),
                "Read a bounded slice from a compacted tool-output artifact for the current session."
                    .to_string(),
                Self::schema::<ContextCoreReadParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Read Context Artifact".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
            Tool::new(
                "context_core_search".to_string(),
                "Search a compacted tool-output artifact for a query and return bounded snippets."
                    .to_string(),
                Self::schema::<ContextCoreSearchParams>(),
            )
            .annotate(ToolAnnotations::from_raw(
                Some("Search Context Artifact".to_string()),
                Some(true),
                Some(false),
                Some(true),
                Some(false),
            )),
        ]
    }

    fn parse_args<T: serde::de::DeserializeOwned>(
        arguments: Option<JsonObject>,
    ) -> Result<T, String> {
        let value = arguments
            .map(serde_json::Value::Object)
            .ok_or_else(|| "Missing arguments".to_string())?;
        serde_json::from_value(value).map_err(|e| format!("Failed to parse arguments: {e}"))
    }

    fn resolve_artifact_path(&self, session_id: &str, artifact_path: &str) -> Result<PathBuf> {
        let allowed_root = self
            .context
            .session_manager
            .artifact_dir(session_id)
            .join("tool_outputs")
            .canonicalize()
            .with_context(|| {
                format!("tool output artifact directory does not exist for session {session_id}")
            })?;
        let path = Path::new(artifact_path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            allowed_root.join(path)
        };
        let canonical = path
            .canonicalize()
            .with_context(|| format!("artifact path not found: {}", path.display()))?;
        if !canonical.starts_with(&allowed_root) {
            anyhow::bail!("artifact_path is outside the current session tool output artifacts");
        }
        if !canonical.is_file() {
            anyhow::bail!("artifact_path is not a file");
        }
        Ok(canonical)
    }

    fn handle_read(&self, session_id: &str, params: ContextCoreReadParams) -> Result<String> {
        let path = self.resolve_artifact_path(session_id, &params.artifact_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read artifact {}", path.display()))?;
        let offset = params.offset.unwrap_or(0).min(text.chars().count());
        let max_chars = params
            .max_chars
            .unwrap_or(DEFAULT_READ_MAX_CHARS)
            .min(MAX_READ_CHARS);
        let chunk: String = text.chars().skip(offset).take(max_chars).collect();
        let returned_chars = chunk.chars().count();
        let total_chars = text.chars().count();
        let next_offset = offset + returned_chars;

        Ok(format!(
            "artifact_path: {}\nchars: {} total, returned {} from offset {}\nnext_offset: {}\n\n{}",
            path.display(),
            total_chars,
            returned_chars,
            offset,
            next_offset,
            chunk
        ))
    }

    fn handle_search(&self, session_id: &str, params: ContextCoreSearchParams) -> Result<String> {
        let path = self.resolve_artifact_path(session_id, &params.artifact_path)?;
        let query = params.query.trim();
        if query.is_empty() {
            anyhow::bail!("query must not be empty");
        }

        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read artifact {}", path.display()))?;
        let lower_query = query.to_lowercase();
        let lower_query_chars = lower_query.chars().count();
        let max_matches = params
            .max_matches
            .unwrap_or(DEFAULT_SEARCH_MATCHES)
            .min(MAX_SEARCH_MATCHES);
        let context_chars = params
            .context_chars
            .unwrap_or(DEFAULT_SEARCH_CONTEXT_CHARS)
            .min(MAX_SEARCH_CONTEXT_CHARS);

        let mut matches = Vec::new();
        let mut char_offset = 0;
        for (line_index, line) in text.lines().enumerate() {
            let (lower_line, lower_to_original_char) = lowercase_with_original_char_map(line);
            let mut search_start_byte = 0;
            while let Some(relative_byte) = lower_line[search_start_byte..].find(&lower_query) {
                let match_byte = search_start_byte + relative_byte;
                let lower_match_char = lower_line[..match_byte].chars().count();
                let Some(&match_char) = lower_to_original_char.get(lower_match_char) else {
                    break;
                };
                let lower_end_char = lower_match_char.saturating_add(lower_query_chars);
                let match_end_char = lower_to_original_char
                    .get(lower_end_char.saturating_sub(1))
                    .map(|idx| idx + 1)
                    .unwrap_or_else(|| line.chars().count());
                let start = match_char.saturating_sub(context_chars);
                let end = match_end_char
                    .saturating_add(context_chars)
                    .min(line.chars().count());
                let snippet: String = line.chars().skip(start).take(end - start).collect();
                matches.push((line_index + 1, char_offset + match_char, snippet));
                if matches.len() >= max_matches {
                    break;
                }
                search_start_byte = match_byte + lower_query.len();
            }
            if matches.len() >= max_matches {
                break;
            }
            char_offset += line.chars().count() + 1;
        }

        let mut out = format!(
            "artifact_path: {}\nquery: {}\nmatches_returned: {}\n\n",
            path.display(),
            query,
            matches.len()
        );

        if matches.is_empty() {
            out.push_str("No matches found.");
        } else {
            for (idx, (line, offset, snippet)) in matches.iter().enumerate() {
                out.push_str(&format!(
                    "--- match {} at line {}, char {} ---\n{}\n\n",
                    idx + 1,
                    line,
                    offset,
                    snippet
                ));
            }
        }
        Ok(out)
    }
}

fn lowercase_with_original_char_map(line: &str) -> (String, Vec<usize>) {
    let mut lower = String::new();
    let mut lower_to_original_char = Vec::new();
    for (original_char_index, ch) in line.chars().enumerate() {
        for lowered in ch.to_lowercase() {
            lower.push(lowered);
            lower_to_original_char.push(original_char_index);
        }
    }
    (lower, lower_to_original_char)
}

#[async_trait]
impl McpClientTrait for ContextCoreClient {
    async fn list_tools(
        &self,
        _session_id: &str,
        _next_cursor: Option<String>,
        _cancellation_token: CancellationToken,
    ) -> Result<ListToolsResult, Error> {
        Ok(ListToolsResult {
            tools: Self::get_tools(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        ctx: &ToolCallContext,
        name: &str,
        arguments: Option<JsonObject>,
        _cancellation_token: CancellationToken,
    ) -> Result<CallToolResult, Error> {
        let content = match name {
            "context_core_read" => {
                Self::parse_args::<ContextCoreReadParams>(arguments).and_then(|params| {
                    self.handle_read(&ctx.session_id, params)
                        .map_err(|e| e.to_string())
                })
            }
            "context_core_search" => Self::parse_args::<ContextCoreSearchParams>(arguments)
                .and_then(|params| {
                    self.handle_search(&ctx.session_id, params)
                        .map_err(|e| e.to_string())
                }),
            _ => Err(format!("Unknown tool: {name}")),
        };

        match content {
            Ok(content) => Ok(CallToolResult::success(vec![Content::text(content)])),
            Err(error) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Error: {error}"
            ))])),
        }
    }

    fn get_info(&self) -> Option<&InitializeResult> {
        Some(&self.info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionManager;
    use std::sync::Arc;

    fn test_client(session_id: &str, content: &str) -> (ContextCoreClient, PathBuf) {
        let data_dir = tempfile::tempdir().unwrap().keep();
        let session_manager = Arc::new(SessionManager::new(data_dir));
        let artifact_dir = session_manager
            .artifact_dir(session_id)
            .join("tool_outputs");
        fs::create_dir_all(&artifact_dir).unwrap();
        let artifact_path = artifact_dir.join("artifact.txt");
        fs::write(&artifact_path, content).unwrap();

        let client = ContextCoreClient::new(PlatformExtensionContext {
            extension_manager: None,
            session_manager,
            session: None,
            use_login_shell_path: false,
        })
        .unwrap();

        (client, artifact_path)
    }

    #[tokio::test]
    async fn read_returns_bounded_slice() {
        let (client, artifact_path) = test_client("session_1", "abcdef");
        let output = client
            .handle_read(
                "session_1",
                ContextCoreReadParams {
                    artifact_path: artifact_path.to_string_lossy().to_string(),
                    offset: Some(2),
                    max_chars: Some(3),
                },
            )
            .unwrap();

        assert!(output.contains("returned 3 from offset 2"));
        assert!(output.ends_with("cde"));
    }

    #[tokio::test]
    async fn search_returns_line_snippets() {
        let (client, artifact_path) = test_client("session_1", "alpha\nbeta needle\ngamma");
        let output = client
            .handle_search(
                "session_1",
                ContextCoreSearchParams {
                    artifact_path: artifact_path.to_string_lossy().to_string(),
                    query: "NEEDLE".to_string(),
                    max_matches: Some(5),
                    context_chars: Some(20),
                },
            )
            .unwrap();

        assert!(output.contains("matches_returned: 1"));
        assert!(output.contains("line 2"));
        assert!(output.contains("beta needle"));
    }

    #[tokio::test]
    async fn search_returns_multiple_matches_on_one_line_and_handles_unicode() {
        let (client, artifact_path) = test_client("session_1", "İstanbul needle NEEDLE needle");
        let output = client
            .handle_search(
                "session_1",
                ContextCoreSearchParams {
                    artifact_path: artifact_path.to_string_lossy().to_string(),
                    query: "needle".to_string(),
                    max_matches: Some(5),
                    context_chars: Some(8),
                },
            )
            .unwrap();

        assert!(output.contains("matches_returned: 3"));
        assert!(output.contains("--- match 1 at line 1"));
        assert!(output.contains("--- match 3 at line 1"));
    }

    #[tokio::test]
    async fn rejects_paths_outside_session_artifacts() {
        let (client, _) = test_client("session_1", "inside");
        let outside = tempfile::NamedTempFile::new().unwrap();
        let err = client
            .handle_read(
                "session_1",
                ContextCoreReadParams {
                    artifact_path: outside.path().to_string_lossy().to_string(),
                    offset: None,
                    max_chars: None,
                },
            )
            .unwrap_err();

        assert!(err.to_string().contains("outside the current session"));
    }
}
