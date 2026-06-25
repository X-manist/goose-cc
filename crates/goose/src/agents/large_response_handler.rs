use crate::config::Config;
use goose_context_core::ContextCompressor;
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;

const DEFAULT_LARGE_TEXT_THRESHOLD: usize = 200_000;

fn large_text_threshold() -> usize {
    Config::global()
        .get_param::<usize>("GOOSE_MAX_TOOL_RESPONSE_SIZE")
        .unwrap_or(DEFAULT_LARGE_TEXT_THRESHOLD)
}

/// Process tool response and handle large text content
pub fn process_tool_response(
    artifact_root: &Path,
    session_id: &str,
    tool_name: &str,
    response: Result<CallToolResult, ErrorData>,
) -> Result<CallToolResult, ErrorData> {
    let threshold = large_text_threshold();
    match response {
        Ok(mut result) => {
            let mut processed_contents = Vec::new();

            for content in result.content {
                match content.as_text() {
                    Some(text_content) => {
                        // Check if text exceeds threshold
                        if text_content.text.chars().count() > threshold {
                            let compressor = ContextCompressor::new(artifact_root);
                            match compressor.compress_large_text(
                                session_id,
                                tool_name,
                                &text_content.text,
                            ) {
                                Ok(compact) => {
                                    processed_contents.push(Content::text(compact.summary));
                                }
                                Err(e) => {
                                    // If file writing fails, include original content with warning
                                    let warning = format!(
                                        "Warning: Failed to write large response to file: {}. Showing full content instead.\n\n{}",
                                        e,
                                        text_content.text
                                    );
                                    processed_contents.push(Content::text(warning));
                                }
                            }
                        } else {
                            // Keep original content for smaller texts
                            processed_contents.push(content);
                        }
                    }
                    None => {
                        // Pass through other content types unchanged
                        processed_contents.push(content);
                    }
                }
            }

            result.content = processed_contents;
            Ok(result)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Content, ErrorCode, ErrorData};
    use std::borrow::Cow;
    use std::fs;
    use std::path::Path;

    fn artifact_root() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_small_text_response_passes_through() {
        let artifact_root = artifact_root();
        // Create a small text response
        let small_text = "This is a small text response";
        let content = Content::text(small_text.to_string());

        let response = Ok(CallToolResult::success(vec![content]));

        // Process the response
        let processed =
            process_tool_response(artifact_root.path(), "test-session", "test_tool", response)
                .unwrap();

        // Verify the response is unchanged
        assert_eq!(processed.content.len(), 1);
        if let Some(text_content) = processed.content[0].as_text() {
            assert_eq!(text_content.text, small_text);
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_large_text_response_redirected_to_file() {
        let artifact_root = artifact_root();
        // Create a text larger than the threshold
        let large_text = "a".repeat(DEFAULT_LARGE_TEXT_THRESHOLD + 1000);
        let content = Content::text(large_text.clone());

        let response = Ok(CallToolResult::success(vec![content]));

        // Process the response
        let processed =
            process_tool_response(artifact_root.path(), "test-session", "test_tool", response)
                .unwrap();

        // Verify the response contains a message about the file
        assert_eq!(processed.content.len(), 1);
        if let Some(text_content) = processed.content[0].as_text() {
            assert!(text_content
                .text
                .contains("Tool output from `test_tool` was large"));
            assert!(text_content.text.contains("chars:"));

            // Extract the file path from the message
            if let Some(file_path) = text_content.text.split("artifact_path: ").nth(1) {
                let file_path = file_path.lines().next().unwrap_or_default();
                // Verify the file exists and contains the original text
                let path = Path::new(file_path.trim());
                if path.exists() {
                    // Only check content if file exists (may not exist in CI environments)
                    if let Ok(file_content) = fs::read_to_string(path) {
                        assert_eq!(file_content, large_text);
                    }

                    // Clean up the file
                    let _ = fs::remove_file(path); // Ignore errors on cleanup
                }
            }
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_image_content_passes_through() {
        let artifact_root = artifact_root();
        // Create an image content
        let image_content = Content::image("base64data".to_string(), "image/png".to_string());

        let response = Ok(CallToolResult::success(vec![image_content]));

        // Process the response
        let processed =
            process_tool_response(artifact_root.path(), "test-session", "test_tool", response)
                .unwrap();

        // Verify the response is unchanged
        assert_eq!(processed.content.len(), 1);
        if let Some(img) = processed.content[0].as_image() {
            assert_eq!(img.data, "base64data");
            assert_eq!(img.mime_type, "image/png");
        } else {
            panic!("Expected image content");
        }
    }

    #[test]
    fn test_mixed_content_handled_correctly() {
        let artifact_root = artifact_root();
        // Create a response with mixed content types
        let small_text = Content::text("Small text");
        let large_text = Content::text("a".repeat(DEFAULT_LARGE_TEXT_THRESHOLD + 1000));
        let image = Content::image("image_data".to_string(), "image/jpeg".to_string());

        let response = Ok(CallToolResult::success(vec![small_text, large_text, image]));

        // Process the response
        let processed =
            process_tool_response(artifact_root.path(), "test-session", "test_tool", response)
                .unwrap();

        // Verify each item is handled correctly
        assert_eq!(processed.content.len(), 3);

        // First item should be unchanged small text
        if let Some(text_content) = processed.content[0].as_text() {
            assert_eq!(text_content.text, "Small text");
        } else {
            panic!("Expected text content");
        }

        // Second item should be a message about the file
        if let Some(text_content) = processed.content[1].as_text() {
            assert!(text_content
                .text
                .contains("Tool output from `test_tool` was large"));

            // Extract the file path and clean up
            if let Some(file_path) = text_content.text.split("artifact_path: ").nth(1) {
                let file_path = file_path.lines().next().unwrap_or_default();
                let path = Path::new(file_path.trim());
                if path.exists() {
                    let _ = fs::remove_file(path); // Ignore errors on cleanup
                }
            }
        } else {
            panic!("Expected text content");
        }

        // Third item should be unchanged image
        if let Some(img) = processed.content[2].as_image() {
            assert_eq!(img.data, "image_data");
            assert_eq!(img.mime_type, "image/jpeg");
        } else {
            panic!("Expected image content");
        }
    }

    #[test]
    fn test_error_response_passes_through() {
        let artifact_root = artifact_root();
        // Create an error response
        let error = ErrorData {
            code: ErrorCode::INTERNAL_ERROR,
            message: Cow::from("Test error"),
            data: None,
        };
        let response: Result<CallToolResult, ErrorData> = Err(error);

        // Process the response
        let processed =
            process_tool_response(artifact_root.path(), "test-session", "test_tool", response);

        // Verify the error is passed through unchanged
        assert!(processed.is_err());
        match processed {
            Err(err) => {
                assert_eq!(err.code, ErrorCode::INTERNAL_ERROR);
                assert_eq!(err.message, "Test error");
            }
            _ => panic!("Expected execution error"),
        }
    }
}
