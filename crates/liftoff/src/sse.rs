//! A minimal server-sent-events parser: bytes in, whole events out.

/// One dispatched event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Event {
    /// The `event:` field; `message` when absent.
    pub(crate) name: String,
    /// The `data:` lines joined with `\n`.
    pub(crate) data: String,
}

/// Collects chunks of any size and yields the events they complete.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    pending: Vec<u8>,
    name: Option<String>,
    data: Vec<String>,
}

impl Parser {
    /// Feeds the next chunk of the body.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<Event> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let rest = self.pending.split_off(end.saturating_add(1));
            let raw = std::mem::replace(&mut self.pending, rest);
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if let Some(event) = self.line(line) {
                events.push(event);
            }
        }
        events
    }

    fn line(&mut self, line: &str) -> Option<Event> {
        if line.is_empty() {
            let name = self.name.take();
            let data = std::mem::take(&mut self.data);
            if data.is_empty() && name.is_none() {
                return None;
            }
            return Some(Event {
                name: name.unwrap_or_else(|| "message".to_owned()),
                data: data.join("\n"),
            });
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => self.data.push(value.to_owned()),
            "event" => self.name = Some(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod parser {
        use super::*;

        fn event(name: &str, data: &str) -> Event {
            Event {
                name: name.to_owned(),
                data: data.to_owned(),
            }
        }

        #[test]
        fn yields_message_when_data_line_ends_with_blank_line() {
            let mut parser = Parser::default();
            assert_eq!(parser.feed(b"data: hello\n\n"), [event("message", "hello")]);
        }

        #[test]
        fn waits_when_event_is_incomplete() {
            let mut parser = Parser::default();
            assert_eq!(parser.feed(b"data: abc"), []);
            assert_eq!(parser.feed(b"def\n"), []);
            assert_eq!(parser.feed(b"\n"), [event("message", "abcdef")]);
        }

        #[test]
        fn joins_data_lines_when_event_has_several() {
            let mut parser = Parser::default();
            assert_eq!(
                parser.feed(b"data: a\ndata: b\n\n"),
                [event("message", "a\nb")]
            );
        }

        #[test]
        fn names_event_when_event_field_given() {
            let mut parser = Parser::default();
            assert_eq!(
                parser.feed(b"event: error\r\ndata: boom\r\n\r\n"),
                [event("error", "boom")]
            );
        }

        #[test]
        fn skips_comments_when_keep_alive_arrives() {
            let mut parser = Parser::default();
            assert_eq!(parser.feed(b": keep-alive\n\n"), []);
        }

        #[test]
        fn keeps_multibyte_text_when_chunk_splits_a_character() {
            let mut parser = Parser::default();
            let bytes = "data: \u{e9}\n\n".as_bytes();
            let (first, second) = bytes.split_at(7);
            assert_eq!(parser.feed(first), []);
            assert_eq!(parser.feed(second), [event("message", "\u{e9}")]);
        }
    }
}
