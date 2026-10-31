//! `Time` of RFC 5280 (4.1.2.5): UTCTime for 1950–2049 and GeneralizedTime otherwise, both in
//! UTC with seconds and no fractions, as seconds since the Unix epoch.

use crate::Error;
use crate::der::{self, tag};

const SECS_PER_DAY: i64 = 86_400;

/// Days since 1970-01-01 of a proleptic Gregorian date (the civil-days algorithm of Howard
/// Hinnant, «chrono-Compatible Low-Level Date Algorithms»).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
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

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        2 if is_leap(y) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn digits(s: &[u8]) -> Result<i64, Error> {
    s.iter().try_fold(0i64, |acc, &c| {
        if c.is_ascii_digit() { Ok(acc * 10 + i64::from(c - b'0')) } else { Err(Error::Der) }
    })
}

/// Seconds since the epoch of a UTCTime or GeneralizedTime element.
pub fn parse(tlv_tag: u8, value: &[u8]) -> Result<i64, Error> {
    let (year, rest) = match (tlv_tag, value.len()) {
        (tag::UTC_TIME, 13) => {
            let yy = digits(&value[..2])?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &value[2..])
        }
        (tag::GENERALIZED_TIME, 15) => (digits(&value[..4])?, &value[4..]),
        _ => return Err(Error::Der),
    };
    if rest[10] != b'Z' {
        return Err(Error::Der);
    }
    let month = digits(&rest[0..2])?;
    let day = digits(&rest[2..4])?;
    let hour = digits(&rest[4..6])?;
    let minute = digits(&rest[6..8])?;
    let second = digits(&rest[8..10])?;
    if !(1..=12).contains(&month)
        || day < 1
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(Error::Der);
    }
    Ok(days_from_civil(year, month, day) * SECS_PER_DAY + hour * 3600 + minute * 60 + second)
}

/// Reads one `Time` from `r`.
pub fn read(r: &mut der::Reader<'_>) -> Result<i64, Error> {
    let tlv = r.read_any()?;
    parse(tlv.tag, tlv.value)
}

/// 9999-12-31T23:59:59Z, the latest time GeneralizedTime can write (RFC 5280, 4.1.2.5: the
/// value for a certificate without a well-defined expiration).
pub const MAX_TIME: i64 = 253_402_300_799;
/// 0000-01-01T00:00:00Z, the earliest time GeneralizedTime can write.
pub const MIN_TIME: i64 = -62_167_219_200;

/// Encodes `secs` as RFC 5280 requires: UTCTime through 2049, GeneralizedTime from 2050. Times
/// outside the years 0000 to 9999 are clamped to them.
pub fn encode(secs: i64) -> Vec<u8> {
    let secs = secs.clamp(MIN_TIME, MAX_TIME);
    let days = secs.div_euclid(SECS_PER_DAY);
    let rem = secs.rem_euclid(SECS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let hms = format!("{:02}{:02}{:02}{:02}{:02}Z", m, d, rem / 3600, rem % 3600 / 60, rem % 60);
    if (1950..2050).contains(&y) {
        der::tlv(tag::UTC_TIME, format!("{:02}{hms}", y % 100).as_bytes())
    } else {
        der::tlv(tag::GENERALIZED_TIME, format!("{y:04}{hms}").as_bytes())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn utc_and_generalized_times() {
        assert_eq!(parse(tag::UTC_TIME, b"700101000000Z"), Ok(0));
        assert_eq!(parse(tag::UTC_TIME, b"491231235959Z"), Ok(2_524_607_999));
        assert_eq!(parse(tag::UTC_TIME, b"500101000000Z"), Ok(-631_152_000));
        assert_eq!(parse(tag::GENERALIZED_TIME, b"20500101000000Z"), Ok(2_524_608_000));
        assert_eq!(parse(tag::UTC_TIME, b"000229120000Z"), Ok(951_825_600));
        for bad in [
            &b"010229000000Z"[..],
            b"701301000000Z",
            b"700100000000Z",
            b"700101240000Z",
            b"700101006000Z",
            b"700101000060Z",
            b"700101000000+",
            b"7001010000000",
            b"70010100000Z",
            b"7a0101000000Z",
        ] {
            assert_eq!(parse(tag::UTC_TIME, bad), Err(Error::Der));
        }
        assert_eq!(parse(tag::OCTET_STRING, b"700101000000Z"), Err(Error::Der));
        assert_eq!(parse(tag::GENERALIZED_TIME, b"19000229000000Z"), Err(Error::Der));
        assert_eq!(parse(tag::GENERALIZED_TIME, b"20000229000000Z"), Ok(951_782_400));
        assert_eq!(parse(tag::UTC_TIME, b"240229000000Z"), Ok(1_709_164_800));
        for t in [0i64, 2_524_607_999, 2_524_608_000, -631_152_000, -631_152_001, 4_102_444_800] {
            let enc = encode(t);
            assert_eq!(read(&mut der::Reader::new(&enc)), Ok(t));
        }
        assert_eq!(encode(-631_152_001)[0], tag::GENERALIZED_TIME);
        assert_eq!(read(&mut der::Reader::new(&encode(i64::MAX))), Ok(MAX_TIME));
        assert_eq!(read(&mut der::Reader::new(&encode(i64::MIN))), Ok(MIN_TIME));
    }
}
