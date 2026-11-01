//! The time zone of the node's process (007 FR-017): the one the balancer reads the window of
//! `config.settings` in, as a config server reads it in its own local time (measured on 9.0.4
//! with `TZ=Asia/Tokyo`: `activeWindow` from `22:00` to `23:00` balanced at 22:23 in Tokyo,
//! 13:23 UTC, and `13:00` to `14:00` did not). The zone is found as the C library finds it:
//! `TZ` when it is set (a name, with or without a leading `:`), else the zone `/etc/localtime`
//! links to, else the name in `/etc/timezone`. UTC when none names a zone of the database.

use teseladb_tz::Zone;

/// What the path of a zone's file has before its name.
const ZONEINFO: &str = "zoneinfo/";
/// The folder of the zones without leap seconds, a copy of the main one.
const POSIX: &str = "posix/";

/// The zone of this process.
pub fn local_zone() -> Option<Zone> {
    let tz = std::env::var("TZ").ok();
    let link = std::fs::read_link("/etc/localtime").ok();
    let link = link.as_ref().map(|p| p.to_string_lossy());
    let file = std::fs::read_to_string("/etc/timezone").ok();
    zone_of(tz.as_deref(), link.as_deref(), file.as_deref())
}

/// Seconds east of UTC of `zone` (UTC when `None`) at `ms` milliseconds since the epoch.
pub fn offset_s(zone: Option<Zone>, ms: u64) -> i32 {
    let secs = i64::try_from(ms / 1_000).unwrap_or(i64::MAX);
    zone.map_or(0, |z| z.at(secs).offset)
}

/// The zone of `TZ`, the target of `/etc/localtime` and the text of `/etc/timezone`.
pub fn zone_of(tz: Option<&str>, link: Option<&str>, file: Option<&str>) -> Option<Zone> {
    if let Some(tz) = tz {
        return Zone::named(tz.strip_prefix(':').unwrap_or(tz));
    }
    let linked = link.and_then(|l| {
        let (_, rest) = l.rsplit_once(ZONEINFO)?;
        Some(rest.strip_prefix(POSIX).unwrap_or(rest))
    });
    match linked {
        Some(name) => Zone::named(name),
        None => Zone::named(file?.trim()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(z: Option<Zone>) -> Option<&'static str> {
        z.map(|z| z.name())
    }

    #[test]
    fn tz_decides_when_it_is_set() {
        let link = Some("/usr/share/zoneinfo/Europe/Madrid");
        assert_eq!(name(zone_of(Some("Asia/Tokyo"), link, None)), Some("Asia/Tokyo"));
        assert_eq!(name(zone_of(Some(":Asia/Tokyo"), link, None)), Some("Asia/Tokyo"));
        // Set to what is not a zone: UTC, as the C library reads it.
        assert_eq!(name(zone_of(Some(""), link, None)), None);
        assert_eq!(name(zone_of(Some("Nowhere/Town"), link, None)), None);
    }

    #[test]
    fn without_tz_the_link_then_the_file_name_the_zone() {
        let madrid = Some("/usr/share/zoneinfo/Europe/Madrid");
        assert_eq!(name(zone_of(None, madrid, Some("Asia/Tokyo\n"))), Some("Europe/Madrid"));
        let mac = Some("/var/db/timezone/zoneinfo/America/New_York");
        assert_eq!(name(zone_of(None, mac, None)), Some("America/New_York"));
        let posix = Some("../usr/share/zoneinfo/posix/Asia/Kolkata");
        assert_eq!(name(zone_of(None, posix, None)), Some("Asia/Kolkata"));
        assert_eq!(
            name(zone_of(None, Some("/etc/elsewhere"), Some("Asia/Tokyo\n"))),
            Some("Asia/Tokyo")
        );
        assert_eq!(name(zone_of(None, None, Some(" Asia/Tokyo \n"))), Some("Asia/Tokyo"));
        assert_eq!(name(zone_of(None, None, None)), None);
        assert_eq!(name(zone_of(None, Some("/usr/share/zoneinfo/Nowhere/Town"), None)), None);
    }

    #[test]
    fn the_offset_is_the_zones_at_the_instant() {
        let at = 1_793_539_398_000; // 1 November 2026, 13:23 UTC.
        assert_eq!(offset_s(Zone::named("Asia/Tokyo"), at), 9 * 3600);
        assert_eq!(offset_s(Zone::named("Europe/Madrid"), at), 3600);
        assert_eq!(offset_s(Zone::named("Europe/Madrid"), at - 10 * 86_400_000), 2 * 3600);
        assert_eq!(offset_s(None, at), 0);
    }

    #[test]
    fn the_process_has_a_zone_or_utc() {
        // Whatever this machine says, it reads without failing.
        let z = local_zone();
        assert!(z.is_none_or(|z| Zone::named(z.name()).is_some()));
    }
}
