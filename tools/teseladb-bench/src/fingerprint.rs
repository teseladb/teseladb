//! Fingerprint of the machine (002 FR-022, R7): CPU model, cores, memory, disk model and system.
//! A comparison with a baseline only means something on the same machine, so a report carries its
//! fingerprint and a baseline of another one is refused.

#[cfg(target_os = "linux")]
#[path = "fingerprint/linux.rs"]
mod platform;
#[cfg(target_os = "macos")]
#[path = "fingerprint/macos.rs"]
mod platform;

use teseladb_json::{Object, Value};

/// What a report's numbers depend on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fingerprint {
    /// CPU model.
    pub cpu: String,
    /// Cores the process can use.
    pub cores: u64,
    /// Physical memory in bytes.
    pub memory_bytes: u64,
    /// Model of the disk that holds the system.
    pub disk: String,
    /// System and kernel.
    pub os: String,
}

/// What a probe that cannot find a value gives.
const UNKNOWN: &str = "unknown";

impl Fingerprint {
    /// This machine's.
    pub fn current() -> Fingerprint {
        let cores = std::thread::available_parallelism().map_or(0, |n| n.get() as u64);
        let p = platform::probe();
        Fingerprint {
            cpu: known(p.cpu),
            cores,
            memory_bytes: p.memory_bytes,
            disk: known(p.disk),
            os: known(p.os),
        }
    }

    /// As a JSON object.
    pub fn to_value(&self) -> Value {
        let mut o = Object::new();
        o.insert("cpu", self.cpu.as_str());
        o.insert("cores", self.cores);
        o.insert("memory_bytes", self.memory_bytes);
        o.insert("disk", self.disk.as_str());
        o.insert("os", self.os.as_str());
        Value::Object(o)
    }

    /// From a JSON object.
    pub fn from_value(v: &Value) -> Result<Fingerprint, String> {
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(format!("fingerprint without `{k}`"))
        };
        let num =
            |k: &str| v.get(k).and_then(Value::as_u64).ok_or(format!("fingerprint without `{k}`"));
        Ok(Fingerprint {
            cpu: text("cpu")?,
            cores: num("cores")?,
            memory_bytes: num("memory_bytes")?,
            disk: text("disk")?,
            os: text("os")?,
        })
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}, {} cores, {} bytes, {}, {}",
            self.cpu, self.cores, self.memory_bytes, self.disk, self.os
        )
    }
}

/// The load average of the last minute, if the system gives it.
pub fn load_average() -> Option<f64> {
    platform::load_average()
}

fn known(s: String) -> String {
    if s.trim().is_empty() || s.trim() == "()" { UNKNOWN.into() } else { s }
}

/// What a platform's probe finds; the cores are the same everywhere.
pub(crate) struct Probe {
    pub cpu: String,
    pub memory_bytes: u64,
    pub disk: String,
    pub os: String,
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn this_machine_has_a_fingerprint_that_reads_back() {
        let f = Fingerprint::current();
        assert!(f.cores > 0 && f.memory_bytes > 0, "{f}");
        assert_ne!(f.cpu, UNKNOWN, "{f}");
        assert_eq!(Fingerprint::from_value(&f.to_value()), Ok(f.clone()));
        assert!(f.to_string().contains("cores"));
    }

    #[test]
    fn missing_fields_and_empty_values() {
        let v = teseladb_json::parse(br#"{"cpu":"x","cores":1}"#).expect("json");
        assert!(Fingerprint::from_value(&v).unwrap_err().contains("memory_bytes"));
        assert_eq!(known(" ".into()), UNKNOWN);
        assert_eq!(known(" ()".into()), UNKNOWN);
        assert_eq!(known("a".into()), "a");
    }
}
