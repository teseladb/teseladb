//! The documents of `config.settings` that Group 0 keeps (007 FR-017, contract `config-db.md`),
//! each in the setting [`PREFIX`] followed by its `_id`, and what they turn on and off: the
//! balancer's moves for space, numbers and load (`balancer` with `mode: "off"` or
//! `stopped: true`, or outside its `activeWindow` or `activeWindowDOW` in the local time of
//! Group 0's leader) and automatic merges (`automerge` with
//! `enabled: false`). The repairs of the balancer (a missing replica, a node being drained) go on
//! whatever the balancer's document says.

use std::collections::BTreeMap;

use teseladb_bson::{RawDoc, RawValue};

/// Prefix of the settings that keep a document of `config.settings`, followed by its `_id`.
pub const PREFIX: &[u8] = b"configSettings.";
/// The setting with the balancer's document.
pub const BALANCER_KEY: &[u8] = b"configSettings.balancer";
/// The setting with the document of automatic merges.
pub const AUTOMERGE_KEY: &[u8] = b"configSettings.automerge";

/// Minutes in a day.
const DAY_MINUTES: u64 = 24 * 60;

fn doc<'a>(settings: &'a BTreeMap<Vec<u8>, Vec<u8>>, key: &[u8]) -> Option<RawDoc<'a>> {
    RawDoc::from_bytes(settings.get(key)?).ok()
}

/// Minutes since midnight of `HH:MM`.
fn minutes(text: &str) -> Option<u64> {
    let (h, m) = text.split_once(':')?;
    let (h, m) = (h.parse::<u64>().ok()?, m.parse::<u64>().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

/// The days of `activeWindowDOW`, from Sunday as a day number counts them.
const DAYS: [&str; 7] =
    ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];

/// Day number of 1 January 1970, a Thursday.
const EPOCH_DAY: u64 = 4;

/// Whether `now` (minutes since midnight) is in the window from `start` to `stop`, which goes
/// across midnight when `stop` comes before `start`.
fn within(start: u64, stop: u64, now: u64) -> bool {
    match start <= stop {
        true => (start..stop).contains(&now),
        false => now >= start || now < stop,
    }
}

/// The window of an entry of `activeWindowDOW`: its day number, `start` and `stop`; `None` when
/// it does not read, which stops the balancer as a config server that fails to read its settings
/// skips the round (measured on 9.0.4: a day not written as `Sunday`, a time not `hh:mm`, a
/// missing field, `start` equal to `stop`).
fn day_window(entry: &RawValue<'_>) -> Option<(u64, u64, u64)> {
    let RawValue::Document(d) = entry else { return None };
    let text = |k: &str| match d.get(k) {
        Some(RawValue::String(s)) => Some(s),
        _ => None,
    };
    let day = DAYS.iter().position(|n| Some(*n) == text("day"))? as u64;
    let (start, stop) = (minutes(text("start")?)?, minutes(text("stop")?)?);
    (start != stop).then_some((day, start, stop))
}

/// Whether the balancer moves replicas and leaders at `local_ms`, the local time of Group 0's
/// leader in milliseconds since the epoch (a config server reads the window in its own local
/// time, measured on 9.0.4). With `activeWindowDOW` the balancer moves on the days it lists, in
/// each day's window, and `activeWindow` does not count; an empty list counts as none.
pub fn balancing(settings: &BTreeMap<Vec<u8>, Vec<u8>>, local_ms: u64) -> bool {
    let Some(d) = doc(settings, BALANCER_KEY) else { return true };
    if matches!(d.get("mode"), Some(RawValue::String("off")))
        || matches!(d.get("stopped"), Some(RawValue::Boolean(true)))
    {
        return false;
    }
    let now = local_ms / 60_000 % DAY_MINUTES;
    let today = (local_ms / 86_400_000 + EPOCH_DAY) % 7;
    let days: Vec<RawValue<'_>> = match d.get("activeWindowDOW") {
        Some(RawValue::Array(list)) => list.iter().collect(),
        _ => Vec::new(),
    };
    if !days.is_empty() {
        let windows: Option<Vec<_>> = days.iter().map(day_window).collect();
        let Some(windows) = windows else { return false };
        return windows
            .iter()
            .any(|(day, start, stop)| *day == today && within(*start, *stop, now));
    }
    let Some(RawValue::Document(w)) = d.get("activeWindow") else { return true };
    let bound = |k: &str| match w.get(k) {
        Some(RawValue::String(s)) => minutes(s),
        _ => None,
    };
    // A window that does not read stops the balancer, as for the days.
    match (bound("start"), bound("stop")) {
        (Some(start), Some(stop)) if start != stop => within(start, stop, now),
        _ => false,
    }
}

/// Whether ranges merge on their own.
pub fn automerge(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> bool {
    doc(settings, AUTOMERGE_KEY)
        .is_none_or(|d| !matches!(d.get("enabled"), Some(RawValue::Boolean(false))))
}
