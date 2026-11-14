//! What the node finds at start of the host settings a production data node needs (012
//! FR-036): Transparent Huge Pages in `madvise`, MGLRU on, `vm.swappiness` at 1, the data
//! directory on XFS mounted with `noatime` and at least 1,048,576 open files for the process.
//! The node writes one line with each value it found, and a warning naming each one that differs.
//! On a host without those files (macOS, a container without `/sys`) the value is unknown.

use std::path::Path;

use crate::log::{Level, Pending};

/// The THP mode a data node runs with.
pub const THP_MODE: &str = "madvise";
/// The swappiness a data node runs with.
pub const SWAPPINESS: u32 = 1;
/// The file system of a data node's data directory.
pub const FILE_SYSTEM: &str = "xfs";
/// The mount option of the data directory.
pub const NOATIME: &str = "noatime";
/// The fewest open files a data node's process may have.
pub const MIN_OPEN_FILES: u64 = 1_048_576;

const THP_FILE: &str = "/sys/kernel/mm/transparent_hugepage/enabled";
const MGLRU_FILE: &str = "/sys/kernel/mm/lru_gen/enabled";
const SWAPPINESS_FILE: &str = "/proc/sys/vm/swappiness";
const MOUNTS_FILE: &str = "/proc/self/mountinfo";

/// The settings the node found; `None` where it could not read one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostSettings {
    /// The THP mode, the one in brackets of `enabled` (`always`, `madvise` or `never`).
    pub thp: Option<String>,
    /// Whether MGLRU is on: any bit set in `lru_gen/enabled`.
    pub mglru: Option<bool>,
    /// `vm.swappiness`.
    pub swappiness: Option<u32>,
    /// The file system type of the mount that holds the data directory.
    pub file_system: Option<String>,
    /// Whether that mount has `noatime`.
    pub noatime: Option<bool>,
    /// The soft limit of open files of the process.
    pub open_files: Option<u64>,
}

impl HostSettings {
    /// What the host has for a node whose data directory is `data`.
    pub fn read(data: &Path) -> HostSettings {
        let text = |p: &str| std::fs::read_to_string(p).ok();
        let data = std::fs::canonicalize(data).unwrap_or_else(|_| data.to_path_buf());
        let mount = text(MOUNTS_FILE).and_then(|m| mount_of(&m, &data.to_string_lossy()));
        HostSettings {
            thp: text(THP_FILE).as_deref().and_then(thp_mode),
            mglru: text(MGLRU_FILE).as_deref().and_then(mglru_on),
            swappiness: text(SWAPPINESS_FILE).and_then(|s| s.trim().parse().ok()),
            file_system: mount.as_ref().map(|m| m.file_system.clone()),
            noatime: mount.as_ref().map(|m| m.options.iter().any(|o| o == NOATIME)),
            open_files: teseladb_sys::host::max_open_files().ok(),
        }
    }

    /// The line's attributes, as JSON: each setting, `null` where unknown.
    pub fn attr(&self) -> String {
        fn text(v: &Option<String>) -> String {
            v.as_ref().map_or_else(|| "null".to_owned(), |s| format!("{s:?}"))
        }
        fn shown<T: std::fmt::Display>(v: Option<T>) -> String {
            v.map_or_else(|| "null".to_owned(), |x| x.to_string())
        }
        format!(
            "{{\"thp\":{},\"mglru\":{},\"swappiness\":{},\"fileSystem\":{},\"noatime\":{},\"openFiles\":{}}}",
            text(&self.thp),
            shown(self.mglru),
            shown(self.swappiness),
            text(&self.file_system),
            shown(self.noatime),
            shown(self.open_files),
        )
    }

    /// The settings known to differ from those of a production data node; an unknown one is not
    /// among them.
    pub fn differences(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.thp.as_deref().is_some_and(|m| m != THP_MODE) {
            out.push("transparent huge pages are not in madvise");
        }
        if self.mglru == Some(false) {
            out.push("MGLRU is off");
        }
        if self.swappiness.is_some_and(|s| s != SWAPPINESS) {
            out.push("vm.swappiness is not 1");
        }
        if self.file_system.as_deref().is_some_and(|f| f != FILE_SYSTEM) {
            out.push("the data directory is not on XFS");
        }
        if self.noatime == Some(false) {
            out.push("the data directory is not mounted with noatime");
        }
        if self.open_files.is_some_and(|n| n < MIN_OPEN_FILES) {
            out.push("the process may open fewer than 1048576 files");
        }
        out
    }

    /// The lines the node writes at start: what it found, and a warning with what differs.
    pub fn lines(&self) -> Vec<(Level, Pending)> {
        let found = Pending { id: 0, msg: FOUND_MSG.to_owned(), attr: self.attr() };
        let mut out = vec![(Level::Info, found)];
        let differs = self.differences();
        if !differs.is_empty() {
            let msg = format!("{DIFFERS_MSG}: {}", differs.join("; "));
            out.push((Level::Warn, Pending { id: 0, msg, attr: self.attr() }));
        }
        out
    }

    /// Writes [`HostSettings::lines`] to the node's log.
    pub fn log(&self) {
        for (level, line) in self.lines() {
            crate::log::defer_at(level, line);
        }
    }
}

/// The message of the line with what the node found.
pub const FOUND_MSG: &str = "host settings of a data node";
/// The start of the warning that names what differs.
pub const DIFFERS_MSG: &str = "this host is not set up as a data node";

/// The mode in brackets of a THP `enabled` file (`always [madvise] never`).
pub fn thp_mode(text: &str) -> Option<String> {
    let start = text.find('[')? + 1;
    let end = start + text[start..].find(']')?;
    Some(text[start..end].to_owned())
}

/// Whether the `lru_gen/enabled` file (`0x0007`, or a decimal) says MGLRU is on.
pub fn mglru_on(text: &str) -> Option<bool> {
    let t = text.trim();
    let n = match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok()?,
        None => t.parse().ok()?,
    };
    Some(n != 0)
}

/// A mount of `/proc/self/mountinfo`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mount {
    /// Where it is mounted.
    pub point: String,
    /// Its file system type.
    pub file_system: String,
    /// Its mount options.
    pub options: Vec<String>,
}

/// The mount of `mountinfo` that holds `path`: the one with the longest mount point that is
/// `path` or one of its parents, the last of them when several are stacked there.
pub fn mount_of(mountinfo: &str, path: &str) -> Option<Mount> {
    let mut best: Option<Mount> = None;
    for line in mountinfo.lines() {
        let Some(m) = parse_mount(line) else { continue };
        let holds = path == m.point
            || m.point == "/"
            || path.strip_prefix(m.point.as_str()).is_some_and(|rest| rest.starts_with('/'));
        if holds && best.as_ref().is_none_or(|b| m.point.len() >= b.point.len()) {
            best = Some(m);
        }
    }
    best
}

/// One line of `mountinfo`: `id parent dev root point options [optional...] - type source super`.
fn parse_mount(line: &str) -> Option<Mount> {
    let (before, after) = line.split_once(" - ")?;
    let fields: Vec<&str> = before.split(' ').collect();
    let point = unescape(fields.get(4)?);
    let options = fields.get(5)?.split(',').map(str::to_owned).collect();
    let file_system = after.split(' ').next()?.to_owned();
    Some(Mount { point, file_system, options })
}

/// A field of `mountinfo` with its octal escapes (`\040` for a space) undone.
fn unescape(field: &str) -> String {
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let octal = b
            .get(i + 1..i + 4)
            .filter(|d| b[i] == b'\\' && d.iter().all(|c| (b'0'..=b'7').contains(c)));
        match octal {
            Some(d) => {
                out.push((d[0] - b'0') * 64 + (d[1] - b'0') * 8 + (d[2] - b'0'));
                i += 4;
            }
            None => {
                out.push(b[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 0:21 / / rw,relatime shared:1 - ext4 /dev/root rw
30 22 259:1 / /var/lib/teseladb rw,noatime shared:2 - xfs /dev/nvme1n1 rw,attr2,inode64
31 22 0:30 / /var/lib/tesela rw,relatime - tmpfs tmpfs rw
32 30 0:31 / /var/lib/teseladb/with\\040space rw,relatime - ext4 /dev/sdb rw";

    #[test]
    fn the_thp_mode_is_the_one_in_brackets() {
        assert_eq!(thp_mode("always [madvise] never\n").as_deref(), Some("madvise"));
        assert_eq!(thp_mode("[always] madvise never").as_deref(), Some("always"));
        assert_eq!(thp_mode("always madvise never"), None);
    }

    #[test]
    fn mglru_is_on_with_any_bit_set() {
        assert_eq!(mglru_on("0x0007\n"), Some(true));
        assert_eq!(mglru_on("0x0000"), Some(false));
        assert_eq!(mglru_on("5"), Some(true));
        assert_eq!(mglru_on("y"), None);
    }

    #[test]
    fn the_data_directory_is_on_its_longest_mount() {
        let m = mount_of(MOUNTINFO, "/var/lib/teseladb/core-00").unwrap();
        assert_eq!((m.point.as_str(), m.file_system.as_str()), ("/var/lib/teseladb", "xfs"));
        assert!(m.options.iter().any(|o| o == NOATIME));
        // A mount point that only shares a prefix of the name is not a parent.
        assert_eq!(mount_of(MOUNTINFO, "/var/lib/teselax").unwrap().point, "/");
        assert_eq!(mount_of(MOUNTINFO, "/var/lib/tesela").unwrap().file_system, "tmpfs");
        let spaced = mount_of(MOUNTINFO, "/var/lib/teseladb/with space/d").unwrap();
        assert_eq!(spaced.point, "/var/lib/teseladb/with space");
        assert_eq!(mount_of("garbage line\n", "/x"), None);
    }

    #[test]
    fn a_data_node_set_up_as_asked_has_no_differences() {
        let good = HostSettings {
            thp: Some(THP_MODE.into()),
            mglru: Some(true),
            swappiness: Some(SWAPPINESS),
            file_system: Some(FILE_SYSTEM.into()),
            noatime: Some(true),
            open_files: Some(MIN_OPEN_FILES),
        };
        assert!(good.differences().is_empty());
        assert_eq!(
            good.attr(),
            "{\"thp\":\"madvise\",\"mglru\":true,\"swappiness\":1,\"fileSystem\":\"xfs\",\"noatime\":true,\"openFiles\":1048576}"
        );
        // Unknown is not a difference.
        assert!(HostSettings::default().differences().is_empty());
        assert!(HostSettings::default().attr().contains("\"thp\":null"));
    }

    #[test]
    fn each_setting_that_differs_is_named() {
        let bad = HostSettings {
            thp: Some("always".into()),
            mglru: Some(false),
            swappiness: Some(60),
            file_system: Some("ext4".into()),
            noatime: Some(false),
            open_files: Some(1024),
        };
        assert_eq!(bad.differences().len(), 6, "{:?}", bad.differences());
    }

    #[test]
    fn the_node_writes_what_it_found_and_warns_of_what_differs() {
        let good = HostSettings { swappiness: Some(SWAPPINESS), ..HostSettings::default() };
        let lines = good.lines();
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].0, lines[0].1.msg.as_str()), (Level::Info, FOUND_MSG));
        assert_eq!(lines[0].1.attr, good.attr());
        let bad = HostSettings { swappiness: Some(60), noatime: Some(false), ..good };
        let lines = bad.lines();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[1].0, Level::Warn);
        assert_eq!(
            lines[1].1.msg,
            format!(
                "{DIFFERS_MSG}: vm.swappiness is not 1; the data directory is not mounted with noatime"
            )
        );
    }

    #[test]
    fn reading_the_host_finds_the_open_files() {
        let found = HostSettings::read(&std::env::temp_dir());
        assert!(found.open_files.is_some_and(|n| n > 0), "{found:?}");
    }
}

#[cfg(all(test, target_os = "linux"))]
#[cfg_attr(coverage, coverage(off))]
mod linux_tests {
    use super::*;

    #[test]
    fn a_linux_host_tells_its_settings() {
        // The kernel of the test containers has THP and swappiness; the data directory has a
        // mount and a file system.
        let found = HostSettings::read(&std::env::temp_dir());
        assert!(found.thp.is_some() && found.swappiness.is_some(), "{found:?}");
        assert!(found.file_system.is_some() && found.noatime.is_some(), "{found:?}");
    }
}
