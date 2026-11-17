//! Built-in tools available to the model.
//!
//! Every tool is approval-gated by default and returns both a short summary
//! (for the transcript) and the full content (for the model), so the same
//! result can be shown to the human and fed back to the engine.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;

/// A boxed, sendable future — the dyn-compatible return type of [`Tool::execute`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

use serde::{Deserialize, Serialize};
use similar::TextDiff;
use thiserror::Error;

use crate::model::ToolResult;

/// Errors produced by tools.
#[derive(Debug, Error)]
pub enum ToolError {
    /// Filesystem failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The tool was not found.
    #[error("unknown tool: {0}")]
    Unknown(String),
    /// The arguments were invalid.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),
    /// The requested path is outside the allowed roots.
    #[error("path not allowed: {0}")]
    PathNotAllowed(String),
}

/// The output of a tool execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// One-line summary for the transcript.
    pub summary: String,
    /// Full text handed back to the model.
    pub content: String,
    /// Unified diff, when the tool edited a file.
    #[serde(default)]
    pub diff: Option<String>,
}

impl ToolOutput {
    /// Build an output without a diff.
    #[must_use]
    pub fn new(summary: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            content: content.into(),
            diff: None,
        }
    }
}

/// A tool the model can invoke.
pub trait Tool: Send + Sync {
    /// Registered name, e.g. `read_file`.
    fn name(&self) -> &str;

    /// Short description shown in `/tools`.
    fn description(&self) -> &str;

    /// Execute the tool with JSON arguments.
    ///
    /// # Errors
    /// Returns an error when the arguments are invalid or the operation fails.
    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>>;
}

/// Arguments accepted by the file tools.
#[derive(Debug, Clone, Deserialize)]
struct PathArgs {
    path: PathBuf,
}

/// Arguments accepted by `grep`.
#[derive(Debug, Clone, Deserialize)]
struct GrepArgs {
    path: PathBuf,
    pattern: String,
}

/// Arguments accepted by `edit_file`.
#[derive(Debug, Clone, Deserialize)]
struct EditArgs {
    path: PathBuf,
    old: String,
    new: String,
}

/// Read a file from disk.
#[derive(Debug, Default)]
pub struct ReadFile {
    /// Roots the tool is allowed to read from.
    pub roots: Vec<PathBuf>,
}

impl ReadFile {
    /// Create the tool restricted to `roots`.
    #[must_use]
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    /// Check that `path` is inside an allowed root.
    ///
    /// # Errors
    /// Returns an error when the path escapes every allowed root.
    pub fn ensure_allowed(&self, path: &Path) -> Result<(), ToolError> {
        if self.roots.is_empty() {
            return Ok(());
        }
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        for root in &self.roots {
            let root = root.canonicalize().unwrap_or_else(|_| root.clone());
            if canonical.starts_with(&root) {
                return Ok(());
            }
        }
        Err(ToolError::PathNotAllowed(path.display().to_string()))
    }
}

/// List the entries of a directory.
#[derive(Debug, Default)]
pub struct ListDir;

/// Search files for a literal pattern.
#[derive(Debug, Default)]
pub struct Grep;

/// Replace a literal string inside a file.
#[derive(Debug, Default)]
pub struct EditFile;

/// Run a shell command (always approval-gated).
#[derive(Debug, Default)]
pub struct RunShell;

impl Tool for ReadFile {
    fn name(&self) -> &str {
        "read_file"
    }

    fn description(&self) -> &str {
        "Read a UTF-8 text file and return its contents."
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {        let args: PathArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        self.ensure_allowed(&args.path)?;
        let content = tokio::fs::read_to_string(&args.path).await?;
        let lines = content.lines().count();
        Ok(ToolOutput::new(
            format!("read {} ({lines} lines)", args.path.display()),
            content,
        ))
        })
    }
}

impl Tool for ListDir {
    fn name(&self) -> &str {
        "list_dir"
    }

    fn description(&self) -> &str {
        "List the files and directories at a path."
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {        let args: PathArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let mut entries = tokio::fs::read_dir(&args.path).await?;
        let mut names = Vec::new();
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name().to_string_lossy().to_string();
            let kind = if entry
                .file_type()
                .await
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                "/"
            } else {
                ""
            };
            names.push(format!("{kind}{name}"));
        }
        names.sort();
        let content = names.join("\n");
        Ok(ToolOutput::new(
            format!("listed {} ({} entries)", args.path.display(), names.len()),
            content,
        ))
        })
    }
}

impl Tool for Grep {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search a file for a literal pattern and return matching lines."
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {        let args: GrepArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let content = tokio::fs::read_to_string(&args.path).await?;
        let matches: Vec<String> = content
            .lines()
            .enumerate()
            .filter(|(_, line)| line.contains(&args.pattern))
            .map(|(idx, line)| format!("{}: {line}", idx + 1))
            .collect();
        let summary = format!(
            "{} match(es) for '{}' in {}",
            matches.len(),
            args.pattern,
            args.path.display()
        );
        Ok(ToolOutput::new(summary, matches.join("\n")))
        })
    }
}

impl Tool for EditFile {
    fn name(&self) -> &str {
        "edit_file"
    }

    fn description(&self) -> &str {
        "Replace a literal string in a file, returning a unified diff."
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {        let args: EditArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let original = tokio::fs::read_to_string(&args.path).await?;
        if !original.contains(&args.old) {
            return Err(ToolError::InvalidArguments(format!(
                "pattern not found in {}",
                args.path.display()
            )));
        }
        let updated = original.replace(&args.old, &args.new);
        let diff = unified_diff(
            &args.path.display().to_string(),
            &original,
            &updated,
        );
        tokio::fs::write(&args.path, &updated).await?;
        Ok(ToolOutput {
            summary: format!("edited {}", args.path.display()),
            content: format!("Applied edit to {}", args.path.display()),
            diff: Some(diff),
        })
        })
    }
}

impl Tool for RunShell {
    fn name(&self) -> &str {
        "run_shell"
    }

    fn description(&self) -> &str {
        "Run a shell command in the project directory."
    }

    fn execute(
        &self,
        arguments: serde_json::Value,
    ) -> BoxFuture<'_, Result<ToolOutput, ToolError>> {
        Box::pin(async move {        let args: PathArgs = serde_json::from_value(arguments.clone())
            .map_err(|e| ToolError::InvalidArguments(e.to_string()))?;
        let command = args.path.to_string_lossy().to_string();
        let output = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(&command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;
        let mut content = String::from_utf8_lossy(&output.stdout).to_string();
        if !output.stderr.is_empty() {
            content.push_str("\n--- stderr ---\n");
            content.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(ToolOutput::new(
            format!("ran `{command}` (exit {})", output.status.code().unwrap_or(-1)),
            content,
        ))
        })
    }
}

/// Registry of the tools exposed to the model.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl std::fmt::Debug for ToolRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolRegistry")
            .field("tools", &self.names())
            .finish()
    }
}

impl ToolRegistry {
    /// Build the registry with po's built-in tools.
    #[must_use]
    pub fn with_defaults(roots: Vec<PathBuf>) -> Self {
        let mut registry = Self::default();
        registry.register(Box::new(ReadFile::new(roots)));
        registry.register(Box::new(ListDir));
        registry.register(Box::new(Grep));
        registry.register(Box::new(EditFile));
        registry.register(Box::new(RunShell));
        registry
    }

    /// Add a tool to the registry.
    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Names of all registered tools.
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.tools.iter().map(|t| t.name()).collect()
    }

    /// Look up a tool by name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|t| &**t)
    }

    /// Execute a tool by name.
    ///
    /// # Errors
    /// Returns an error when the tool is unknown or the execution fails.
    pub async fn execute(
        &self,
        name: &str,
        arguments: &serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let tool = self
            .get(name)
            .ok_or_else(|| ToolError::Unknown(name.to_string()))?;
        let output = tool.execute(arguments.clone()).await?;
        Ok(ToolResult {
            id: String::new(),
            ok: true,
            summary: output.summary,
            content: output.content,
            diff: output.diff,
        })
    }
}

/// Build a unified diff between two versions of a file.
#[must_use]
pub fn unified_diff(path: &str, original: &str, updated: &str) -> String {
    let diff = TextDiff::from_lines(original, updated);
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => '-',
            similar::ChangeTag::Insert => '+',
            similar::ChangeTag::Equal => ' ',
        };
        out.push(sign);
        out.push_str(change.value());
        if !change.value().ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn read_file_returns_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "hello\nworld\n").await.expect("write");

        let tool = ReadFile::new(vec![dir.path().to_path_buf()]);
        let output = tool
            .execute(serde_json::json!({"path": file.to_string_lossy()}))
            .await
            .expect("execute");
        assert_eq!(output.content, "hello\nworld\n");
        assert!(output.summary.contains("2 lines"));
    }

    #[tokio::test]
    async fn read_file_rejects_paths_outside_the_allowed_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let tool = ReadFile::new(vec![dir.path().join("allowed").to_path_buf()]);
        let err = tool
            .execute(serde_json::json!({"path": "/etc/passwd"}))
            .await
            .expect_err("must reject");
        assert!(matches!(err, ToolError::PathNotAllowed(_)));
    }

    #[tokio::test]
    async fn edit_file_produces_a_diff() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "one\ntwo\n").await.expect("write");

        let tool = EditFile;
        let output = tool
            .execute(serde_json::json!({
                "path": file.to_string_lossy(),
                "old": "two",
                "new": "three"
            }))
            .await
            .expect("execute");
        let diff = output.diff.expect("diff present");
        assert!(diff.contains("-two"));
        assert!(diff.contains("+three"));
        let content = tokio::fs::read_to_string(&file).await.expect("read");
        assert_eq!(content, "one\nthree\n");
    }

    #[tokio::test]
    async fn grep_finds_matching_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("a.txt");
        tokio::fs::write(&file, "alpha\nbeta\nalpha again\n").await.expect("write");

        let tool = Grep;
        let output = tool
            .execute(serde_json::json!({
                "path": file.to_string_lossy(),
                "pattern": "alpha"
            }))
            .await
            .expect("execute");
        assert_eq!(output.content.lines().count(), 2);
    }

    #[tokio::test]
    async fn registry_exposes_the_builtin_tools() {
        let registry = ToolRegistry::with_defaults(Vec::new());
        let names = registry.names();
        for expected in [
            "read_file",
            "list_dir",
            "grep",
            "edit_file",
            "run_shell",
        ] {
            assert!(names.contains(&expected), "missing tool {expected}");
        }
    }
}
