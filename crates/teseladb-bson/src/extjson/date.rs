//! Dates of relaxed Extended JSON: milliseconds since the epoch as ISO-8601 in UTC for the years
//! 1970 to 9999, and back, with an optional fraction and a `Z` or `±HH:MM` offset. Days and civil
//! dates convert with Howard Hinnant's algorithms.

const DAY_MS: i64 = 86_400_000;
/// 10000-01-01T00:00:00Z.
const YEAR_10000_MS: i64 = 253_402_300_800_000;

/// `ms` as `YYYY-MM-DDTHH:MM:SS[.mmm]Z`, the fraction only if not zero; `None` outside the years
/// 1970 to 9999.
#[expect(clippy::many_single_char_names, reason = "y, m, d, h and s are the fields of the date")]
pub(crate) fn to_iso(ms: i64) -> Option<String> {
    if !(0..YEAR_10000_MS).contains(&ms) {
        return None;
    }
    let (y, m, d) = civil_from_days(ms.div_euclid(DAY_MS));
    let rem = ms.rem_euclid(DAY_MS);
    let (h, mi, s, milli) = (rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000);
    let base = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    Some(if milli == 0 { format!("{base}Z") } else { format!("{base}.{milli:03}Z") })
}

/// `ms` as the reference server writes dates in its diagnostics (`currentOp`, `getLog`):
/// `YYYY-MM-DDTHH:MM:SS.mmm+00:00`, always with the milliseconds; `None` outside the years 1970
/// to 9999.
#[must_use]
pub fn to_log_time(ms: i64) -> Option<String> {
    let iso = to_iso(ms)?;
    let base = &iso[..19];
    Some(format!("{base}.{:03}+00:00", ms.rem_euclid(1000)))
}

/// The milliseconds of an ISO-8601 date with time and offset.
#[expect(clippy::many_single_char_names, reason = "y, m, d, h and s are the fields of the date")]
pub(crate) fn from_iso(t: &str) -> Option<i64> {
    let b = t.as_bytes();
    let num = |from: usize, len: usize| -> Option<i64> {
        let s = t.get(from..from + len)?;
        s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok())?
    };
    if b.len() < 20
        || b[4] != b'-'
        || b[7] != b'-'
        || b[10] != b'T'
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let (y, mo, d, h, mi, s) =
        (num(0, 4)?, num(5, 2)?, num(8, 2)?, num(11, 2)?, num(14, 2)?, num(17, 2)?);
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let mut pos = 19;
    let mut milli = 0;
    if b[pos] == b'.' {
        let digits = b[pos + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if digits == 0 {
            return None;
        }
        let frac = &t[pos + 1..pos + 1 + digits.min(3)];
        milli =
            frac.parse::<i64>().ok()? * 10i64.pow(u32::try_from(3 - digits.min(3)).unwrap_or(0));
        pos += 1 + digits;
    }
    let offset_min = match &t[pos..] {
        "Z" => 0,
        zone => {
            let sign = match zone.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let (zh, zm) = match zone.len() {
                6 if zone.as_bytes()[3] == b':' => (num(pos + 1, 2)?, num(pos + 4, 2)?),
                5 => (num(pos + 1, 2)?, num(pos + 3, 2)?),
                _ => return None,
            };
            if zh > 23 || zm > 59 {
                return None;
            }
            sign * (zh * 60 + zm)
        }
    };
    let days = days_from_civil(y, mo, d);
    Some(days * DAY_MS + h * 3_600_000 + mi * 60_000 + s * 1000 + milli - offset_min * 60_000)
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date of a day since 1970-01-01.
#[must_use]
pub fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn log_times_keep_the_milliseconds() {
        assert_eq!(to_log_time(0).as_deref(), Some("1970-01-01T00:00:00.000+00:00"));
        assert_eq!(
            to_log_time(1_793_363_006_383).as_deref(),
            Some("2026-10-30T12:23:26.383+00:00")
        );
        assert_eq!(to_log_time(-1), None);
    }

    #[test]
    fn dates_go_both_ways() {
        for (ms, iso) in [
            (0, "1970-01-01T00:00:00Z"),
            (1_356_351_330_501, "2012-12-24T12:15:30.501Z"),
            (951_782_400_000, "2000-02-29T00:00:00Z"),
            (YEAR_10000_MS - 1, "9999-12-31T23:59:59.999Z"),
        ] {
            assert_eq!(to_iso(ms).as_deref(), Some(iso));
            assert_eq!(from_iso(iso), Some(ms));
        }
        assert_eq!(to_iso(-1), None);
        assert_eq!(to_iso(YEAR_10000_MS), None);
        // Offsets and fractions of any length.
        assert_eq!(from_iso("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(from_iso("1970-01-01T00:00:00-0130"), Some(5_400_000));
        assert_eq!(from_iso("1970-01-01T00:00:00.5Z"), Some(500));
        assert_eq!(from_iso("2012-02-29T00:00:00Z"), Some(1_330_473_600_000));
        assert_eq!(from_iso("1970-01-01T00:00:00+0100"), Some(-3_600_000));
        assert_eq!(from_iso("1970-01-01T00:00:00.123456Z"), Some(123));
        for bad in [
            "1970-01-01",
            "1970-13-01T00:00:00Z",
            "2001-02-29T00:00:00Z",
            "1970-01-01T24:00:00Z",
            "1970-01-01T00:00:00",
            "1970-01-01T00:00:00.Z",
            "1970-01-01T00:00:00+1",
            "1970-01-01T00:00:00*01:00",
            "1970-01-01T00:00:00+25:00",
            "1970-01-01T00:00:00+01:60",
            "1970-01-01T00:00:00+01-00",
            "1970-01x01T00:00:00Z",
            "1970-01-01x00:00:00Z",
            "1970-01-01T00x00:00Z",
            "1970-01-01T00:00x00Z",
            "1970-01-00T00:00:00Z",
            "1970-01-01T00:60:00Z",
            "1970-01-01T00:00:60Z",
            "1970-01-01T00:00:00+0a:00",
            "1970-0a-01T00:00:00Z",
            "197a-01-01T00:00:00Z",
            "1970/01/01T00:00:00Z",
        ] {
            assert_eq!(from_iso(bad), None, "{bad}");
        }
    }
}
