//! TeselaDB against the reference server (002 FR-028): the repeats of each side, of the same
//! scenario at the same concurrency on the same machine, give for every measure a median and a
//! spread (the largest repeat minus the smallest). TeselaDB loses a measure when its median of
//! the throughput (units per second, more is better) or of a p50, p99 or p99.9 (less is better)
//! is worse than the reference's by more than the larger of the two spreads.

use teseladb_json::Value;

use crate::fingerprint::Fingerprint;

/// Suffixes of the latencies the rule applies to.
const LATENCIES: [&str; 3] = [".p50", ".p99", ".p999"];
/// Suffix of the units of a rate: operations, bytes or anything else per second.
const PER_SECOND: &str = "/s";

/// One repeat of one side, as `--json` writes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Repeat {
    /// Scenario.
    pub scenario: String,
    /// Connections of the run; `None` in a report written before it was recorded.
    pub concurrency: Option<u64>,
    /// Measures by name, in order: unit and value.
    pub metrics: Vec<(String, String, f64)>,
    /// Machine.
    pub fingerprint: Fingerprint,
}

impl Repeat {
    /// From the JSON of a report.
    pub fn parse(text: &str) -> Result<Repeat, String> {
        let v = teseladb_json::parse(text.as_bytes()).map_err(crate::client::text)?;
        let scenario =
            v.get("scenario").and_then(Value::as_str).ok_or("report without `scenario`")?;
        let concurrency = v.get("concurrency").and_then(Value::as_f64).map(|c| c as u64);
        let list = v.get("metrics").and_then(Value::as_array).ok_or("report without `metrics`")?;
        let metrics = list
            .iter()
            .map(|m| {
                let name = m.get("name").and_then(Value::as_str).ok_or("metric without `name`")?;
                let unit = m.get("unit").and_then(Value::as_str).unwrap_or_default();
                let value = m
                    .get("value")
                    .and_then(Value::as_f64)
                    .ok_or(format!("metric {name} without a value"))?;
                Ok((name.to_owned(), unit.to_owned(), value))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let fingerprint =
            Fingerprint::from_value(v.get("fingerprint").ok_or("report without `fingerprint`")?)?;
        Ok(Repeat { scenario: scenario.to_owned(), concurrency, metrics, fingerprint })
    }

    fn value(&self, name: &str) -> Option<f64> {
        self.metrics.iter().find(|(m, _, _)| m == name).map(|(_, _, v)| *v)
    }
}

/// Which way a ruled measure is better.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Better {
    /// A rate: more is better.
    Higher,
    /// A latency: less is better.
    Lower,
}

/// The direction of a measure the rule applies to, or `None` for the rest.
pub fn ruled(name: &str, unit: &str) -> Option<Better> {
    if unit.ends_with(PER_SECOND) {
        Some(Better::Higher)
    } else if LATENCIES.iter().any(|s| name.ends_with(s)) {
        Some(Better::Lower)
    } else {
        None
    }
}

/// Median and spread of the repeats of one side.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    /// Median.
    pub median: f64,
    /// Largest repeat minus the smallest.
    pub spread: f64,
}

impl Summary {
    /// Of a non-empty list of values.
    pub fn of(values: &[f64]) -> Summary {
        let mut v = values.to_vec();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let median = if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 };
        Summary { median, spread: v[n - 1] - v[0] }
    }
}

/// One ruled measure of the two sides.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Measure.
    pub name: String,
    /// Unit.
    pub unit: String,
    /// Which way is better.
    pub better: Better,
    /// TeselaDB's repeats.
    pub ours: Summary,
    /// The reference's repeats.
    pub reference: Summary,
}

impl Row {
    /// Tells whether TeselaDB's median is worse than the reference's beyond the larger spread.
    pub fn lost(&self) -> bool {
        let worse_by = match self.better {
            Better::Higher => self.reference.median - self.ours.median,
            Better::Lower => self.ours.median - self.reference.median,
        };
        worse_by > self.ours.spread.max(self.reference.spread)
    }
}

/// Refuses repeats that are not of the same scenario, concurrency and machine as the first one.
fn same_run(first: &Repeat, other: &Repeat, which: &str) -> Result<(), String> {
    if other.scenario != first.scenario {
        return Err(format!(
            "{which} is of scenario {}, not {}: they do not compare",
            other.scenario, first.scenario
        ));
    }
    if other.concurrency != first.concurrency {
        return Err(format!(
            "{which} ran with concurrency {}, not {}: they do not compare",
            shown(other.concurrency),
            shown(first.concurrency)
        ));
    }
    if other.fingerprint != first.fingerprint {
        return Err(format!(
            "{which} ran on another machine ({}, not {}): they do not compare",
            other.fingerprint, first.fingerprint
        ));
    }
    Ok(())
}

fn shown(c: Option<u64>) -> String {
    c.map_or("unknown".into(), |c| c.to_string())
}

/// The ruled measures of the reference against TeselaDB, in the reference's order.
pub fn compare(ours: &[Repeat], reference: &[Repeat]) -> Result<Vec<Row>, String> {
    let first = reference.first().ok_or("no report of the reference")?;
    if ours.is_empty() {
        return Err("no report of TeselaDB".into());
    }
    for (i, r) in reference.iter().enumerate() {
        same_run(first, r, &format!("repeat {} of the reference", i + 1))?;
    }
    for (i, r) in ours.iter().enumerate() {
        same_run(first, r, &format!("repeat {} of TeselaDB", i + 1))?;
    }
    first
        .metrics
        .iter()
        .filter_map(|(name, unit, _)| ruled(name, unit).map(|b| (name, unit, b)))
        .map(|(name, unit, better)| {
            let side = |reps: &[Repeat], which: &str| {
                reps.iter()
                    .map(|r| r.value(name).ok_or(format!("a report of {which} has no {name}")))
                    .collect::<Result<Vec<f64>, String>>()
                    .map(|v| Summary::of(&v))
            };
            Ok(Row {
                name: name.clone(),
                unit: unit.clone(),
                better,
                ours: side(ours, "TeselaDB")?,
                reference: side(reference, "the reference")?,
            })
        })
        .collect()
}

/// One line per row: both medians with their spreads, and `LOSS` where TeselaDB loses.
pub fn render(rows: &[Row]) -> String {
    let mut out = format!(
        "  {:<34} {:>8} {:>16} {:>12} {:>16} {:>12}\n",
        "measure", "unit", "teseladb", "spread", "reference", "spread"
    );
    for r in rows {
        let tag = if r.lost() { "  LOSS" } else { "" };
        out.push_str(&format!(
            "  {:<34} {:>8} {:>16.1} {:>12.1} {:>16.1} {:>12.1}{tag}\n",
            r.name, r.unit, r.ours.median, r.ours.spread, r.reference.median, r.reference.spread
        ));
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn fp() -> Fingerprint {
        Fingerprint { cpu: "c".into(), cores: 1, memory_bytes: 1, disk: "d".into(), os: "o".into() }
    }

    fn rep(p50: f64, rate: f64) -> Repeat {
        Repeat {
            scenario: "s".into(),
            concurrency: Some(8),
            metrics: vec![
                ("op.p50".into(), "ns".into(), p50),
                ("throughput".into(), "op/s".into(), rate),
                ("errors".into(), "op".into(), 3.0),
            ],
            fingerprint: fp(),
        }
    }

    #[test]
    fn medians_and_spreads() {
        assert_eq!(Summary::of(&[3.0, 1.0, 2.0]), Summary { median: 2.0, spread: 2.0 });
        assert_eq!(Summary::of(&[4.0, 1.0]), Summary { median: 2.5, spread: 3.0 });
        assert_eq!(Summary::of(&[7.0]), Summary { median: 7.0, spread: 0.0 });
    }

    #[test]
    fn only_rates_and_the_three_latencies_are_ruled() {
        assert_eq!(ruled("throughput", "op/s"), Some(Better::Higher));
        assert_eq!(ruled("lsm.flush_mb_s", "MB/s"), Some(Better::Higher));
        for n in ["read.p50", "read.p99", "read.p999"] {
            assert_eq!(ruled(n, "ns"), Some(Better::Lower), "{n}");
        }
        for n in ["read.p90", "read.p9999", "read.max", "errors", "host.load_per_core"] {
            assert_eq!(ruled(n, "ns"), None, "{n}");
        }
    }

    #[test]
    fn a_loss_is_beyond_the_larger_spread() {
        let rows = compare(&[rep(120.0, 90.0)], &[rep(100.0, 100.0), rep(110.0, 100.0)]).unwrap();
        assert_eq!(rows.len(), 2, "errors is not ruled");
        // p50: 120 against a median of 105 with a spread of 10.
        assert!(rows[0].lost());
        // throughput: 90 against 100, spread 0.
        assert!(rows[1].lost());
        let rows = compare(&[rep(114.0, 100.0)], &[rep(100.0, 100.0), rep(110.0, 100.0)]).unwrap();
        assert!(!rows[0].lost() && !rows[1].lost(), "{rows:?}");
        let text = render(&rows);
        assert!(text.contains("op.p50") && !text.contains("LOSS"), "{text}");
    }

    #[test]
    fn sides_that_do_not_compare_are_errors() {
        assert!(compare(&[rep(1.0, 1.0)], &[]).unwrap_err().contains("reference"));
        assert!(compare(&[], &[rep(1.0, 1.0)]).unwrap_err().contains("TeselaDB"));
        let old = Repeat { concurrency: None, ..rep(1.0, 1.0) };
        let e = compare(&[old], &[rep(1.0, 1.0)]).unwrap_err();
        assert!(e.contains("unknown") && e.contains("concurrency"), "{e}");
        let other = Repeat { scenario: "t".into(), ..rep(1.0, 1.0) };
        assert!(
            compare(&[rep(1.0, 1.0)], &[rep(1.0, 1.0), other]).unwrap_err().contains("scenario")
        );
        let machine =
            Repeat { fingerprint: Fingerprint { disk: "x".into(), ..fp() }, ..rep(1.0, 1.0) };
        assert!(compare(&[machine], &[rep(1.0, 1.0)]).unwrap_err().contains("machine"));
        let mut lacking = rep(1.0, 1.0);
        lacking.metrics.remove(0);
        assert!(compare(&[lacking], &[rep(1.0, 1.0)]).unwrap_err().contains("op.p50"));
    }

    #[test]
    fn reports_read_back() {
        let text = r#"{"scenario":"s","concurrency":8,"metrics":[{"name":"op.p50","unit":"ns","value":1}],
            "fingerprint":{"cpu":"c","cores":1,"memory_bytes":1,"disk":"d","os":"o"}}"#;
        let r = Repeat::parse(text).unwrap();
        assert_eq!((r.concurrency, r.metrics.len()), (Some(8), 1));
        for (text, want) in [
            ("{", "line"),
            ("{}", "scenario"),
            (r#"{"scenario":"s"}"#, "metrics"),
            (r#"{"scenario":"s","metrics":[{}]}"#, "name"),
            (r#"{"scenario":"s","metrics":[{"name":"a"}]}"#, "value"),
            (r#"{"scenario":"s","metrics":[]}"#, "fingerprint"),
        ] {
            let e = Repeat::parse(text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }
}
