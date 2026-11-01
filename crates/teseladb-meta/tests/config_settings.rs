//! 007 T045: what the documents of `config.settings` that Group 0 keeps turn on and off (FR-017):
//! the balancer's moves (`mode: "off"`, `stopped: true`, outside `activeWindow` or
//! `activeWindowDOW` in local time) and automatic merges (`automerge` with `enabled: false`).

use std::collections::BTreeMap;

use teseladb_bson::Document;
use teseladb_meta::config_settings::{AUTOMERGE_KEY, BALANCER_KEY, automerge, balancing};

fn with(key: &[u8], doc: Document) -> BTreeMap<Vec<u8>, Vec<u8>> {
    BTreeMap::from([(key.to_vec(), doc.to_bytes().unwrap())])
}

/// 10:30 UTC.
const MORNING: u64 = (10 * 60 + 30) * 60_000;

#[test]
fn the_balancer_moves_unless_stopped_or_outside_its_window() {
    let none = BTreeMap::new();
    assert!(balancing(&none, MORNING), "no document: on");
    let doc = |d: Document| with(BALANCER_KEY, d.with("_id", "balancer"));
    assert!(!balancing(&doc(Document::new().with("mode", "off")), MORNING));
    assert!(!balancing(&doc(Document::new().with("stopped", true)), MORNING));
    assert!(balancing(&doc(Document::new().with("mode", "full").with("stopped", false)), MORNING));
    let window = |start: &str, stop: &str| {
        doc(Document::new()
            .with("activeWindow", Document::new().with("start", start).with("stop", stop)))
    };
    assert!(balancing(&window("09:00", "11:00"), MORNING));
    assert!(!balancing(&window("11:00", "12:00"), MORNING));
    // Across midnight.
    assert!(balancing(&window("22:00", "11:00"), MORNING));
    assert!(!balancing(&window("22:00", "06:00"), MORNING));
    assert!(balancing(&window("10:00", "06:00"), MORNING));
    // A window that does not read stops the balancer: the config server fails to read its
    // settings and skips the round (measured on 9.0.4).
    assert!(!balancing(&window("9h", "11:00"), MORNING));
    assert!(!balancing(&window("25:00", "11:00"), MORNING));
    assert!(!balancing(&window("10:00", "10:00"), MORNING));
    let numbers = Document::new().with("start", 9).with("stop", 11);
    assert!(!balancing(&doc(Document::new().with("activeWindow", numbers)), MORNING));
    // A document that does not read leaves the balancer on.
    assert!(balancing(&BTreeMap::from([(BALANCER_KEY.to_vec(), vec![1])]), MORNING));
}

/// Sunday 1 November 2026, 22:24 (local time).
const SUNDAY_NIGHT: u64 = 1_793_571_840_000;

fn days(days: &[(&str, &str, &str)]) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let list: Vec<teseladb_bson::Value> = days
        .iter()
        .map(|(day, start, stop)| {
            Document::new().with("day", *day).with("start", *start).with("stop", *stop).into()
        })
        .collect();
    with(BALANCER_KEY, Document::new().with("_id", "balancer").with("activeWindowDOW", list))
}

#[test]
fn the_days_of_the_week_open_the_balancer_on_their_day() {
    // Measured on a config server 9.0.4 on Sunday 1 November 2026 at 22:24 local time.
    assert!(balancing(&days(&[("Sunday", "22:00", "23:00")]), SUNDAY_NIGHT));
    assert!(!balancing(&days(&[("Monday", "22:00", "23:00")]), SUNDAY_NIGHT));
    let both = [("Monday", "01:00", "02:00"), ("Sunday", "22:00", "23:00")];
    assert!(balancing(&days(&both), SUNDAY_NIGHT));
    // Across midnight the window counts on its own day: before `stop` or from `start`.
    assert!(balancing(&days(&[("Sunday", "23:00", "22:50")]), SUNDAY_NIGHT));
    assert!(!balancing(&days(&[("Saturday", "23:00", "22:50")]), SUNDAY_NIGHT));
    // An empty list leaves the balancer on.
    assert!(balancing(&days(&[]), SUNDAY_NIGHT));
    // With days, `activeWindow` does not count.
    let mut d = days(&[("Monday", "22:00", "23:00")]);
    let doc = Document::new()
        .with("_id", "balancer")
        .with(
            "activeWindowDOW",
            vec![teseladb_bson::Value::from(
                Document::new().with("day", "Sunday").with("start", "22:00").with("stop", "23:00"),
            )],
        )
        .with("activeWindow", Document::new().with("start", "01:00").with("stop", "02:00"));
    assert!(balancing(&with(BALANCER_KEY, doc), SUNDAY_NIGHT));
    let doc = Document::new()
        .with("_id", "balancer")
        .with(
            "activeWindowDOW",
            vec![teseladb_bson::Value::from(
                Document::new().with("day", "Monday").with("start", "22:00").with("stop", "23:00"),
            )],
        )
        .with("activeWindow", Document::new().with("start", "22:00").with("stop", "23:00"));
    d.insert(BALANCER_KEY.to_vec(), doc.to_bytes().unwrap());
    assert!(!balancing(&d, SUNDAY_NIGHT));
}

#[test]
fn days_that_do_not_read_stop_the_balancer() {
    // The config server fails to read its settings and skips the round.
    for bad in [
        [("sunday", "22:00", "23:00")],
        [("Sun", "22:00", "23:00")],
        [("Sunday", "22:00", "22:00")],
        [("Sunday", "2200", "23:00")],
    ] {
        assert!(!balancing(&days(&bad), SUNDAY_NIGHT), "{bad:?}");
    }
    let missing = Document::new().with("day", "Sunday").with("start", "22:00");
    let doc = Document::new().with("activeWindowDOW", vec![teseladb_bson::Value::from(missing)]);
    assert!(!balancing(&with(BALANCER_KEY, doc), SUNDAY_NIGHT));
}

#[test]
fn automatic_merges_run_unless_turned_off() {
    assert!(automerge(&BTreeMap::new()));
    assert!(!automerge(&with(AUTOMERGE_KEY, Document::new().with("enabled", false))));
    assert!(automerge(&with(AUTOMERGE_KEY, Document::new().with("enabled", true))));
}
