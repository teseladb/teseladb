//! Server-Sent Events (HTML Living Standard, section 9.2): the events a server
//! writes on a `text/event-stream` answer, with their `id` for `Last-Event-ID`, and a reader of
//! the stream for the clients and the tests.

use std::fmt::Write as _;

/// The media type of an event stream.
pub const EVENT_STREAM: &str = "text/event-stream";
/// The field a client sends to resume after its last event.
pub const LAST_EVENT_ID: &str = "Last-Event-ID";
/// A comment line that keeps the connection busy (a heartbeat).
pub const HEARTBEAT: &[u8] = b": heartbeat\n\n";

/// One event.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// Its id: what the client sends back as `Last-Event-ID`.
    pub id: Option<String>,
    /// Its type (`message` when absent).
    pub event: Option<String>,
    /// Its data; each line goes in a `data:` line of its own.
    pub data: String,
    /// The reconnection time the client should use, in milliseconds.
    pub retry: Option<u64>,
}

impl Event {
    /// An event with `data` and nothing else.
    #[must_use]
    pub fn data(data: impl Into<String>) -> Event {
        Event { data: data.into(), ..Event::default() }
    }

    /// The bytes of the event. Line breaks in `id` and `event`, which would end their field,
    /// become spaces.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let one_line = |s: &str| s.replace(['\r', '\n'], " ");
        let mut out = String::new();
        if let Some(e) = &self.event {
            let _ = writeln!(out, "event: {}", one_line(e));
        }
        if let Some(id) = &self.id {
            let _ = writeln!(out, "id: {}", one_line(id).replace('\0', ""));
        }
        if let Some(r) = self.retry {
            let _ = writeln!(out, "retry: {r}");
        }
        for line in self.data.split('\n') {
            out.push_str("data: ");
            out.push_str(line.strip_suffix('\r').unwrap_or(line));
            out.push('\n');
        }
        out.push('\n');
        out.into_bytes()
    }
}

/// Reads the events of a stream that arrives in pieces, as a browser does.
#[derive(Clone, Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
    pending: Event,
    has_data: bool,
    /// The id of the last event dispatched (what a client would send as `Last-Event-ID`).
    pub last_id: Option<String>,
}

impl Reader {
    /// Takes `bytes`; the events they complete.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|b| *b == b'\n' || *b == b'\r') {
            // A CR followed by LF is one line end; a CR at the end may still be one.
            let crlf = self.buf[end] == b'\r' && self.buf.get(end + 1) == Some(&b'\n');
            if self.buf[end] == b'\r' && end + 1 == self.buf.len() {
                break;
            }
            let line = String::from_utf8_lossy(&self.buf[..end]).into_owned();
            self.buf.drain(..end + if crlf { 2 } else { 1 });
            if let Some(e) = self.line(&line) {
                events.push(e);
            }
        }
        events
    }

    fn line(&mut self, line: &str) -> Option<Event> {
        if line.is_empty() {
            let mut e = std::mem::take(&mut self.pending);
            let had = std::mem::take(&mut self.has_data);
            if e.id.is_some() {
                self.last_id.clone_from(&e.id);
            } else {
                e.id.clone_from(&self.last_id);
            }
            return had.then_some(e);
        }
        if line.starts_with(':') {
            return None;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.pending.data.push('\n');
                }
                self.pending.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.pending.event = Some(value.to_owned()),
            "id" if !value.contains('\0') => self.pending.id = Some(value.to_owned()),
            "retry" => self.pending.retry = value.parse().ok().or(self.pending.retry),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn events_are_written_with_their_fields() {
        let e = Event {
            id: Some("tok\n1".into()),
            event: Some("change".into()),
            data: "a\nb\r".into(),
            retry: Some(5),
        };
        assert_eq!(e.encode(), b"event: change\nid: tok 1\nretry: 5\ndata: a\ndata: b\n\n");
        assert_eq!(Event::data("").encode(), b"data: \n\n");
    }

    #[test]
    fn invalid_utf8_reads_as_replacement_characters() {
        // The SSE standard decodes the stream as UTF-8 with U+FFFD for what is not: each invalid
        // byte becomes three, so an event can hold more bytes than the stream had.
        let input = b"data:\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\n\n";
        let events = Reader::default().feed(input);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "\u{FFFD}".repeat(10));
        assert!(events[0].data.len() > input.len());
        assert!(events[0].data.len() <= 3 * input.len());
    }

    #[test]
    fn the_reader_gives_back_what_was_written() {
        let events = [
            Event {
                id: Some("1".into()),
                event: Some("change".into()),
                data: "{\"a\":1}\nx".into(),
                retry: None,
            },
            Event::data("plain"),
        ];
        let mut bytes = Vec::new();
        for e in &events {
            bytes.extend(e.encode());
        }
        bytes.extend_from_slice(HEARTBEAT);
        let mut r = Reader::default();
        let mut got = Vec::new();
        for chunk in bytes.chunks(3) {
            got.extend(r.feed(chunk));
        }
        // The second event keeps the last id, as a browser does.
        let mut want = events.to_vec();
        want[1].id = Some("1".into());
        assert_eq!(got, want);
        assert_eq!(r.last_id.as_deref(), Some("1"));
    }

    #[test]
    fn the_reader_takes_every_line_end_and_odd_fields() {
        let mut r = Reader::default();
        let got = r.feed(b"data:x\r\ndata\rretry: 7\nretry: no\nfoo: 1\nid: a\0b\nid\n\r");
        assert_eq!(got, []);
        let got = r.feed(b"\n\n");
        assert_eq!(
            got,
            [Event { id: Some(String::new()), event: None, data: "x\n".into(), retry: Some(7) }]
        );
        // A blank line without data dispatches nothing.
        assert_eq!(r.feed(b"event: e\n\n"), []);
        assert_eq!(r.feed(b"\r"), []);
    }
}
