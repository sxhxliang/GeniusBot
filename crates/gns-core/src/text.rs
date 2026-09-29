//! Small text helpers shared by prompt builders and stores.

/// Clamp a single line to `max` characters, appending an ellipsis when cut.
pub fn clamp_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    clamp_chars(line, max)
}

/// Clamp a block of text to `max` characters, appending an ellipsis when cut.
pub fn clamp_block(text: &str, max: usize) -> String {
    clamp_chars(text.trim(), max)
}

fn clamp_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('…');
    out
}

/// Turn a human name into a filesystem-safe slug, falling back to `fallback`.
pub fn slugify_name(name: &str, fallback: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in name.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    let slug = slug.trim_matches('-').to_owned();
    if slug.is_empty() { fallback.to_owned() } else { slug.chars().take(60).collect() }
}

/// `CHAR_HARD_LIMIT`: the generic clamp applied to every tool result.
pub const CHAR_HARD_LIMIT: usize = 100_000;

/// `truncateOutput`: clamp to `max_length` characters, either keeping the
/// head (`... (output truncated)`) or the head and tail
/// (`... (output truncated) ...`). Returns the text and whether it was cut.
pub fn truncate_output(output: &str, max_length: usize, front_and_back: bool) -> (String, bool) {
    let chars: Vec<char> = output.chars().collect();
    if chars.len() <= max_length {
        return (output.to_owned(), false);
    }
    if front_and_back {
        let half = max_length / 2;
        let head: String = chars[..half].iter().collect();
        let tail: String = chars[chars.len() - half..].iter().collect();
        return (format!("{head}\n\n... (output truncated) ...\n\n{tail}"), true);
    }
    let head: String = chars[..max_length].iter().collect();
    (format!("{head}\n\n... (output truncated)"), true)
}

/// Current wall-clock time in milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Format a millisecond timestamp as `YYYY-MM-DD` (UTC).
pub fn format_date(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|dt| dt.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "unknown-date".to_owned())
}

/// Format a millisecond timestamp for humans (UTC). The runtime formats in
/// the user's IANA zone when it has one; core stays zone-agnostic.
pub fn format_timestamp(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| "unknown time".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_adds_ellipsis() {
        assert_eq!(clamp_block("hello world", 5), "hell…");
        assert_eq!(clamp_block("hi", 5), "hi");
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify_name("Daily Digest!", "x"), "daily-digest");
        assert_eq!(slugify_name("___", "automation"), "automation");
    }
}
