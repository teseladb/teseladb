//! Log lines as text and JSON, level filter, escapes and timestamps.

use std::io::Write;
use std::sync::{Arc, Mutex};

use teseladb_server::log::{Format, Level, Logger, timestamp};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn timestamps_follow_the_calendar() {
    assert_eq!(timestamp(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(timestamp(951_782_400_000), "2000-02-29T00:00:00.000Z");
    assert_eq!(timestamp(1_767_225_600_123), "2026-01-01T00:00:00.123Z");
    assert_eq!(timestamp(4_102_444_799_999), "2099-12-31T23:59:59.999Z");
}

#[test]
fn text_lines_quote_values_that_need_it() {
    let l = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    let line =
        l.format_line(0, Level::Warn, "disk slow", &[("core", &3), ("path", &"a b"), ("e", &"")]);
    assert_eq!(line, "1970-01-01T00:00:00.000Z warn disk slow core=3 path=\"a b\" e=\"\"\n");
    // A control character, a quote or an equals sign could forge another field: quoted too.
    let line =
        l.format_line(0, Level::Info, "m", &[("c", &"\u{1}x"), ("q", &"a\"b"), ("k", &"x=y")]);
    assert_eq!(line, "1970-01-01T00:00:00.000Z info m c=\"\\u0001x\" q=\"a\\\"b\" k=\"x=y\"\n");
}

#[test]
fn json_lines_escape_everything() {
    let l = Logger::new(Level::Info, Format::Json, Box::new(std::io::sink()));
    let line = l.format_line(0, Level::Error, "a \"b\"\n", &[("k", &"\t\\\u{1}é")]);
    assert_eq!(
        line,
        "{\"ts\":\"1970-01-01T00:00:00.000Z\",\"level\":\"error\",\"msg\":\"a \\\"b\\\"\\n\",\"k\":\"\\t\\\\\\u0001é\"}\n"
    );
}

#[test]
fn the_level_filters_before_writing() {
    let sink = Sink::default();
    let l = Logger::new(Level::Info, Format::Text, Box::new(sink.clone()));
    assert!(format!("{l:?}").contains("Info"));
    l.debug("hidden", &[]);
    l.info("shown", &[]);
    l.warn("shown", &[]);
    l.error("shown", &[("r", &"\r")]);
    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert_eq!(text.lines().count(), 3);
    assert!(!text.contains("hidden"));
    assert!(text.contains("r=\"\\r\""));
    assert!(l.enabled(Level::Error) && !l.enabled(Level::Trace));
}

#[test]
fn levels_and_formats_parse() {
    for (s, l) in [
        ("error", Level::Error),
        ("warn", Level::Warn),
        ("info", Level::Info),
        ("debug", Level::Debug),
        ("trace", Level::Trace),
    ] {
        assert_eq!(s.parse::<Level>(), Ok(l));
    }
    assert!("loud".parse::<Level>().is_err());
    assert_eq!("text".parse::<Format>(), Ok(Format::Text));
    assert_eq!("json".parse::<Format>(), Ok(Format::Json));
    assert!("xml".parse::<Format>().is_err());
    let l = Logger::new(Level::Trace, Format::Text, Box::new(std::io::sink()));
    assert!(l.format_line(0, Level::Debug, "m", &[]).contains(" debug m"));
    assert!(l.format_line(0, Level::Trace, "m", &[]).contains(" trace m"));
}
