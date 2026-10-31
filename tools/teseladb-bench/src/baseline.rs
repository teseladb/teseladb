//! Baselines (002 FR-017, FR-022): a saved report with its machine's fingerprint, and the rule that
//! a p50 or a p99 more than [`LIMIT_PCT`] worse than the baseline's is a regression. A baseline
//! only compares with a report of the same scenario on a machine with the same fingerprint, and
//! only when both runs started on a machine about as idle as each other and not oversubscribed
//! ([`HOST_LOAD`]): with the machine busy, two identical runs of `reference` differed by more than
//! the rule allows (002 T034).

use teseladb_json::Value;

use crate::fingerprint::Fingerprint;
use crate::report::Report;

/// Largest worsening of a p50 or a p99 that is not a regression, in per cent.
pub const LIMIT_PCT: f64 = 10.0;
/// Suffixes of the metrics the rule applies to.
const RULED: [&str; 2] = [".p50", ".p99"];
/// Metric with the load of the machine when the run started: the load average of the last minute
/// over the cores.
pub const HOST_LOAD: &str = "host.load_per_core";
/// Load per core above which a run measured the scheduler more than the code: more threads wanted
/// to run than there were cores.
pub const BUSY_LOAD: f64 = 1.0;
/// Largest difference of load per core between two runs that compare.
pub const LOAD_GAP: f64 = 0.25;

/// A report as saved: what a comparison needs.
#[derive(Clone, Debug, PartialEq)]
pub struct Saved {
    /// Scenario.
    pub scenario: String,
    /// Metrics by name, in order.
    pub metrics: Vec<(String, f64)>,
    /// Machine.
    pub fingerprint: Fingerprint,
}

/// One ruled metric of the baseline against the report.
#[derive(Clone, Debug, PartialEq)]
pub struct Delta {
    /// Metric.
    pub name: String,
    /// Baseline's value.
    pub base: f64,
    /// Report's value.
    pub now: f64,
    /// Change in per cent (positive is worse: the ruled metrics are latencies).
    pub pct: f64,
}

impl Delta {
    /// Tells whether it breaks the rule.
    pub fn regressed(&self) -> bool {
        self.pct > LIMIT_PCT
    }
}

impl Saved {
    /// From a report of this run on this machine.
    pub fn of(report: &Report, fingerprint: Fingerprint) -> Saved {
        Saved {
            scenario: report.scenario.clone(),
            metrics: report.metrics.iter().map(|m| (m.name.clone(), m.value)).collect(),
            fingerprint,
        }
    }

    /// From the JSON of a report with its fingerprint (`--json`, `--save-baseline`).
    pub fn parse(text: &str) -> Result<Saved, String> {
        let v = teseladb_json::parse(text.as_bytes()).map_err(|e| e.to_string())?;
        let scenario =
            v.get("scenario").and_then(Value::as_str).ok_or("report without `scenario`")?;
        let list = v.get("metrics").and_then(Value::as_array).ok_or("report without `metrics`")?;
        let metrics = list
            .iter()
            .map(|m| {
                let name = m.get("name").and_then(Value::as_str).ok_or("metric without `name`")?;
                let value = m
                    .get("value")
                    .and_then(Value::as_f64)
                    .ok_or(format!("metric {name} without a value"))?;
                Ok((name.to_owned(), value))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let fingerprint =
            Fingerprint::from_value(v.get("fingerprint").ok_or("report without `fingerprint`")?)?;
        Ok(Saved { scenario: scenario.to_owned(), metrics, fingerprint })
    }
}

/// Refuses a baseline of another scenario or of another machine.
pub fn same_run(base: &Saved, scenario: &str, fingerprint: &Fingerprint) -> Result<(), String> {
    if base.scenario != scenario {
        return Err(format!("the baseline is of scenario {}, not {scenario}", base.scenario));
    }
    if &base.fingerprint != fingerprint {
        return Err(format!(
            "the baseline is of another machine (fingerprint: {}; this one: {fingerprint})",
            base.fingerprint
        ));
    }
    Ok(())
}

/// The load per core a saved run started with.
fn host_load(s: &Saved, which: &str) -> Result<f64, String> {
    s.metrics.iter().find(|(m, _)| m == HOST_LOAD).map(|(_, v)| *v).ok_or(format!(
        "the {which} has no {HOST_LOAD}: run it again to know how busy the machine was"
    ))
}

/// Refuses two runs that started with the machine oversubscribed or busier one than the other.
pub fn same_load(base: &Saved, now: &Saved) -> Result<(), String> {
    let (b, n) = (host_load(base, "baseline")?, host_load(now, "report")?);
    if b > BUSY_LOAD || n > BUSY_LOAD {
        return Err(format!(
            "the machine was busy (load per core {b:.2} in the baseline, {n:.2} now; the limit is \
             {BUSY_LOAD:.2}): the latencies are not comparable"
        ));
    }
    if (b - n).abs() > LOAD_GAP {
        return Err(format!(
            "the machine was not as busy in both runs (load per core {b:.2} in the baseline, \
             {n:.2} now; they may differ by {LOAD_GAP:.2}): the latencies are not comparable"
        ));
    }
    Ok(())
}

/// The ruled metrics of the baseline against the report, in the baseline's order.
pub fn compare(base: &Saved, now: &Saved) -> Result<Vec<Delta>, String> {
    same_run(base, &now.scenario, &now.fingerprint)?;
    same_load(base, now)?;
    base.metrics
        .iter()
        .filter(|(name, _)| RULED.iter().any(|s| name.ends_with(s)))
        .map(|(name, b)| {
            let n = now.metrics.iter().find(|(m, _)| m == name).map(|(_, v)| *v);
            let n = n.ok_or(format!("the report has no {name}, which the baseline has"))?;
            let pct = if *b > 0.0 { (n - b) / b * 100.0 } else { 0.0 };
            Ok(Delta { name: name.clone(), base: *b, now: n, pct })
        })
        .collect()
}

/// One line per delta.
pub fn render(deltas: &[Delta]) -> String {
    deltas
        .iter()
        .map(|d| {
            let tag = if d.regressed() { "  REGRESSION" } else { "" };
            format!("  {:<34} {:>14.1} -> {:>14.1}  {:+.1} %{tag}\n", d.name, d.base, d.now, d.pct)
        })
        .collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn fp(disk: &str) -> Fingerprint {
        Fingerprint {
            cpu: "c".into(),
            cores: 1,
            memory_bytes: 1,
            disk: disk.into(),
            os: "o".into(),
        }
    }

    fn saved(p50: f64, p99: f64) -> Saved {
        Saved {
            scenario: "s".into(),
            metrics: vec![
                ("op.p50".into(), p50),
                ("op.p99".into(), p99),
                ("op.p999".into(), 1.0),
                (HOST_LOAD.into(), 0.1),
            ],
            fingerprint: fp("d"),
        }
    }

    fn loaded(load: f64) -> Saved {
        let mut s = saved(1.0, 1.0);
        s.metrics.retain(|(m, _)| m != HOST_LOAD);
        s.metrics.push((HOST_LOAD.into(), load));
        s
    }

    #[test]
    fn runs_on_a_busy_machine_or_on_unequal_loads_do_not_compare() {
        assert!(compare(&loaded(0.3), &loaded(0.5)).is_ok());
        let e = compare(&loaded(0.3), &loaded(1.2)).unwrap_err();
        assert!(e.contains("busy") && e.contains("1.20"), "{e}");
        assert!(compare(&loaded(1.1), &loaded(1.0)).unwrap_err().contains("busy"));
        let e = compare(&loaded(0.1), &loaded(0.4)).unwrap_err();
        assert!(e.contains("not as busy"), "{e}");
        let mut old = saved(1.0, 1.0);
        old.metrics.retain(|(m, _)| m != HOST_LOAD);
        assert!(compare(&old, &saved(1.0, 1.0)).unwrap_err().contains("baseline has no"));
        assert!(compare(&saved(1.0, 1.0), &old).unwrap_err().contains("report has no"));
    }

    #[test]
    fn the_rule_is_more_than_ten_per_cent_on_p50_or_p99() {
        let d = compare(&saved(100.0, 1000.0), &saved(110.0, 900.0)).unwrap();
        assert_eq!(d.len(), 2);
        assert!(!d[0].regressed(), "exactly 10 % is allowed");
        let d = compare(&saved(100.0, 1000.0), &saved(100.0, 1101.0)).unwrap();
        assert!(d[1].regressed());
        assert!(render(&d).contains("REGRESSION"));
        // A zero baseline cannot regress.
        assert!(!compare(&saved(0.0, 0.0), &saved(5.0, 5.0)).unwrap()[0].regressed());
    }

    #[test]
    fn other_machines_scenarios_and_missing_metrics_are_errors() {
        let other = Saved { fingerprint: fp("x"), ..saved(1.0, 1.0) };
        assert!(compare(&saved(1.0, 1.0), &other).unwrap_err().contains("another machine"));
        let scenario = Saved { scenario: "t".into(), ..saved(1.0, 1.0) };
        assert!(compare(&saved(1.0, 1.0), &scenario).unwrap_err().contains("scenario"));
        let empty = Saved { metrics: vec![(HOST_LOAD.into(), 0.1)], ..saved(1.0, 1.0) };
        assert!(compare(&saved(1.0, 1.0), &empty).unwrap_err().contains("op.p50"));
    }

    #[test]
    fn a_saved_report_reads_back() {
        let mut r = Report::new("s");
        r.push("op.p50", "ns", 5.0, None);
        r.fingerprint = Some(fp("d"));
        let back = Saved::parse(&r.to_json()).unwrap();
        assert_eq!(back, Saved::of(&r, fp("d")));
        for (text, want) in [
            ("{", "line"),
            ("{}", "scenario"),
            (r#"{"scenario":"s"}"#, "metrics"),
            (r#"{"scenario":"s","metrics":[{}]}"#, "name"),
            (r#"{"scenario":"s","metrics":[{"name":"a"}]}"#, "value"),
            (r#"{"scenario":"s","metrics":[]}"#, "fingerprint"),
        ] {
            let e = Saved::parse(text).unwrap_err();
            assert!(e.contains(want), "{text}: {e}");
        }
    }
}
