//! IANA time zones without I/O: the database that the reference server 9.0.4 carries in its timelib
//! (version 2024a with the `backzone` history, measured on a reference router 9.0.4: Almaty is at
//! +05 in 2025, Asunción at −04 in July 2025, Amsterdam at +01:19:32 in 1935), generated into
//! [`tzdata`] by `teseladb-tzgen`. Names are case-sensitive, as in the reference server.
//!
//! A zone gives the local type (offset, daylight saving and abbreviation) of any instant: its
//! transitions up to the last one the data lists, and its POSIX rule after that.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod posix;
pub mod tzdata;

pub use posix::Rule;

/// One local time type of a zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalType {
    /// Seconds east of UTC.
    pub offset: i32,
    /// Whether it is daylight saving time.
    pub dst: bool,
    /// Its abbreviation (`CET`, `+0530`, `LMT`).
    pub abbr: &'static str,
}

/// The data of one zone, as the generator writes it.
#[derive(Debug)]
pub struct ZoneData {
    /// The instants of its transitions, in seconds since the epoch, ascending.
    pub transitions: &'static [i64],
    /// The local type each transition goes to.
    pub indices: &'static [u8],
    /// Its local types; the first is the one before every transition.
    pub types: &'static [LocalType],
    /// The POSIX TZ rule for instants after the last transition.
    pub footer: &'static str,
}

/// A time zone of the database.
#[derive(Clone, Copy, Debug)]
pub struct Zone {
    name: &'static str,
    data: &'static ZoneData,
}

/// The type of an instant in a zone, and the instant's offset in seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Local {
    /// Seconds east of UTC.
    pub offset: i32,
    /// Whether it is daylight saving time.
    pub dst: bool,
    /// The abbreviation.
    pub abbr: &'static str,
}

impl Zone {
    /// The zone named `name`, as written in the database (`Europe/Madrid`, `US/Eastern`, `UTC`).
    #[must_use]
    pub fn named(name: &str) -> Option<Zone> {
        let i = tzdata::NAMES.binary_search_by(|(n, _)| (*n).cmp(name)).ok()?;
        let (name, index) = tzdata::NAMES[i];
        Some(Zone { name, data: &tzdata::ZONES[usize::from(index)] })
    }

    /// A zone of data that is not in the database (generated elsewhere, or for a test).
    #[must_use]
    pub fn from_data(name: &'static str, data: &'static ZoneData) -> Zone {
        Zone { name, data }
    }

    /// Its name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The local type at `secs` seconds since the epoch.
    #[must_use]
    pub fn at(&self, secs: i64) -> Local {
        let d = self.data;
        // After the last transition (or with none) the POSIX rule decides, when there is one.
        let past_last = d.transitions.last().is_none_or(|l| secs >= *l);
        if past_last && let Some(rule) = Rule::parse(d.footer) {
            return rule.at(secs);
        }
        let t = match d.transitions.partition_point(|t| *t <= secs) {
            0 => d.types[0],
            n => d.types[usize::from(d.indices[n - 1])],
        };
        Local { offset: t.offset, dst: t.dst, abbr: t.abbr }
    }

    /// The UTC instant of local time `local` (seconds since the epoch, read as wall clock time), as
    /// the reference server's timelib resolves it (measured on a reference router 9.0.4 with
    /// `$dateFromParts`): the offset at `local` read as UTC first, then the offset at the instant
    /// that gives. A time in a gap is read with the offset before it (`02:30` the night Madrid
    /// moves from `02:00` to `03:00` is `01:30Z`), and one that happens twice is the later (`02:30`
    /// the night it moves back is `01:30Z`, not `00:30Z`).
    #[must_use]
    pub fn utc_of(&self, local: i64) -> i64 {
        let first = i64::from(self.at(local).offset);
        let utc = local - first;
        let second = i64::from(self.at(utc).offset);
        if second == first { utc } else { local - second }
    }
}

/// The version of the IANA database.
#[must_use]
pub fn version() -> &'static str {
    tzdata::VERSION
}

/// Every zone name, in order.
pub fn names() -> impl Iterator<Item = &'static str> {
    tzdata::NAMES.iter().map(|(n, _)| *n)
}
