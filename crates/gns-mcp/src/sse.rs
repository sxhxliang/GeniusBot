//! Minimal Server-Sent Events parser (only `data:` lines matter for MCP).

/// Incremental SSE parser: feed bytes, take complete event payloads.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: String,
}

impl SseParser {
    /// Feed a chunk and return the `data` payloads of every event completed by it.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        self.buffer = self.buffer.replace("\r\n", "\n");
        let mut events = Vec::new();
        while let Some(end) = self.buffer.find("\n\n") {
            let block: String = self.buffer.drain(..end + 2).collect();
            if let Some(data) = event_data(&block) {
                events.push(data);
            }
        }
        events
    }

    /// Flush a trailing event that was not terminated by a blank line.
    pub fn finish(&mut self) -> Option<String> {
        let block = std::mem::take(&mut self.buffer);
        event_data(&block)
    }
}

fn event_data(block: &str) -> Option<String> {
    let lines: Vec<&str> =
        block.lines().filter_map(|line| line.strip_prefix("data:")).map(|rest| rest.strip_prefix(' ').unwrap_or(rest)).collect();
    if lines.is_empty() { None } else { Some(lines.join("\n")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_events_across_chunks_and_ignores_comments() {
        let mut p = SseParser::default();
        assert!(p.feed(b": keep-alive\n\nevent: message\ndata: {\"a\":").is_empty());
        let events = p.feed(b"1}\n\ndata: x\ndata: y\n\n");
        assert_eq!(events, vec!["{\"a\":1}".to_owned(), "x\ny".to_owned()]);
        assert!(p.feed(b"data: tail").is_empty());
        assert_eq!(p.finish().as_deref(), Some("tail"));
    }
}
