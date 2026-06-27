use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreePlan {
    pub repo_root: PathBuf,
    pub worktree_dir: PathBuf,
    pub branch: String,
    pub base_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
}

fn git_command(repo_root: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_root);
    cmd
}

fn run_git(repo_root: &Path, args: &[&str]) -> Result<String> {
    let out = git_command(repo_root)
        .args(args)
        .output()
        .with_context(|| format!("failed to invoke git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8(out.stdout).map_err(|e| anyhow!("git output was not UTF-8: {e}"))
}

pub fn find_repo_root(start: &Path) -> Result<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("failed to invoke git rev-parse --show-toplevel")?;
    if !out.status.success() {
        bail!(
            "not inside a git repository: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(PathBuf::from(String::from_utf8(out.stdout)?.trim()))
}

pub fn current_branch(start: &Path) -> Result<Option<String>> {
    let repo_root = find_repo_root(start)?;
    let out = run_git(&repo_root, &["branch", "--show-current"])?;
    let branch = out.trim();
    if branch.is_empty() {
        Ok(None)
    } else {
        Ok(Some(branch.to_string()))
    }
}

pub fn is_goose_worktree(start: &Path) -> Result<bool> {
    Ok(current_branch(start)?
        .as_deref()
        .is_some_and(|branch| branch.starts_with("goose/")))
}

pub fn is_linked_worktree(start: &Path) -> Result<bool> {
    let repo_root = find_repo_root(start)?;
    let current_root = repo_root.canonicalize().unwrap_or(repo_root.clone());
    let infos = list_worktrees(start)?;
    let Some(primary) = infos.first() else {
        return Ok(false);
    };
    let primary_root = primary.path.canonicalize().unwrap_or(primary.path.clone());
    Ok(primary_root != current_root)
}

pub fn sanitize_segment(value: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_was_dash = false;
        } else if ch == '-' {
            if !last_was_dash {
                out.push(ch);
                last_was_dash = true;
            }
        } else if matches!(ch, '_' | '.') {
            out.push(ch);
            last_was_dash = false;
        } else if ch.is_whitespace() || matches!(ch, '/' | '\\' | ':') {
            if !last_was_dash {
                out.push('-');
                last_was_dash = true;
            }
        }
    }
    let out = out
        .trim_matches(|c| matches!(c, '-' | '_' | '.'))
        .to_string();
    if out.is_empty() {
        "task".to_string()
    } else {
        out.chars().take(48).collect()
    }
}

fn branch_exists(repo_root: &Path, branch: &str) -> Result<bool> {
    let status = git_command(repo_root)
        .args(["show-ref", "--verify", "--quiet"])
        .arg(format!("refs/heads/{branch}"))
        .status()
        .with_context(|| format!("failed to invoke git show-ref for {branch}"))?;
    Ok(status.success())
}

fn validate_branch(repo_root: &Path, branch: &str) -> Result<()> {
    let out = git_command(repo_root)
        .args(["check-ref-format", "--branch", branch])
        .output()
        .with_context(|| format!("failed to invoke git check-ref-format for {branch}"))?;
    if !out.status.success() {
        bail!(
            "generated worktree branch is invalid: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn plan_worktree(
    start: &Path,
    session_id: &str,
    label: Option<&str>,
    base_ref: Option<&str>,
) -> Result<WorktreePlan> {
    let repo_root = find_repo_root(start)?;
    let repo_name = repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(sanitize_segment)
        .unwrap_or_else(|| "repo".to_string());
    let session_part = sanitize_segment(session_id);
    let label_part = label
        .map(sanitize_segment)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "task".to_string());
    let parent = repo_root
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| repo_root.clone());
    let base_name = format!("{session_part}-{label_part}");
    let mut branch = format!("goose/{base_name}");
    let mut worktree_dir = parent.join(format!("{repo_name}-{base_name}"));
    let mut suffix = 2usize;
    while worktree_dir.exists() || branch_exists(&repo_root, &branch)? {
        branch = format!("goose/{base_name}-{suffix}");
        worktree_dir = parent.join(format!("{repo_name}-{base_name}-{suffix}"));
        suffix += 1;
    }
    validate_branch(&repo_root, &branch)?;
    Ok(WorktreePlan {
        repo_root,
        worktree_dir,
        branch,
        base_ref: base_ref.unwrap_or("HEAD").to_string(),
    })
}

pub fn create_worktree(plan: &WorktreePlan) -> Result<()> {
    let out = git_command(&plan.repo_root)
        .args(["worktree", "add", "-b"])
        .arg(&plan.branch)
        .arg(&plan.worktree_dir)
        .arg(&plan.base_ref)
        .output()
        .with_context(|| {
            format!(
                "failed to invoke git worktree add for {}",
                plan.worktree_dir.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn remove_worktree(repo_root: &Path, worktree_dir: &Path) -> Result<()> {
    let out = git_command(repo_root)
        .args(["worktree", "remove", "--force"])
        .arg(worktree_dir)
        .output()
        .with_context(|| {
            format!(
                "failed to invoke git worktree remove for {}",
                worktree_dir.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    Ok(())
}

pub fn list_worktrees(start: &Path) -> Result<Vec<WorktreeInfo>> {
    let repo_root = find_repo_root(start)?;
    let out = run_git(&repo_root, &["worktree", "list", "--porcelain"])?;
    let mut infos = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            if let Some(info) = current.take() {
                infos.push(info);
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(path),
                branch: None,
                head: None,
            });
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(info) = current.as_mut() {
                info.branch = Some(branch.trim_start_matches("refs/heads/").to_string());
            }
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(info) = current.as_mut() {
                info.head = Some(head.to_string());
            }
        }
    }
    if let Some(info) = current {
        infos.push(info);
    }
    Ok(infos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_segment_normalizes_to_safe_ascii() {
        assert_eq!(
            sanitize_segment("Feature: Hello World"),
            "feature-hello-world"
        );
        assert_eq!(sanitize_segment("../../"), "task");
        assert_eq!(sanitize_segment("review/fix"), "review-fix");
        assert_eq!(sanitize_segment("a..b"), "a..b");
    }

    #[test]
    fn current_branch_detects_goose_worktree_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["checkout", "-b", "goose/session-task"])
            .output()
            .unwrap();

        assert!(is_goose_worktree(&repo).unwrap());
    }

    #[test]
    fn plan_uses_safe_branch_and_directory_names() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("My Repo");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap();
        let plan = plan_worktree(&repo, "Session:ABC", Some("Long Task"), Some("HEAD")).unwrap();
        assert_eq!(plan.branch, "goose/session-abc-long-task");
        assert!(plan.worktree_dir.to_string_lossy().contains("my-repo"));
    }

    #[test]
    fn plan_avoids_existing_branch_and_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["checkout", "-b", "goose/session-task"])
            .output()
            .unwrap();
        std::fs::create_dir(tmp.path().join("repo-session-task")).unwrap();

        let plan = plan_worktree(&repo, "session", Some("task"), Some("HEAD")).unwrap();
        assert_eq!(plan.branch, "goose/session-task-2");
        assert!(plan.worktree_dir.ends_with("repo-session-task-2"));
    }

    #[test]
    fn linked_worktree_detection_and_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("repo-linked");
        std::fs::create_dir(&repo).unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "user.email", "test@example.com"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["config", "user.name", "Test User"])
            .output()
            .unwrap();
        std::fs::write(repo.join("README.md"), "test").unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["add", "README.md"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["commit", "-m", "init"])
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["worktree", "add", "-b", "goose/linked"])
            .arg(&linked)
            .output()
            .unwrap();

        assert!(!is_linked_worktree(&repo).unwrap());
        assert!(is_linked_worktree(&linked).unwrap());

        remove_worktree(&repo, &linked).unwrap();
        assert!(!linked.exists());
    }
}
