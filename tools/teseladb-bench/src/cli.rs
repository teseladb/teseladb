//! Command-line arguments (contract `bench-cli.md` of 002, which adds the rest).

use std::path::PathBuf;
use std::time::Duration;

/// Options of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct Args {
    /// Scenario.
    pub scenario: String,
    /// Data directory (empty or nonexistent). Without it, a temporary one deleted at the end.
    pub data: Option<PathBuf>,
    /// JSON file with the result.
    pub json: Option<PathBuf>,
    /// Duration of the timed phases.
    pub duration: Duration,
    /// Warm-up before each timed phase: its operations run untimed and do not count.
    pub warmup: Duration,
    /// Records: entries to append (`raftlog`), corpus size in bytes (`deflate`).
    pub records: u64,
    /// Raft groups (`raftlog` scenario).
    pub groups: u64,
    /// Baseline to compare with (`--baseline`).
    pub baseline: Option<PathBuf>,
    /// Where to save this run as a baseline (`--save-baseline`).
    pub save_baseline: Option<PathBuf>,
    /// A saved report to compare with the baseline instead of running (`--compare`).
    pub compare: Option<PathBuf>,
    /// Output on stdout: text or JSON (`--format`).
    pub format: Format,
    /// Server of the network scenarios (`refdb://host:port`); a local cluster without it.
    pub target: Option<String>,
    /// Workers of the network scenarios.
    pub concurrency: usize,
    /// Operations per second of an open loop; a closed loop without it.
    pub rate: Option<f64>,
    /// Most cores of `cores-curve` (012 FR-009).
    pub max_cores: u16,
}

/// Output format of `--format`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// The report as text.
    #[default]
    Text,
    /// The report as JSON.
    Json,
}

/// Value of `--format` for [`Format::Text`].
pub const FORMAT_TEXT: &str = "text";
/// Value of `--format` for [`Format::Json`].
pub const FORMAT_JSON: &str = "json";

impl Default for Args {
    fn default() -> Self {
        Args {
            scenario: String::new(),
            data: None,
            json: None,
            duration: Duration::from_secs(60),
            warmup: Duration::from_secs(10),
            records: 1_000_000,
            groups: 1000,
            baseline: None,
            save_baseline: None,
            compare: None,
            format: Format::Text,
            target: None,
            concurrency: 1,
            rate: None,
            max_cores: 16,
        }
    }
}

/// Usage.
pub const USAGE: &str = "usage: teseladb-bench --scenario raftlog|raftlog-recover|deflate|bson|js|keys|lsm|\
                         lsm-sustained|write|read|failover|follower-read|group0-limits|split|add-node|rebalance|reference|wire|crud|index-write|index-read|index-build-load|aggregate|sort|group-large|fle-query|txn|txn-readers|contention|tls-handshake|scram|timeseries|ycsb-a..ycsb-f|cores-curve|disk-usage [--data dir] [--json file] \
                         [--duration 3s] [--warmup 10s] [--records N] [--groups N] [--format text|json] \
                         [--target refdb://host:port] [--concurrency N] [--rate ops/s] [--max-cores N] \
                         [--baseline file] [--save-baseline file]\n       \
                         teseladb-bench --compare report.json --baseline file";

/// Parses a duration such as `30s`, `500ms` or `2m`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let (num, unit) = s.find(|c: char| !c.is_ascii_digit()).map_or((s, ""), |i| s.split_at(i));
    let n: u64 = num.parse().map_err(|_| format!("invalid duration: {s}"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "s" | "" => Ok(Duration::from_secs(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        _ => Err(format!("unknown duration unit: {s}")),
    }
}

/// Parses the arguments (without the program name).
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("missing value for {a}"));
        match a.as_str() {
            "--scenario" => out.scenario = value()?,
            "--data" => out.data = Some(PathBuf::from(value()?)),
            "--json" => out.json = Some(PathBuf::from(value()?)),
            "--duration" => out.duration = parse_duration(&value()?)?,
            "--warmup" => out.warmup = parse_duration(&value()?)?,
            "--records" => {
                // With no records the latency histograms are empty and their targets meaningless.
                out.records = value()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or("--records needs a number above 0")?
            }
            "--groups" => out.groups = value()?.parse().map_err(|_| "--groups needs a number")?,
            "--baseline" => out.baseline = Some(PathBuf::from(value()?)),
            "--save-baseline" => out.save_baseline = Some(PathBuf::from(value()?)),
            "--compare" => out.compare = Some(PathBuf::from(value()?)),
            "--target" => out.target = Some(value()?),
            "--max-cores" => {
                out.max_cores = value()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or("--max-cores needs a number above 0")?
            }
            "--concurrency" => {
                out.concurrency = value()?
                    .parse()
                    .ok()
                    .filter(|&n| n > 0)
                    .ok_or("--concurrency needs a number above 0")?
            }
            "--rate" => {
                out.rate = Some(
                    value()?
                        .parse()
                        .ok()
                        .filter(|r: &f64| *r > 0.0)
                        .ok_or("--rate needs operations per second above 0")?,
                )
            }
            "--format" => {
                out.format = match value()?.as_str() {
                    FORMAT_TEXT => Format::Text,
                    FORMAT_JSON => Format::Json,
                    other => return Err(format!("--format is text or json, not {other}")),
                }
            }
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown option: {other}\n{USAGE}")),
        }
    }
    if out.compare.is_some() && out.baseline.is_none() {
        return Err(format!("--compare needs --baseline\n{USAGE}"));
    }
    if out.scenario.is_empty() && out.compare.is_none() {
        return Err(USAGE.into());
    }
    Ok(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(s.split_whitespace().map(String::from))
    }

    #[test]
    fn parses_options_and_rejects_bad_ones() {
        let a = p("--scenario raftlog --data /tmp/x --json o.json --duration 500ms --records 10 --groups 5")
            .unwrap();
        assert_eq!(a.scenario, "raftlog");
        assert_eq!(a.data, Some(PathBuf::from("/tmp/x")));
        assert_eq!(a.json, Some(PathBuf::from("o.json")));
        assert_eq!(a.duration, Duration::from_millis(500));
        assert_eq!(a.warmup, Duration::from_secs(10), "the contract's default");
        assert_eq!((a.records, a.groups), (10, 5));
        assert_eq!(p("").unwrap_err(), USAGE);
        assert_eq!(p("--help").unwrap_err(), USAGE);
        assert!(p("--scenario").unwrap_err().contains("missing value"));
        assert!(p("--scenario s --nope").unwrap_err().contains("unknown"));
        assert!(p("--scenario s --records x").is_err());
        assert!(p("--scenario s --records 0").unwrap_err().contains("above 0"));
        assert!(p("--scenario s --groups x").is_err());
        assert!(p("--scenario s --duration 3h").is_err());
        assert!(p("--scenario s --duration x").is_err());
        assert_eq!(a.max_cores, 16, "012 FR-009");
        assert_eq!(p("--scenario cores-curve --max-cores 4").unwrap().max_cores, 4);
        assert!(p("--scenario s --max-cores 0").unwrap_err().contains("--max-cores"));
    }

    #[test]
    fn parses_the_warm_up() {
        assert_eq!(p("--scenario s --warmup 250ms").unwrap().warmup, Duration::from_millis(250));
        assert_eq!(p("--scenario s --warmup 0").unwrap().warmup, Duration::ZERO);
        assert!(p("--scenario s --warmup").unwrap_err().contains("missing value for --warmup"));
        assert!(p("--scenario s --warmup 3h").unwrap_err().contains("unknown duration unit"));
        assert!(USAGE.contains("[--warmup 10s]"));
    }

    #[test]
    fn parses_baselines_comparisons_and_formats() {
        let a = p("--scenario s --baseline b.json --save-baseline n.json --format json").unwrap();
        assert_eq!(a.baseline, Some(PathBuf::from("b.json")));
        assert_eq!(a.save_baseline, Some(PathBuf::from("n.json")));
        assert_eq!(a.format, Format::Json);
        assert_eq!(p("--scenario s --format text").unwrap().format, Format::Text);
        assert!(p("--scenario s --format xml").unwrap_err().contains("text or json"));
        let c = p("--compare r.json --baseline b.json").unwrap();
        assert_eq!((c.compare, c.scenario.as_str()), (Some(PathBuf::from("r.json")), ""));
        assert!(p("--compare r.json").unwrap_err().contains("needs --baseline"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("7"), Ok(Duration::from_secs(7)));
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
    }
}
