//! Read preferences (007 FR-013): a read with `readPreference` goes to a replica of its range that
//! its mode allows, nearest first, among those whose tags match the first tag set that matches any;
//! a follower serves it at a closed mark no older than `maxStalenessSeconds`, or sends it on.

use std::fmt;

use teseladb_types::NodeId;

/// Smallest `maxStalenessSeconds` the reference server accepts.
pub const MIN_MAX_STALENESS_S: u64 = 90;

/// A read preference's mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The leader only.
    Primary,
    /// The leader, or a follower without one.
    PrimaryPreferred,
    /// Followers only.
    Secondary,
    /// Followers, or the leader without one.
    SecondaryPreferred,
    /// Any replica, nearest first.
    Nearest,
}

/// A tag set: name and value pairs a replica's tags must all have. `None` matches no replica (the
/// reference server takes a tag set that is not a document, or a tag whose value is not a string, and
/// no node's tags, which are strings, have it).
pub type TagSet = Option<Vec<(String, String)>>;

/// A read preference.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadPreference {
    /// Mode.
    pub mode: Mode,
    /// How old the data a follower serves may be, in milliseconds (`None`: no limit).
    pub max_staleness_ms: Option<u64>,
    /// Tag sets, in order.
    pub tag_sets: Vec<TagSet>,
}

/// Why a read preference is refused, as a reference router 9.0.4 refuses it (measured), in the
/// order it checks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadPrefError {
    /// A mode the reference server does not have (`BadValue`).
    UnknownMode(String),
    /// A negative `maxStalenessSeconds` (`BadValue`).
    StalenessNegative,
    /// A `maxStalenessSeconds` past `i64::MAX` (`BadValue`).
    StalenessTooLarge,
    /// A `maxStalenessSeconds` below [`MIN_MAX_STALENESS_S`] (`MaxStalenessOutOfRange`).
    StalenessTooSmall,
    /// Hedging enabled with `primary` (`InvalidOptions`).
    PrimaryWithHedge,
    /// `primary` with a tag set that is not empty (`BadValue`).
    PrimaryWithTags,
    /// `primary` with `maxStalenessSeconds` (`BadValue`).
    PrimaryWithStaleness,
}

/// The reference server's `BadValue`.
const BAD_VALUE: i32 = 2;
/// The reference server's `InvalidOptions`.
const INVALID_OPTIONS: i32 = 72;
/// The reference server's `MaxStalenessOutOfRange`.
const MAX_STALENESS_OUT_OF_RANGE: i32 = 160;

impl ReadPrefError {
    /// The reference server's code for it.
    pub fn code(&self) -> i32 {
        match self {
            ReadPrefError::StalenessTooSmall => MAX_STALENESS_OUT_OF_RANGE,
            ReadPrefError::PrimaryWithHedge => INVALID_OPTIONS,
            ReadPrefError::UnknownMode(_)
            | ReadPrefError::StalenessNegative
            | ReadPrefError::StalenessTooLarge
            | ReadPrefError::PrimaryWithTags
            | ReadPrefError::PrimaryWithStaleness => BAD_VALUE,
        }
    }
}

impl fmt::Display for ReadPrefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadPrefError::UnknownMode(m) => write!(
                f,
                "Enumeration value '{m}' for field 'readPreference.mode' is not a valid value."
            ),
            ReadPrefError::StalenessNegative => {
                f.write_str("maxStalenessSeconds must be a non-negative integer")
            }
            ReadPrefError::StalenessTooLarge => {
                write!(f, "maxStalenessSeconds value cannot exceed {}", i64::MAX)
            }
            ReadPrefError::StalenessTooSmall => {
                write!(f, "maxStalenessSeconds value cannot be less than {MIN_MAX_STALENESS_S}")
            }
            ReadPrefError::PrimaryWithHedge => {
                f.write_str("cannot enable hedging for $readPreference mode \"primaryOnly\"")
            }
            ReadPrefError::PrimaryWithTags => {
                f.write_str("Only empty tags are allowed with primary read preference")
            }
            ReadPrefError::PrimaryWithStaleness => {
                f.write_str("maxStalenessSeconds can not be set for the primary mode")
            }
        }
    }
}

impl ReadPreference {
    /// The preference of `mode`, `maxStalenessSeconds` (0: no limit), `tags` and whether it asks
    /// for hedged reads (`hedge.enabled`, true when `hedge` is given without it). TeselaDB reads one
    /// replica at a time: `hedge` is only checked (DV-021).
    pub fn new(
        mode: &str,
        max_staleness_s: Option<f64>,
        tag_sets: Vec<TagSet>,
        hedge: bool,
    ) -> Result<ReadPreference, ReadPrefError> {
        let mode = match mode {
            "primary" => Mode::Primary,
            "primaryPreferred" => Mode::PrimaryPreferred,
            "secondary" => Mode::Secondary,
            "secondaryPreferred" => Mode::SecondaryPreferred,
            "nearest" => Mode::Nearest,
            other => return Err(ReadPrefError::UnknownMode(other.to_owned())),
        };
        let max_staleness_ms = match max_staleness_s {
            Some(s) if s < 0.0 => return Err(ReadPrefError::StalenessNegative),
            Some(s) if s > i64::MAX as f64 => return Err(ReadPrefError::StalenessTooLarge),
            None => None,
            Some(0.0) => None,
            Some(s) if s < MIN_MAX_STALENESS_S as f64 => {
                return Err(ReadPrefError::StalenessTooSmall);
            }
            Some(s) => Some((s * 1000.0) as u64),
        };
        if mode == Mode::Primary {
            if hedge {
                return Err(ReadPrefError::PrimaryWithHedge);
            }
            if tag_sets.iter().any(|set| set.as_ref().is_none_or(|pairs| !pairs.is_empty())) {
                return Err(ReadPrefError::PrimaryWithTags);
            }
            if max_staleness_ms.is_some() {
                return Err(ReadPrefError::PrimaryWithStaleness);
            }
        }
        Ok(ReadPreference { mode, max_staleness_ms, tag_sets })
    }
}

impl ReadPreference {
    /// The preference as a reference router 9.0.4 writes it in `FailedToSatisfyReadPreference`
    /// (measured): its mode, its tag sets if any and its `maxStalenessSeconds` in whole seconds if
    /// any.
    pub fn describe(&self) -> String {
        let mode = match self.mode {
            Mode::Primary => "primary",
            Mode::PrimaryPreferred => "primaryPreferred",
            Mode::Secondary => "secondary",
            Mode::SecondaryPreferred => "secondaryPreferred",
            Mode::Nearest => "nearest",
        };
        let mut out = format!("{{ mode: \"{mode}\"");
        if !self.tag_sets.is_empty() {
            let sets: Vec<String> = self
                .tag_sets
                .iter()
                .map(|set| {
                    let pairs: Vec<String> = set
                        .iter()
                        .flatten()
                        .map(|(name, value)| format!("{name}: \"{value}\""))
                        .collect();
                    if pairs.is_empty() {
                        "{}".to_owned()
                    } else {
                        format!("{{ {} }}", pairs.join(", "))
                    }
                })
                .collect();
            out.push_str(&format!(", tags: [ {} ]", sets.join(", ")));
        }
        if let Some(ms) = self.max_staleness_ms {
            out.push_str(&format!(", maxStalenessSeconds: {}", ms / 1000));
        }
        out.push_str(" }");
        out
    }
}

/// A replica of the range a read goes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replica {
    /// Its node.
    pub node: NodeId,
    /// Whether it leads the range, as far as the router knows.
    pub leader: bool,
    /// Its node's round trip from here, in milliseconds.
    pub latency_ms: u64,
    /// Its node's tags.
    pub tags: Vec<(String, String)>,
}

/// The replicas a read with `pref` tries, in order.
pub fn candidates(pref: &ReadPreference, replicas: &[Replica]) -> Vec<NodeId> {
    let leader = replicas.iter().filter(|r| r.leader).map(|r| r.node);
    let followers: Vec<&Replica> = replicas.iter().filter(|r| !r.leader).collect();
    match pref.mode {
        Mode::Primary => leader.collect(),
        Mode::PrimaryPreferred => leader.chain(nearest(pref, &followers)).collect(),
        Mode::Secondary => nearest(pref, &followers),
        Mode::SecondaryPreferred => nearest(pref, &followers).into_iter().chain(leader).collect(),
        Mode::Nearest => nearest(pref, &replicas.iter().collect::<Vec<_>>()),
    }
}

/// The replicas of `pool` that match the first tag set matching any (all without tag sets),
/// nearest first.
fn nearest(pref: &ReadPreference, pool: &[&Replica]) -> Vec<NodeId> {
    let matches = |r: &Replica, set: &TagSet| {
        set.as_ref().is_some_and(|pairs| pairs.iter().all(|t| r.tags.contains(t)))
    };
    let mut chosen: Vec<&Replica> = match pref.tag_sets.as_slice() {
        [] => pool.to_vec(),
        sets => sets
            .iter()
            .map(|set| pool.iter().copied().filter(|r| matches(r, set)).collect::<Vec<_>>())
            .find(|m| !m.is_empty())
            .unwrap_or_default(),
    };
    chosen.sort_by_key(|r| (r.latency_ms, r.node));
    chosen.into_iter().map(|r| r.node).collect()
}
