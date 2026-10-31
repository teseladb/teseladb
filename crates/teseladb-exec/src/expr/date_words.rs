//! The words of date text without a format, as RefDb's timelib knows them (measured on a
//! `refrouter` 9.0.4 on 31-10-2026): zone abbreviations with their offsets, month and weekday names,
//! Roman months, ordinals and the units of relative dates.

/// Every zone abbreviation timelib accepts, lower-case, with its offset in seconds: the 25
/// military letters (no `j`) and the abbreviations of its table, which keeps the first offset an
/// abbreviation had (`bmt` is Bogotá's −3:58:29 of 1884). Measured one by one, from every word of
/// one to three letters and every abbreviation of the IANA database.
pub(super) const ABBREVIATIONS: [(&str, i32); 140] = [
    ("a", 3600),
    ("acdt", 37800),
    ("acst", 34200),
    ("addt", -7200),
    ("adt", -10800),
    ("aedt", 39600),
    ("aest", 36000),
    ("ahdt", -32400),
    ("ahst", -36000),
    ("akdt", -28800),
    ("akst", -32400),
    ("amt", -13840),
    ("apt", -10800),
    ("ast", -14400),
    ("awdt", 32400),
    ("awst", 28800),
    ("awt", -10800),
    ("b", 7200),
    ("bdst", 7200),
    ("bdt", -36000),
    ("bmt", -14309),
    ("bst", 3600),
    ("c", 10800),
    ("cast", 34200),
    ("cat", 7200),
    ("cdt", -18000),
    ("cemt", 10800),
    ("cest", 7200),
    ("cet", 3600),
    ("chst", 36000),
    ("cmt", -15408),
    ("cpt", -18000),
    ("cst", -21600),
    ("cwt", -18000),
    ("d", 14400),
    ("dmt", -1521),
    ("e", 18000),
    ("eat", 10800),
    ("edt", -14400),
    ("eest", 10800),
    ("eet", 7200),
    ("emt", -26248),
    ("ept", -14400),
    ("est", -18000),
    ("ewt", -14400),
    ("f", 21600),
    ("ffmt", -14660),
    ("fmt", -4056),
    ("g", 25200),
    ("gdt", 39600),
    ("gmt", 0),
    ("gst", 36000),
    ("h", 28800),
    ("hdt", -34200),
    ("hkst", 32400),
    ("hkt", 28800),
    ("hmt", -19776),
    ("hpt", -34200),
    ("hst", -36000),
    ("hwt", -34200),
    ("i", 32400),
    ("iddt", 14400),
    ("idt", 10800),
    ("imt", 25025),
    ("ist", 7200),
    ("jdt", 36000),
    ("jmt", 8440),
    ("jst", 32400),
    ("k", 36000),
    ("kdt", 36000),
    ("kmt", 5736),
    ("kst", 30600),
    ("l", 39600),
    ("lst", 9394),
    ("m", 43200),
    ("mdst", 16279),
    ("mdt", -21600),
    ("mest", 7200),
    ("met", 3600),
    ("mmt", 9017),
    ("mpt", -21600),
    ("msd", 14400),
    ("msk", 10800),
    ("mst", -25200),
    ("mwt", -21600),
    ("n", -3600),
    ("nddt", -5400),
    ("ndt", -9052),
    ("npt", -9000),
    ("nst", -12600),
    ("nwt", -9000),
    ("nzdt", 46800),
    ("nzmt", 41400),
    ("nzst", 43200),
    ("o", -7200),
    ("p", -10800),
    ("pdt", -25200),
    ("pkst", 21600),
    ("pkt", 18000),
    ("plmt", 25590),
    ("pmt", -13236),
    ("ppmt", -17340),
    ("ppt", -25200),
    ("pst", -28800),
    ("pwt", -25200),
    ("q", -14400),
    ("qmt", -18840),
    ("r", -18000),
    ("rmt", 5794),
    ("s", -21600),
    ("sast", 7200),
    ("sdmt", -16800),
    ("sjmt", -20173),
    ("smt", -13884),
    ("sst", -39600),
    ("t", -25200),
    ("tbmt", 10751),
    ("tmt", 12344),
    ("u", -28800),
    ("uct", 0),
    ("utc", 0),
    ("v", -32400),
    ("w", -36000),
    ("wast", 7200),
    ("wat", 3600),
    ("wemt", 7200),
    ("west", 3600),
    ("wet", 0),
    ("wib", 25200),
    ("wit", 32400),
    ("wita", 28800),
    ("wmt", 5040),
    ("x", -39600),
    ("y", -43200),
    ("yddt", -25200),
    ("ydt", -28800),
    ("ypt", -28800),
    ("yst", -32400),
    ("ywt", -28800),
    ("z", 0),
];

/// The months by name and three letters (`sept` too), for the month alone and in dates.
pub(super) const MONTH_NAMES: [(&str, i64); 24] = [
    ("january", 1),
    ("february", 2),
    ("march", 3),
    ("april", 4),
    ("may", 5),
    ("june", 6),
    ("july", 7),
    ("august", 8),
    ("september", 9),
    ("october", 10),
    ("november", 11),
    ("december", 12),
    ("jan", 1),
    ("feb", 2),
    ("mar", 3),
    ("apr", 4),
    ("jun", 6),
    ("jul", 7),
    ("aug", 8),
    ("sep", 9),
    ("sept", 9),
    ("oct", 10),
    ("nov", 11),
    ("dec", 12),
];

/// The months in upper-case Roman numerals, which only dates with a day or a year take
/// (`14 III 1879`; `3x` is no date).
pub(super) const ROMAN_MONTHS: [(&str, i64); 12] = [
    ("I", 1),
    ("II", 2),
    ("III", 3),
    ("IV", 4),
    ("V", 5),
    ("VI", 6),
    ("VII", 7),
    ("VIII", 8),
    ("IX", 9),
    ("X", 10),
    ("XI", 11),
    ("XII", 12),
];

/// The weekdays by name (with a plural `s`) and three letters, with their number (0 for
/// Sunday).
pub(super) const DAY_NAMES: [(&str, i64); 21] = [
    ("sunday", 0),
    ("monday", 1),
    ("tuesday", 2),
    ("wednesday", 3),
    ("thursday", 4),
    ("friday", 5),
    ("saturday", 6),
    ("sundays", 0),
    ("mondays", 1),
    ("tuesdays", 2),
    ("wednesdays", 3),
    ("thursdays", 4),
    ("fridays", 5),
    ("saturdays", 6),
    ("sun", 0),
    ("mon", 1),
    ("tue", 2),
    ("wed", 3),
    ("thu", 4),
    ("fri", 5),
    ("sat", 6),
];

/// Ordinals and the relative words, with their amount and whether they count the day itself
/// (`this tuesday` on a Tuesday is that day; `next tuesday` is a week later).
pub(super) const ORDINALS: [(&str, i64, bool); 16] = [
    ("first", 1, false),
    ("second", 2, false),
    ("third", 3, false),
    ("fourth", 4, false),
    ("fifth", 5, false),
    ("sixth", 6, false),
    ("seventh", 7, false),
    ("eighth", 8, false),
    ("ninth", 9, false),
    ("tenth", 10, false),
    ("eleventh", 11, false),
    ("twelfth", 12, false),
    ("next", 1, false),
    ("last", -1, false),
    ("previous", -1, false),
    ("this", 0, true),
];

/// The relative words that also make `<word> week` (`next week`).
pub(super) const RELATIVE_TEXT: [&str; 4] = ["next", "last", "previous", "this"];

/// What a unit of a relative date counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Unit {
    Micro,
    Milli,
    Second,
    Minute,
    Hour,
    Day,
    Week,
    Fortnight,
    Month,
    Year,
    Weekday,
    /// A weekday by name (`+1 monday`, `next monday`).
    Named(i64),
}

/// The units of relative dates. `week` alone is a unit after a number only (`5 week`), never
/// after a word (`second week`).
pub(super) const UNITS: [(&str, Unit); 17] = [
    ("ms", Unit::Milli),
    ("µs", Unit::Micro),
    ("msec", Unit::Milli),
    ("millisecond", Unit::Milli),
    ("µsec", Unit::Micro),
    ("microsecond", Unit::Micro),
    ("usec", Unit::Micro),
    ("sec", Unit::Second),
    ("second", Unit::Second),
    ("min", Unit::Minute),
    ("minute", Unit::Minute),
    ("hour", Unit::Hour),
    ("day", Unit::Day),
    ("fortnight", Unit::Fortnight),
    ("forthnight", Unit::Fortnight),
    ("month", Unit::Month),
    ("year", Unit::Year),
];

/// The plurals of the units that take an `s` (`ms` and `µs` take none).
pub(super) const PLURALS: [(&str, Unit); 15] = [
    ("msecs", Unit::Milli),
    ("milliseconds", Unit::Milli),
    ("µsecs", Unit::Micro),
    ("microseconds", Unit::Micro),
    ("usecs", Unit::Micro),
    ("secs", Unit::Second),
    ("seconds", Unit::Second),
    ("mins", Unit::Minute),
    ("minutes", Unit::Minute),
    ("hours", Unit::Hour),
    ("days", Unit::Day),
    ("fortnights", Unit::Fortnight),
    ("forthnights", Unit::Fortnight),
    ("months", Unit::Month),
    ("years", Unit::Year),
];

/// Whole words of the units (`weeks`, `weekday`, `weekdays`) beside [`UNITS`].
pub(super) const UNIT_WORDS: [(&str, Unit); 3] =
    [("weeks", Unit::Week), ("weekday", Unit::Weekday), ("weekdays", Unit::Weekday)];

/// The value of `word` in `table`, ignoring ASCII case.
pub(super) fn lookup<T: Copy>(table: &[(&str, T)], word: &str) -> Option<T> {
    table.iter().find(|(w, _)| w.eq_ignore_ascii_case(word)).map(|(_, v)| *v)
}

/// The unit `word` names, after a number (`with_week`) or a word.
pub(super) fn unit_of(word: &str, with_week: bool) -> Option<Unit> {
    if with_week && word.eq_ignore_ascii_case("week") {
        return Some(Unit::Week);
    }
    if let Some(u) = lookup(&UNIT_WORDS, word) {
        return Some(u);
    }
    if let Some(d) = lookup(&DAY_NAMES, word) {
        return Some(Unit::Named(d));
    }
    lookup(&UNITS, word).or_else(|| lookup(&PLURALS, word))
}
