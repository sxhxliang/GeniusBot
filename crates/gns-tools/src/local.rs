//! `Read` (`packages/agent/tools/core/read/read.ts`), confined to the agent's
//! workspace and data directories. Reading a directory lists its entries (an
//! extension of the original).

use crate::paths::sandbox_path;
use async_trait::async_trait;
use gns_core::*;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

/// Extensions the original refuses to read as text.
const UNSUPPORTED_BINARY_EXTENSIONS: &[&str] =
    &["zip", "tar", "gz", "exe", "dll", "so", "dylib", "bin", "mp4", "webm", "mov", "avi", "mkv", "wmv", "flv", "m4v"];

/// Arguments of `Read`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ReadArgs {
    /// The absolute path of the file to read (a path relative to your workspace is also accepted).
    pub path: String,
    /// The line number to start reading from. Only provide if the file is too large to read at once.
    #[serde(default)]
    pub offset: Option<i64>,
    /// The number of lines to read. Only provide if the file is too large to read at once.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Default)]
pub struct ReadTool;

impl ReadTool {
    pub fn arc() -> Arc<dyn Tool> {
        Typed::arc(Self)
    }
}

pub(crate) fn roots(ctx: &ToolContext) -> [&Path; 2] {
    [ctx.workspace_dir.as_path(), ctx.data_dir.as_path()]
}

/// `LINE_NUMBER|LINE_CONTENT` with the number padded to 6 (`addLineNumbersDefault`).
pub fn add_line_numbers(content: &str, start_line: usize) -> String {
    content.split('\n').enumerate().map(|(i, line)| format!("{:>6}|{line}", start_line + i)).collect::<Vec<_>>().join("\n")
}

/// Render a read result: numbered lines plus `... N lines not shown ...`
/// notes when a range was requested (`formatCodeBlock`).
pub fn format_read_output(content: &str, start_line: usize, total_lines: usize, ranged: bool) -> String {
    let mut result = add_line_numbers(content, start_line);
    if ranged {
        if start_line > 1 {
            let hidden = start_line - 1;
            result = format!("... {hidden} {} not shown ...\n{result}", if hidden == 1 { "line" } else { "lines" });
        }
        let end_line = start_line + content.split('\n').count() - 1;
        if total_lines > end_line {
            let hidden = total_lines - end_line;
            result = format!("{result}\n... {hidden} {} not shown ...", if hidden == 1 { "line" } else { "lines" });
        }
    }
    result
}

#[async_trait]
impl TypedTool for ReadTool {
    type Args = ReadArgs;
    fn name(&self) -> &str {
        READ_TOOL_NAME
    }
    fn availability(&self) -> ToolAvailability {
        ToolAvailability::Local
    }
    fn description(&self) -> &str {
        "Reads a file on your own computer, the same filesystem Shell acts on. Reading a directory lists its entries.\n\nText files include line numbers and support offset/limit paging."
    }
    async fn run(&self, ctx: &ToolContext, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let path = sandbox_path(&args.path, &ctx.workspace_dir, &roots(ctx))?;
        let display = path.display().to_string();
        if path.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&path)
                .map_err(|e| ToolError::failed(e.to_string()))?
                .filter_map(|e| e.ok())
                .map(|e| {
                    let mut n = e.file_name().to_string_lossy().into_owned();
                    if e.path().is_dir() {
                        n.push('/');
                    }
                    n
                })
                .collect();
            names.sort();
            return Ok(
                ToolOutput::text(if names.is_empty() { "(empty directory)".to_owned() } else { names.join("\n") }).with_summary(display)
            );
        }
        if let Some(ext) = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)
            && UNSUPPORTED_BINARY_EXTENSIONS.contains(&ext.as_str())
        {
            return Err(ToolError::failed(format!("Cannot read binary files of type {ext}")));
        }
        // Validate offset/limit the way the zod schema does (0 → 1).
        let offset = match args.offset {
            None => None,
            Some(0) => Some(1usize),
            Some(n) if n >= 1 => Some(n as usize),
            Some(_) => return Err(ToolError::input("Offset must be >= 1.")),
        };
        let limit = match args.limit {
            None => None,
            Some(n) if n >= 1 => Some(n as usize),
            Some(_) => return Err(ToolError::input("Limit must be >= 1.")),
        };
        let raw = match tokio::fs::read(&path).await {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ToolError::failed("File not found")),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return Err(ToolError::failed("Permission denied")),
            Err(e) => return Err(ToolError::failed(format!("cannot read {display}: {e}"))),
        };
        let file_size = raw.len();
        let content = String::from_utf8_lossy(&raw);
        let ranged = offset.is_some() || limit.is_some();
        if content.is_empty() && !ranged {
            return Ok(ToolOutput::text("File is empty.").with_summary(display));
        }
        let lines: Vec<&str> = content.split('\n').collect();
        let total_lines = lines.len();
        let (start_index, slice) = if ranged {
            let effective_offset = offset.unwrap_or(1);
            let start_index = effective_offset - 1;
            if start_index >= total_lines {
                return Err(ToolError::failed(format!("Offset {effective_offset} is beyond file length ({total_lines} lines)")));
            }
            let end_index = (start_index + limit.unwrap_or(total_lines)).min(total_lines);
            (start_index, lines[start_index..end_index].join("\n"))
        } else {
            (0, content.into_owned())
        };
        if slice.chars().count() > READ_CHAR_HARD_LIMIT {
            return Ok(ToolOutput::text(format!(
                "File content ({file_size} characters) exceeds maximum allowed characters ({READ_CHAR_HARD_LIMIT} characters).\nPlease use offset and limit parameters to read specific portions of the file, or use the 'grep' tool to search for specific content."
            ))
            .with_summary(display));
        }
        Ok(ToolOutput::text(format_read_output(&slice, start_index + 1, total_lines, ranged)).with_summary(display))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_numbers_and_omitted_notes() {
        assert_eq!(add_line_numbers("a\nb", 1), "     1|a\n     2|b");
        assert_eq!(format_read_output("a\nb", 1, 2, false), "     1|a\n     2|b");
        assert_eq!(format_read_output("c", 3, 5, true), "... 2 lines not shown ...\n     3|c\n... 2 lines not shown ...");
        assert_eq!(format_read_output("b", 2, 2, true), "... 1 line not shown ...\n     2|b");
    }
}
