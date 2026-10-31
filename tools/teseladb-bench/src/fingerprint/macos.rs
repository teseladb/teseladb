//! The fingerprint's probe on macOS: `sysctl`, `diskutil` and `sw_vers`.

use std::process::Command;

use super::Probe;

/// Disk whose model counts: the internal one the system starts from (`/` is an APFS volume of a
/// synthesized disk, which has no model).
const BOOT_DISK: &str = "disk0";

pub(crate) fn probe() -> Probe {
    let sysctl = |key: &str| output("sysctl", &["-n", key]);
    let disk = output("diskutil", &["info", BOOT_DISK])
        .lines()
        .find_map(|l| l.trim().strip_prefix("Device / Media Name:").map(|v| v.trim().to_owned()))
        .unwrap_or_default();
    Probe {
        cpu: sysctl("machdep.cpu.brand_string"),
        memory_bytes: sysctl("hw.memsize").parse().unwrap_or(0),
        disk,
        os: format!(
            "macOS {} ({})",
            output("sw_vers", &["-productVersion"]),
            output("uname", &["-r"])
        ),
    }
}

/// The load average of the last minute (`sysctl vm.loadavg`: `{ 1m 5m 15m }`).
pub(crate) fn load_average() -> Option<f64> {
    output("sysctl", &["-n", "vm.loadavg"])
        .trim_matches(|c| c == '{' || c == '}' || c == ' ')
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// The trimmed output of a command, or nothing.
fn output(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_probe_finds_the_boot_disk_and_a_missing_command_gives_nothing() {
        let p = probe();
        assert!(!p.disk.is_empty() && p.memory_bytes > 0, "{}", p.disk);
        assert_eq!(output("/nonexistent/command", &[]), "");
        assert!(load_average().is_some_and(|l| l >= 0.0));
    }
}
