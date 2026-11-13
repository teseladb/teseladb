//! Result of a scenario: metrics with their target, as text and as JSON.

use std::fmt;

use teseladb_types::Histogram;

use crate::fingerprint::Fingerprint;

/// Target of a metric.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// The value has to be at most this.
    AtMost(f64),
    /// The value has to be at least this.
    AtLeast(f64),
}

impl Target {
    /// Tells whether `v` meets the target.
    pub fn met(self, v: f64) -> bool {
        match self {
            Target::AtMost(t) => v <= t,
            Target::AtLeast(t) => v >= t,
        }
    }
}

/// A measured metric.
#[derive(Clone, Debug, PartialEq)]
pub struct Metric {
    /// Name (stable: 005's baselines use it).
    pub name: String,
    /// Unit.
    pub unit: &'static str,
    /// Value.
    pub value: f64,
    /// Target from the plan, if any.
    pub target: Option<Target>,
}

/// Result of a scenario.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    /// Scenario.
    pub scenario: String,
    /// Metrics in order.
    pub metrics: Vec<Metric>,
    /// Notes (environment, warnings).
    pub notes: Vec<String>,
    /// Machine it ran on (FR-022).
    pub fingerprint: Option<Fingerprint>,
}

impl Report {
    /// Empty report of a scenario.
    pub fn new(scenario: &str) -> Report {
        Report { scenario: scenario.into(), ..Default::default() }
    }

    /// Adds a metric.
    pub fn push(&mut self, name: &str, unit: &'static str, value: f64, target: Option<Target>) {
        self.metrics.push(Metric { name: name.into(), unit, value, target });
    }

    /// Adds p50, p99 and p99.9 of a histogram in nanoseconds, with optional targets.
    pub fn latency(&mut self, name: &str, h: &Histogram, p50: Option<f64>, p99: Option<f64>) {
        self.push(&format!("{name}.p50"), "ns", h.percentile(50.0) as f64, p50.map(Target::AtMost));
        self.push(&format!("{name}.p90"), "ns", h.percentile(90.0) as f64, None);
        self.push(&format!("{name}.p99"), "ns", h.percentile(99.0) as f64, p99.map(Target::AtMost));
        self.push(&format!("{name}.p999"), "ns", h.percentile(99.9) as f64, None);
        // 002 FR-036: the tail to p99.99 and the worst one.
        self.push(&format!("{name}.p9999"), "ns", h.percentile(99.99) as f64, None);
        self.push(&format!("{name}.max"), "ns", h.max() as f64, None);
    }

    /// Metrics with a target they do not meet.
    pub fn missed(&self) -> Vec<&Metric> {
        self.metrics.iter().filter(|m| m.target.is_some_and(|t| !t.met(m.value))).collect()
    }

    /// JSON of the report (no dependencies: written by hand).
    pub fn to_json(&self) -> String {
        let mut out = format!("{{\"scenario\":{},\"metrics\":[", quote(&self.scenario));
        for (i, m) in self.metrics.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"name\":{},\"unit\":{},\"value\":{}",
                quote(&m.name),
                quote(m.unit),
                number(m.value)
            ));
            if let Some(t) = m.target {
                let (kind, v) = match t {
                    Target::AtMost(v) => ("at_most", v),
                    Target::AtLeast(v) => ("at_least", v),
                };
                out.push_str(&format!(
                    ",\"target\":{{\"{kind}\":{}}},\"met\":{}",
                    number(v),
                    t.met(m.value)
                ));
            }
            out.push('}');
        }
        out.push_str("],\"notes\":[");
        let notes: Vec<String> = self.notes.iter().map(|n| quote(n)).collect();
        out.push_str(&notes.join(","));
        out.push(']');
        if let Some(f) = &self.fingerprint {
            out.push_str(",\"fingerprint\":");
            out.push_str(&teseladb_json::to_string(&f.to_value()).unwrap_or_default());
        }
        out.push('}');
        out
    }
}

fn number(v: f64) -> String {
    if v.is_finite() { format!("{v}") } else { "null".into() }
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "scenario {}", self.scenario)?;
        for m in &self.metrics {
            let target = match m.target {
                None => String::new(),
                Some(t) => {
                    let (op, v) = match t {
                        Target::AtMost(v) => ("≤", v),
                        Target::AtLeast(v) => ("≥", v),
                    };
                    format!(
                        "  target {op} {v} {}  {}",
                        m.unit,
                        if t.met(m.value) { "OK" } else { "NO" }
                    )
                }
            };
            writeln!(f, "  {:<34} {:>14.1} {:<4}{target}", m.name, m.value, m.unit)?;
        }
        for n in &self.notes {
            writeln!(f, "  note: {n}")?;
        }
        let missed = self.missed().len();
        write!(f, "targets missed: {missed}")
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn targets_text_and_json() {
        let mut r = Report::new("raftlog");
        let mut h = Histogram::new();
        for v in [100, 200, 300] {
            h.record(v);
        }
        r.latency("put", &h, Some(1000.0), None);
        r.push("batch.mean", "req", 5.0, Some(Target::AtLeast(20.0)));
        r.push("weird", "x", f64::NAN, None);
        r.notes.push("macOS \"indicative\"\n".into());
        assert_eq!(r.missed().len(), 1);
        assert!(Target::AtLeast(1.0).met(1.0) && !Target::AtMost(1.0).met(2.0));
        let text = r.to_string();
        assert!(text.contains("put.p50") && text.contains("OK") && text.contains("NO"), "{text}");
        assert!(text.ends_with("targets missed: 1"));
        let j = r.to_json();
        assert!(j.starts_with("{\"scenario\":\"raftlog\""), "{j}");
        assert!(j.contains("\"target\":{\"at_least\":20},\"met\":false"), "{j}");
        assert!(j.contains("\"value\":null"));
        assert!(j.contains("\\\"indicative\\\"\\u000a"), "{j}");
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod latency_tests {
    use super::*;

    #[test]
    fn a_latency_gives_the_tail_to_p9999_and_the_max() {
        let mut h = Histogram::new();
        for v in 1..=100_000u64 {
            h.record(v * 1000);
        }
        let mut r = Report::new("x");
        r.latency("op", &h, None, None);
        let names: Vec<&str> = r.metrics.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["op.p50", "op.p90", "op.p99", "op.p999", "op.p9999", "op.max"]);
        let value = |n: &str| r.metrics.iter().find(|m| m.name == n).unwrap().value;
        assert!(value("op.p9999") >= value("op.p999"));
        let max = value("op.max");
        assert!((max - 100_000_000.0).abs() / 100_000_000.0 < 0.01, "{max}");
    }
}
