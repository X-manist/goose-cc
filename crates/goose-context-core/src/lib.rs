use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_HEAD_CHARS: usize = 6_000;
const DEFAULT_TAIL_CHARS: usize = 2_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolArtifact {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: usize,
    pub chars: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactToolText {
    pub artifact: ToolArtifact,
    pub summary: String,
}

#[derive(Debug, Clone)]
pub struct ContextCompressor {
    artifact_root: PathBuf,
}

impl ContextCompressor {
    pub fn new(artifact_root: impl Into<PathBuf>) -> Self {
        Self {
            artifact_root: artifact_root.into(),
        }
    }

    pub fn compress_large_text(
        &self,
        session_id: &str,
        tool_name: &str,
        text: &str,
    ) -> Result<CompactToolText> {
        let artifact = self.write_artifact(session_id, tool_name, text)?;
        let summary = build_summary(tool_name, text, &artifact);
        Ok(CompactToolText { artifact, summary })
    }

    fn write_artifact(
        &self,
        session_id: &str,
        tool_name: &str,
        text: &str,
    ) -> Result<ToolArtifact> {
        let session_dir = self.artifact_root.join(stable_segment(session_id));
        fs::create_dir_all(&session_dir).with_context(|| {
            format!(
                "failed to create context artifact directory {}",
                session_dir.display()
            )
        })?;

        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let sha256 = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_micros();
        let file_name = format!(
            "{}_{}_{}.txt",
            timestamp,
            stable_segment(tool_name),
            &sha256[..12]
        );
        let path = session_dir.join(file_name);
        fs::write(&path, text)
            .with_context(|| format!("failed to write context artifact {}", path.display()))?;

        Ok(ToolArtifact {
            path,
            sha256,
            bytes: text.len(),
            chars: text.chars().count(),
        })
    }
}

pub fn default_artifact_root() -> PathBuf {
    std::env::temp_dir().join("goose_context_artifacts")
}

pub fn build_summary(tool_name: &str, text: &str, artifact: &ToolArtifact) -> String {
    let head = take_chars(text, DEFAULT_HEAD_CHARS);
    let omitted = artifact.chars.saturating_sub(head.chars().count());
    let tail = if omitted > DEFAULT_TAIL_CHARS {
        take_last_chars(text, DEFAULT_TAIL_CHARS)
    } else {
        String::new()
    };

    let mut out = format!(
        "Tool output from `{}` was large and has been stored as a local context artifact.\n\
artifact_path: {}\n\
sha256: {}\n\
bytes: {}\n\
chars: {}\n\n\
The model-visible output below is compacted. Use context_core_read or context_core_search with artifact_path if exact raw output is needed.\n\n\
--- output head ---\n{}",
        tool_name,
        artifact.path.display(),
        artifact.sha256,
        artifact.bytes,
        artifact.chars,
        head
    );

    if !tail.is_empty() {
        out.push_str(&format!(
            "\n--- omitted {} chars from middle ---\n--- output tail ---\n{}",
            artifact
                .chars
                .saturating_sub(head.chars().count() + tail.chars().count()),
            tail
        ));
    } else if omitted > 0 {
        out.push_str(&format!("\n--- omitted {} chars ---", omitted));
    }

    out
}

pub fn stable_segment(value: &str) -> String {
    let mut sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '-'
            }
        })
        .collect();

    if sanitized.is_empty() {
        sanitized.push_str("unknown");
    }

    let mut hasher = Sha256::new();
    hasher.update(value.as_bytes());
    let digest = hasher.finalize();
    let hash = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let prefix: String = sanitized.chars().take(48).collect();
    let prefix = prefix.trim_matches('-');
    if prefix.is_empty() || prefix == "." || prefix == ".." {
        format!("artifact-{hash}")
    } else {
        format!("{prefix}-{hash}")
    }
}

fn take_chars(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn take_last_chars(text: &str, limit: usize) -> String {
    let mut chars: Vec<char> = text.chars().rev().take(limit).collect();
    chars.reverse();
    chars.into_iter().collect()
}

pub fn artifact_exists(artifact: &ToolArtifact) -> bool {
    Path::new(&artifact.path).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_artifact_and_returns_summary() {
        let dir = tempfile::tempdir().unwrap();
        let compressor = ContextCompressor::new(dir.path());
        let text = format!("{}{}", "a".repeat(8_000), "z".repeat(3_000));

        let compact = compressor
            .compress_large_text("session/1", "developer__shell", &text)
            .unwrap();

        assert!(artifact_exists(&compact.artifact));
        assert_eq!(fs::read_to_string(&compact.artifact.path).unwrap(), text);
        assert!(compact.summary.contains("artifact_path:"));
        assert!(compact.summary.contains("--- output head ---"));
        assert!(compact.summary.contains("--- output tail ---"));
        assert!(compact.summary.contains("developer__shell"));
    }

    #[test]
    fn path_segments_are_stable_and_not_colliding() {
        let dir = tempfile::tempdir().unwrap();
        let compressor = ContextCompressor::new(dir.path());

        let compact = compressor
            .compress_large_text("../bad", "tool/name", "hello")
            .unwrap();

        let path = compact.artifact.path.to_string_lossy();
        assert!(!path.contains("../bad"));
        assert!(path.contains("tool-name"));
        assert_ne!(stable_segment("a:b"), stable_segment("a_b"));
        assert_ne!(stable_segment("."), ".");
        assert_ne!(stable_segment(".."), "..");
    }
}
