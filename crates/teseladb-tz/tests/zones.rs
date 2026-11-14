//! Offsets of zones against those a `refrouter` 9.0.4 gave on 31-10-2026 through `$dateToString`
//! (010 FR-007, T006): the history of `backzone`, the changes of 2024a and not later ones, the
//! POSIX rules after the last transition, and daylight saving changes.

use teseladb_tz::Zone;

/// Seconds since the epoch of `y-m-d h:00:00Z`.
fn at(y: i64, m: i64, d: i64, h: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    days * 86_400 + h * 3600
}

fn offset(zone: &str, secs: i64) -> i32 {
    Zone::named(zone).unwrap_or_else(|| panic!("{zone}")).at(secs).offset
}

#[test]
fn the_database_is_2024a_with_backzone() {
    assert_eq!(teseladb_tz::version(), "2024a");
    assert_eq!(offset("Asia/Almaty", at(2025, 7, 1, 12)), 5 * 3600);
    assert_eq!(offset("America/Asuncion", at(2025, 7, 1, 12)), -4 * 3600);
    assert!(Zone::named("America/Coyhaique").is_none());
    assert_eq!(offset("America/Ciudad_Juarez", at(2025, 7, 1, 12)), -6 * 3600);
    assert_eq!(offset("Europe/Amsterdam", at(1935, 6, 1, 12)), 3600 + 19 * 60 + 32);
    assert_eq!(offset("Europe/Brussels", at(1935, 6, 1, 12)), 3600);
    assert_eq!(offset("Europe/Oslo", at(1960, 6, 1, 12)), 2 * 3600);
    assert_eq!(offset("Europe/Madrid", at(1900, 6, 1, 12)), -(14 * 60 + 44));
    assert_eq!(offset("America/New_York", at(1800, 6, 1, 12)), -(4 * 3600 + 56 * 60 + 2));
    assert_eq!(offset("Asia/Kuala_Lumpur", at(1960, 6, 1, 12)), 7 * 3600 + 1800);
    assert_eq!(offset("Asia/Tehran", at(2026, 1, 1, 12)), 3 * 3600 + 1800);
}

#[test]
fn names_are_case_sensitive_and_include_the_legacy_ones() {
    assert!(Zone::named("europe/madrid").is_none());
    assert!(Zone::named("utc").is_none());
    assert_eq!(offset("US/Eastern", at(2025, 7, 1, 12)), -4 * 3600);
    assert_eq!(offset("EST5EDT", at(2025, 7, 1, 12)), -4 * 3600);
    assert_eq!(offset("Europe/Kiev", at(2025, 7, 1, 12)), 3 * 3600);
    assert_eq!(offset("Factory", at(2025, 7, 1, 12)), 0);
    assert_eq!(Zone::named("UTC").map(|z| z.name()), Some("UTC"));
    assert!(teseladb_tz::names().count() > 590);
}

#[test]
fn rules_after_the_last_transition_follow_the_posix_footer() {
    assert_eq!(offset("Europe/Madrid", at(2050, 7, 1, 12)), 2 * 3600);
    assert_eq!(offset("Europe/Madrid", at(2050, 1, 1, 12)), 3600);
    assert_eq!(offset("America/Santiago", at(2100, 1, 1, 12)), -3 * 3600);
    assert_eq!(offset("America/Santiago", at(2100, 7, 1, 12)), -4 * 3600);
    assert_eq!(offset("America/Vancouver", at(2026, 12, 1, 12)), -8 * 3600);
    assert_eq!(offset("America/Vancouver", at(2026, 7, 1, 12)), -7 * 3600);
    assert_eq!(offset("Africa/Casablanca", at(2040, 7, 1, 12)), 3600);
    assert_eq!(offset("Africa/Casablanca", at(2087, 4, 1, 12)), 0);
    let z = Zone::named("Australia/Lord_Howe").expect("a zone");
    assert_eq!(z.at(at(2060, 1, 1, 0)).offset, 11 * 3600);
    assert_eq!(z.at(at(2060, 7, 1, 0)).offset, 10 * 3600 + 1800);
}

#[test]
fn daylight_saving_changes_happen_at_their_instant() {
    let madrid = Zone::named("Europe/Madrid").expect("a zone");
    // 2026-03-29 01:00Z: 02:00 CET becomes 03:00 CEST.
    let change = at(2026, 3, 29, 1);
    assert_eq!(madrid.at(change - 1).offset, 3600);
    assert_eq!(madrid.at(change - 1).abbr, "CET");
    assert_eq!(madrid.at(change).offset, 7200);
    assert!(madrid.at(change).dst);
    assert_eq!(madrid.at(change).abbr, "CEST");
    // 2026-10-25 01:00Z: 03:00 CEST becomes 02:00 CET.
    let back = at(2026, 10, 25, 1);
    assert_eq!(madrid.at(back - 1).offset, 7200);
    assert_eq!(madrid.at(back).offset, 3600);
    // Local 02:30 that night does not exist: read with the offset before (01:30Z, 03:30 local).
    assert_eq!(madrid.utc_of(at(2026, 3, 29, 2) + 1800), at(2026, 3, 29, 1) + 1800);
    // Local 02:30 in October happens twice: the later instant (01:30Z), as RefDb resolves it.
    assert_eq!(madrid.utc_of(at(2026, 10, 25, 2) + 1800), at(2026, 10, 25, 1) + 1800);
    // Local 01:30 and 03:30 that night happen once.
    assert_eq!(madrid.utc_of(at(2026, 10, 25, 1) + 1800), at(2026, 10, 24, 23) + 1800);
    assert_eq!(madrid.utc_of(at(2026, 10, 25, 3) + 1800), at(2026, 10, 25, 2) + 1800);
    assert_eq!(madrid.utc_of(at(2026, 3, 29, 3) + 1800), at(2026, 3, 29, 1) + 1800);
    // An ordinary local time.
    assert_eq!(madrid.utc_of(at(2026, 7, 1, 14)), at(2026, 7, 1, 12));
}
