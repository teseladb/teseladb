//! POSIX TZ rules (RFC 8536, section 3.3) in every form the IANA footers use (010 T006).

use teseladb_tz::Rule;

/// Seconds since the epoch of `y-m-d h:00:00Z`.
fn at(y: i64, m: i64, d: i64, h: i64) -> i64 {
    let yy = if m <= 2 { y - 1 } else { y };
    let era = yy.div_euclid(400);
    let yoe = yy - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    (era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468) * 86_400 + h * 3600
}

#[test]
fn rules_without_daylight_saving_are_one_offset() {
    let utc = Rule::parse("UTC0").expect("a rule");
    assert_eq!(utc.at(0).offset, 0);
    assert_eq!(utc.at(0).abbr, "UTC");
    let tehran = Rule::parse("<+0330>-3:30").expect("a rule");
    assert_eq!(tehran.at(at(2030, 7, 1, 0)).offset, 3 * 3600 + 1800);
    assert_eq!(tehran.at(0).abbr, "+0330");
    let odd = Rule::parse("<-0130>1:30:15").expect("a rule");
    assert_eq!(odd.at(0).offset, -(5400 + 15));
}

#[test]
fn month_week_day_rules_change_at_their_local_times() {
    let ny = Rule::parse("EST5EDT,M3.2.0,M11.1.0").expect("a rule");
    // 2030-03-10 02:00 EST is 07:00Z.
    assert!(!ny.at(at(2030, 3, 10, 7) - 1).dst);
    assert!(ny.at(at(2030, 3, 10, 7)).dst);
    assert_eq!(ny.at(at(2030, 7, 1, 0)).offset, -4 * 3600);
    // 2030-11-03 02:00 EDT is 06:00Z.
    assert!(ny.at(at(2030, 11, 3, 6) - 1).dst);
    assert!(!ny.at(at(2030, 11, 3, 6)).dst);
    // Israel: the Friday before the last Sunday of March at 02:00 (26:00 of Thursday).
    let israel = Rule::parse("IST-2IDT,M3.4.4/26,M10.5.0").expect("a rule");
    assert_eq!(israel.at(at(2030, 7, 1, 0)).offset, 3 * 3600);
    assert_eq!(israel.at(at(2030, 1, 1, 0)).offset, 2 * 3600);
    // Greenland: negative transition times.
    let nuuk = Rule::parse("<-02>2<-01>,M3.5.0/-1,M10.5.0/0").expect("a rule");
    assert_eq!(nuuk.at(at(2030, 7, 1, 0)).offset, -3600);
    assert_eq!(nuuk.at(at(2030, 12, 1, 0)).offset, -7200);
}

#[test]
fn southern_and_julian_rules_wrap_the_year() {
    let santiago = Rule::parse("<-04>4<-03>,M9.1.6/24,M4.1.6/24").expect("a rule");
    assert_eq!(santiago.at(at(2100, 1, 1, 12)).offset, -3 * 3600);
    assert_eq!(santiago.at(at(2100, 7, 1, 12)).offset, -4 * 3600);
    // `Jn` never counts February 29; `n` does.
    let julian = Rule::parse("AAA3BBB,J60,J300").expect("a rule");
    assert!(julian.at(at(2032, 3, 2, 12)).dst);
    assert!(!julian.at(at(2032, 2, 28, 12)).dst);
    let zero = Rule::parse("AAA3BBB,59,299").expect("a rule");
    assert!(zero.at(at(2033, 3, 2, 12)).dst);
    // The daylight saving offset defaults to one hour ahead.
    assert_eq!(julian.at(at(2032, 7, 1, 0)).offset, -2 * 3600);
}

#[test]
fn malformed_rules_are_none() {
    for bad in [
        "",
        "AB",
        "<+03",
        "UTC",
        "EST5EDT",
        "EST5EDT,M3.2.0",
        "EST5EDT,M3.2.0;M11.1.0",
        "EST5EDT,Mx",
    ] {
        assert_eq!(Rule::parse(bad), None, "{bad}");
    }
}

#[test]
fn leap_years_follow_the_gregorian_rule() {
    // J60 is March 1 in every year; day 59 is February 29 only in leap years (2000, not 2100).
    let julian = Rule::parse("AAA3BBB,J60,J300").expect("a rule");
    assert!(julian.at(at(2000, 3, 1, 12)).dst);
    assert!(julian.at(at(2100, 3, 1, 12)).dst);
    assert!(!julian.at(at(2100, 2, 28, 12)).dst);
    let zero = Rule::parse("AAA3BBB,59,299").expect("a rule");
    assert!(zero.at(at(2000, 2, 29, 12)).dst);
    assert!(!zero.at(at(2100, 2, 28, 12)).dst);
    // The last Sunday of February in leap and common years.
    let feb = Rule::parse("AAA3BBB,M2.5.0,M10.5.0").expect("a rule");
    assert!(feb.at(at(2032, 2, 29, 12)).dst);
    assert!(!feb.at(at(2032, 2, 28, 12)).dst);
    assert!(feb.at(at(2100, 2, 28, 12)).dst);
    assert!(!feb.at(at(2100, 2, 27, 12)).dst);
    // After the end of the period in a northern rule.
    assert!(!feb.at(at(2100, 12, 1, 12)).dst);
}

static NO_FOOTER: teseladb_tz::ZoneData = teseladb_tz::ZoneData {
    transitions: &[0],
    indices: &[1],
    types: &[
        teseladb_tz::LocalType { offset: 3600, dst: false, abbr: "AAA" },
        teseladb_tz::LocalType { offset: 7200, dst: false, abbr: "BBB" },
    ],
    footer: "",
};

#[test]
fn a_zone_without_a_rule_keeps_its_last_type() {
    let z = teseladb_tz::Zone::from_data("Test/NoFooter", &NO_FOOTER);
    assert_eq!(z.name(), "Test/NoFooter");
    assert_eq!(z.at(-1).offset, 3600);
    assert_eq!(z.at(10_000_000_000).offset, 7200);
}
