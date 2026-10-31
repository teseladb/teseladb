//! The fingerprint's probe on Linux: `/proc`, `/sys/block` and `/etc/os-release`.

use super::Probe;

fn read(p: &str) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// The load average of the last minute (first field of `/proc/loadavg`).
pub(crate) fn load_average() -> Option<f64> {
    read("/proc/loadavg").split_whitespace().next()?.parse().ok()
}

/// The value after `prefix` in the first line of `text` that starts with it.
fn field(text: &str, prefix: &str) -> Option<String> {
    text.lines().find_map(|l| l.strip_prefix(prefix).map(|v| v.trim().to_owned()))
}

pub(crate) fn probe() -> Probe {
    let cpu = field(&read("/proc/cpuinfo"), "model name")
        .map(|v| v.trim_start_matches(':').trim().to_owned());
    let memory_kib: u64 = field(&read("/proc/meminfo"), "MemTotal:")
        .and_then(|v| v.trim_end_matches("kB").trim().parse().ok())
        .unwrap_or(0);
    let os =
        field(&read("/etc/os-release"), "PRETTY_NAME=").map(|v| v.trim_matches('"').to_owned());
    Probe {
        cpu: cpu.unwrap_or_default(),
        memory_bytes: memory_kib * 1024,
        disk: disks(),
        os: format!("{} ({})", os.unwrap_or_default(), read("/proc/sys/kernel/osrelease").trim()),
    }
}

/// Models of the block devices that are disks (not loop, RAM, zram or device-mapper devices),
/// in name order, joined.
fn disks() -> String {
    let mut models: Vec<String> = std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            !["loop", "ram", "dm-", "zram"].iter().any(|p| name.starts_with(p))
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("device/model")).ok())
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty())
        .collect();
    models.sort();
    models.join(", ")
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn fields_are_read_from_their_line() {
        assert_eq!(field("a: 1\nmodel name\t: X\n", "model name"), Some(": X".into()));
        assert_eq!(field("", "x"), None);
        let p = probe();
        assert!(p.memory_bytes > 0);
        assert!(load_average().is_some_and(|l| l >= 0.0));
    }
}
