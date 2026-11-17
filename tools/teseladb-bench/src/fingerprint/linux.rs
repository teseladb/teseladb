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

/// The value of a `/proc/cpuinfo` field, without its colon.
fn cpu_field(info: &str, name: &str) -> Option<String> {
    field(info, name).map(|v| v.trim_start_matches(':').trim().to_owned()).filter(|v| !v.is_empty())
}

/// The CPU model: `model name` on x86, and on ARM, where `/proc/cpuinfo` has none, the
/// `Hardware` line or the implementer and part of the first core (MIDR fields).
fn cpu_model(info: &str) -> Option<String> {
    cpu_field(info, "model name").or_else(|| cpu_field(info, "Hardware")).or_else(|| {
        let implementer = cpu_field(info, "CPU implementer")?;
        let part = cpu_field(info, "CPU part")?;
        Some(format!("ARM implementer {implementer} part {part}"))
    })
}

pub(crate) fn probe() -> Probe {
    let cpu = cpu_model(&read("/proc/cpuinfo"));
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
        let x86 = "processor\t: 0\nmodel name\t: Some CPU @ 3 GHz\n";
        assert_eq!(cpu_model(x86).as_deref(), Some("Some CPU @ 3 GHz"));
        // An ARM core's cpuinfo has no model name.
        let arm =
            "processor\t: 0\nCPU implementer\t: 0x61\nCPU architecture: 8\nCPU part\t: 0x000\n";
        assert_eq!(cpu_model(arm).as_deref(), Some("ARM implementer 0x61 part 0x000"));
        assert_eq!(cpu_model("Hardware\t: Board X\n").as_deref(), Some("Board X"));
        assert_eq!(cpu_model("processor\t: 0\n"), None);
        let p = probe();
        assert!(p.memory_bytes > 0);
        assert!(load_average().is_some_and(|l| l >= 0.0));
    }
}
