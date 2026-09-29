//! Memory persisted as markdown: `profile.md` and `log/YYYY-MM.md`, one fact
//! per line as `- (YYYY-MM-DD) fact` (`memory-service.ts`). One shard
//! directory per (scope, agent) so every file has a single writer.
//!
//! Only lines matching the original's `FACT_LINE` pattern are facts; the
//! headers written on file creation and any other prose are ignored.

use gns_core::HostError;
use gns_core::consts::{MEMORY_LOG_HEADER, MEMORY_PROFILE_HEADER};
use gns_core::memory::{MemoryKind, MemoryRecall, MemoryRecord, memory_dedupe_key, normalize_memory_content};
use gns_core::text::format_date;
use std::path::{Path, PathBuf};

/// A memory shard directory.
#[derive(Clone, Debug)]
pub struct MemoryFiles {
    dir: PathBuf,
}

/// One parsed fact with its location, for in-place removal.
struct ParsedFact {
    record: MemoryRecord,
    path: PathBuf,
    line: usize,
}

impl MemoryFiles {
    /// Point at a shard directory (created lazily on write).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
    pub fn dir(&self) -> &Path {
        &self.dir
    }
    fn profile_path(&self) -> PathBuf {
        self.dir.join("profile.md")
    }
    fn log_dir(&self) -> PathBuf {
        self.dir.join("log")
    }
    fn log_path_for(&self, created_at: i64) -> PathBuf {
        let month = format_date(created_at).chars().take(7).collect::<String>();
        self.log_dir().join(format!("{month}.md"))
    }
    fn log_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(self.log_dir()) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|e| e == "md")).collect(),
            Err(_) => Vec::new(),
        };
        files.sort();
        files
    }
    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_default()
    }

    /// Every fact in file order: profile first, then the log files sorted by name.
    fn facts(&self) -> Vec<ParsedFact> {
        let mut out = Vec::new();
        let profile = self.profile_path();
        out.extend(parse_facts(&Self::read(&profile), MemoryKind::Profile).into_iter().map(|(line, record)| ParsedFact {
            record,
            path: profile.clone(),
            line,
        }));
        for path in self.log_files() {
            out.extend(parse_facts(&Self::read(&path), MemoryKind::Log).into_iter().map(|(line, record)| ParsedFact {
                record,
                path: path.clone(),
                line,
            }));
        }
        out
    }

    /// Read every fact (profile facts, then log facts in chronological order).
    pub fn recall(&self) -> Result<MemoryRecall, HostError> {
        let mut profile = Vec::new();
        let mut recent = Vec::new();
        for fact in self.facts() {
            match fact.record.kind {
                MemoryKind::Profile => profile.push(fact.record),
                MemoryKind::Log => recent.push(fact.record),
            }
        }
        recent.sort_by_key(|r| r.created_at);
        Ok(MemoryRecall { profile, recent })
    }

    /// Append a fact unless an equivalent one already exists (case-insensitive,
    /// whitespace-collapsed compare across profile and log). Returns the
    /// record when written, `None` when the fact is empty or already recorded.
    pub fn write(&self, content: &str, kind: MemoryKind, now_ms: i64) -> Result<Option<MemoryRecord>, HostError> {
        let content = normalize_memory_content(content);
        if content.is_empty() {
            return Ok(None);
        }
        let key = memory_dedupe_key(&content);
        if self.facts().iter().any(|f| memory_dedupe_key(&f.record.content) == key) {
            return Ok(None);
        }
        let path = match kind {
            MemoryKind::Profile => self.profile_path(),
            MemoryKind::Log => self.log_path_for(now_ms),
        };
        let raw = Self::read(&path);
        let base = if raw.is_empty() {
            match kind {
                MemoryKind::Profile => MEMORY_PROFILE_HEADER,
                MemoryKind::Log => MEMORY_LOG_HEADER,
            }
        } else {
            raw.as_str()
        };
        let mut text = base.to_owned();
        if !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&serialize_fact_line(&content, now_ms));
        text.push('\n');
        crate::write_text_atomic(&path, &text)?;
        Ok(Some(MemoryRecord { content, created_at: now_ms, kind }))
    }

    /// Remove the FIRST recorded fact whose text matches `content`
    /// (normalised compare). Returns whether a line was removed.
    pub fn forget(&self, content: &str) -> Result<bool, HostError> {
        let key = memory_dedupe_key(content);
        if key.is_empty() {
            return Ok(false);
        }
        let Some(fact) = self.facts().into_iter().find(|f| memory_dedupe_key(&f.record.content) == key) else {
            return Ok(false);
        };
        let raw = Self::read(&fact.path);
        let mut lines: Vec<&str> = raw.split('\n').collect();
        if fact.line < lines.len() {
            lines.remove(fact.line);
        }
        crate::write_text_atomic(&fact.path, &lines.join("\n"))?;
        Ok(true)
    }
}

/// `- (YYYY-MM-DD) fact`
pub fn serialize_fact_line(content: &str, created_at: i64) -> String {
    format!("- ({}) {content}", format_date(created_at))
}

/// Parse a memory file: `(line index, record)` for every line matching
/// `^-\s+\((\d{4}-\d{2}-\d{2})\)\s+(.+?)\s*$`. The date is midnight UTC.
pub fn parse_facts(raw: &str, kind: MemoryKind) -> Vec<(usize, MemoryRecord)> {
    raw.split('\n')
        .enumerate()
        .filter_map(|(line, text)| {
            let (date, body) = match_fact_line(text)?;
            let content = normalize_memory_content(body);
            if content.is_empty() {
                return None;
            }
            let created_at = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
                .ok()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .map(|dt| dt.and_utc().timestamp_millis())
                .unwrap_or(0);
            Some((line, MemoryRecord { content, created_at, kind }))
        })
        .collect()
}

/// Hand-rolled `FACT_LINE`: returns `(date, fact)` when the line matches.
fn match_fact_line(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix('-')?;
    let after_dash = rest.trim_start();
    if after_dash.len() == rest.len() {
        return None; // `-` must be followed by whitespace
    }
    let rest = after_dash.strip_prefix('(')?;
    let close = rest.find(')')?;
    let date = &rest[..close];
    let bytes = date.as_bytes();
    let is_date =
        bytes.len() == 10 && bytes.iter().enumerate().all(|(i, b)| if i == 4 || i == 7 { *b == b'-' } else { b.is_ascii_digit() });
    if !is_date {
        return None;
    }
    let after = &rest[close + 1..];
    let fact = after.trim_start();
    if fact.len() == after.len() {
        return None; // `)` must be followed by whitespace
    }
    let fact = fact.trim_end();
    if fact.is_empty() {
        return None;
    }
    Some((date, fact))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_recall_forget() {
        let dir = tempfile::tempdir().unwrap();
        let mem = MemoryFiles::new(dir.path().join("memory"));
        let now = 1_750_000_000_000;
        assert!(mem.write("User prefers pnpm", MemoryKind::Profile, now).unwrap().is_some());
        assert!(mem.write("user prefers   PNPM", MemoryKind::Log, now).unwrap().is_none(), "dedupe");
        assert!(mem.write("[note] trying vitest", MemoryKind::Log, now).unwrap().is_some());
        assert!(mem.write("   ", MemoryKind::Log, now).unwrap().is_none(), "empty");
        let recall = mem.recall().unwrap();
        assert_eq!(recall.profile.len(), 1);
        assert_eq!(recall.recent.len(), 1);
        assert_eq!(format_date(recall.recent[0].created_at), format_date(now));
        assert!(mem.forget("User prefers pnpm").unwrap());
        assert!(mem.recall().unwrap().profile.is_empty());
        assert!(!mem.forget("nothing").unwrap());
    }

    #[test]
    fn files_get_the_original_headers_and_only_fact_lines_parse() {
        let dir = tempfile::tempdir().unwrap();
        let mem = MemoryFiles::new(dir.path().join("memory"));
        let now = 1_750_000_000_000;
        mem.write("Likes tea", MemoryKind::Profile, now).unwrap();
        mem.write("Asked about vitest", MemoryKind::Log, now).unwrap();
        let profile = std::fs::read_to_string(dir.path().join("memory/profile.md")).unwrap();
        assert!(profile.starts_with(MEMORY_PROFILE_HEADER), "{profile}");
        assert!(profile.ends_with(&format!("- ({}) Likes tea\n", format_date(now))));
        let log = std::fs::read_to_string(dir.path().join(format!("memory/log/{}.md", &format_date(now)[..7]))).unwrap();
        assert!(log.starts_with(MEMORY_LOG_HEADER));
        // Headers, comments and undated bullets are not facts.
        let recall = mem.recall().unwrap();
        assert_eq!(recall.profile.iter().map(|r| r.content.as_str()).collect::<Vec<_>>(), vec!["Likes tea"]);
        let parsed = parse_facts("# About\n\n<!-- x -->\n- undated\n-(2026-01-02) nospace\n- (2026-01-02) real fact  \n", MemoryKind::Log);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].0, 5);
        assert_eq!(parsed[0].1.content, "real fact");
        assert_eq!(parsed[0].1.created_at, 1_767_312_000_000, "midnight UTC");
    }

    #[test]
    fn forget_removes_only_the_first_matching_line() {
        let dir = tempfile::tempdir().unwrap();
        let mem = MemoryFiles::new(dir.path().join("memory"));
        let path = dir.path().join("memory/profile.md");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "# h\n- (2026-01-01) dup\n- (2026-01-02) keep\n- (2026-01-03) dup\n").unwrap();
        assert!(mem.forget("DUP").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# h\n- (2026-01-02) keep\n- (2026-01-03) dup\n");
    }
}
