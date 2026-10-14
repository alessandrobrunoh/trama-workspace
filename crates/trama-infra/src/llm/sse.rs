//! Incremental Server-Sent Events parser (the subset chat-completions APIs
//! use): `data:` fields joined by newlines, dispatched on a blank line.
//! Comments (`:`) and other fields (`event:`, `id:`, `retry:`) are ignored.

/// Feed raw bytes in arbitrary chunks; complete events come out in order.
#[derive(Debug, Default)]
pub struct SseParser {
    buffer: Vec<u8>,
    data: Option<String>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `chunk` and return the `data` payload of every event it
    /// completed.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.buffer.iter().position(|b| *b == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            let line = String::from_utf8_lossy(&line);
            if line.is_empty() {
                if let Some(data) = self.data.take() {
                    events.push(data);
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line.as_ref(), ""),
            };
            if field == "data" {
                match &mut self.data {
                    Some(data) => {
                        data.push('\n');
                        data.push_str(value);
                    }
                    None => self.data = Some(value.to_owned()),
                }
            }
        }
        events
    }

    /// End of stream: an event not followed by a blank line still counts.
    pub fn finish(&mut self) -> Option<String> {
        if !self.buffer.is_empty() {
            let rest = std::mem::take(&mut self.buffer);
            let mut events = self.push(&rest);
            events.extend(self.push(b"\n\n"));
            return events.pop();
        }
        self.data.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_across_chunks() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: {\"a\"").is_empty());
        assert_eq!(parser.push(b":1}\r\n\r\ndata: x\n"), vec!["{\"a\":1}"]);
        assert_eq!(parser.push(b"data: y\n\n"), vec!["x\ny"]);
    }

    #[test]
    fn ignores_comments_and_other_fields() {
        let mut parser = SseParser::new();
        let events = parser.push(b": keep-alive\n\nevent: delta\nid: 3\ndata:[DONE]\n\n");
        assert_eq!(events, vec!["[DONE]"]);
    }

    #[test]
    fn finish_flushes_unterminated_event() {
        let mut parser = SseParser::new();
        assert!(parser.push(b"data: tail").is_empty());
        assert_eq!(parser.finish().as_deref(), Some("tail"));
    }
}
