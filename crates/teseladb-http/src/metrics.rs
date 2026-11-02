//! The metrics of the HTTP port for `/metrics` (019 FR-030): per route of the REST API and per
//! MCP tool, the requests, a latency histogram, the errors by status and the open Server-Sent
//! Events streams, in the Prometheus text format. Labels hold route templates and tool names,
//! never data of a user (a database, a collection, an id).

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The upper bounds of the latency buckets, in seconds.
pub const BUCKETS_SECS: [f64; 12] =
    [0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// What a series counts: a route of the REST API or an MCP tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// A route template (`/v1/databases/{db}/collections`).
    Route,
    /// An MCP tool (`find`).
    Tool,
}

impl Family {
    fn prefix(self) -> &'static str {
        match self {
            Family::Route => "teseladb_http",
            Family::Tool => "teseladb_mcp_tool",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Family::Route => "route",
            Family::Tool => "tool",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Series {
    requests: u64,
    errors: BTreeMap<u16, u64>,
    buckets: [u64; BUCKETS_SECS.len()],
    sum_secs: f64,
    open_streams: u64,
}

/// The metrics of a node's HTTP port.
#[derive(Clone, Debug, Default)]
pub struct HttpMetrics {
    series: BTreeMap<(Family, String), Series>,
}

/// `name` with what a Prometheus label value cannot hold escaped.
fn escaped(name: &str) -> String {
    name.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

impl HttpMetrics {
    /// A request of `name` ended with `status` after `secs` seconds; a status of 400 or more
    /// counts as an error.
    pub fn record(&mut self, family: Family, name: &str, status: u16, secs: f64) {
        let s = self.series.entry((family, name.to_owned())).or_default();
        s.requests += 1;
        s.sum_secs += secs;
        if let Some(i) = BUCKETS_SECS.iter().position(|b| secs <= *b) {
            s.buckets[i] += 1;
        }
        if status >= 400 {
            *s.errors.entry(status).or_default() += 1;
        }
    }

    /// A stream of `name` opened.
    pub fn stream_opened(&mut self, family: Family, name: &str) {
        self.series.entry((family, name.to_owned())).or_default().open_streams += 1;
    }

    /// A stream of `name` closed.
    pub fn stream_closed(&mut self, family: Family, name: &str) {
        if let Some(s) = self.series.get_mut(&(family, name.to_owned())) {
            s.open_streams = s.open_streams.saturating_sub(1);
        }
    }

    /// The metrics in the Prometheus text format.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        for family in [Family::Route, Family::Tool] {
            self.render_family(family, &mut out);
        }
        out
    }

    fn render_family(&self, family: Family, out: &mut String) {
        let (p, l) = (family.prefix(), family.label());
        let all: Vec<(&String, &Series)> = self
            .series
            .iter()
            .filter(|((f, _), _)| *f == family)
            .map(|((_, n), s)| (n, s))
            .collect();
        if all.is_empty() {
            return;
        }
        let _ = writeln!(out, "# HELP {p}_requests_total Requests that ended.");
        let _ = writeln!(out, "# TYPE {p}_requests_total counter");
        for (n, s) in &all {
            let _ = writeln!(out, "{p}_requests_total{{{l}=\"{}\"}} {}", escaped(n), s.requests);
        }
        let _ = writeln!(out, "# HELP {p}_duration_seconds Time from request to answer.");
        let _ = writeln!(out, "# TYPE {p}_duration_seconds histogram");
        for (n, s) in &all {
            let n = escaped(n);
            let mut cumulative = 0;
            for (b, count) in BUCKETS_SECS.iter().zip(s.buckets) {
                cumulative += count;
                let _ = writeln!(
                    out,
                    "{p}_duration_seconds_bucket{{{l}=\"{n}\",le=\"{b}\"}} {cumulative}"
                );
            }
            let _ = writeln!(
                out,
                "{p}_duration_seconds_bucket{{{l}=\"{n}\",le=\"+Inf\"}} {}",
                s.requests
            );
            let _ = writeln!(out, "{p}_duration_seconds_sum{{{l}=\"{n}\"}} {}", s.sum_secs);
            let _ = writeln!(out, "{p}_duration_seconds_count{{{l}=\"{n}\"}} {}", s.requests);
        }
        let _ =
            writeln!(out, "# HELP {p}_errors_total Requests that ended with an error, by status.");
        let _ = writeln!(out, "# TYPE {p}_errors_total counter");
        for (n, s) in &all {
            for (status, count) in &s.errors {
                let _ = writeln!(
                    out,
                    "{p}_errors_total{{{l}=\"{}\",status=\"{status}\"}} {count}",
                    escaped(n)
                );
            }
        }
        let _ = writeln!(out, "# HELP {p}_sse_streams_open Server-Sent Events streams open.");
        let _ = writeln!(out, "# TYPE {p}_sse_streams_open gauge");
        for (n, s) in &all {
            let _ =
                writeln!(out, "{p}_sse_streams_open{{{l}=\"{}\"}} {}", escaped(n), s.open_streams);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn requests_latency_errors_and_streams_per_route_and_tool() {
        let mut m = HttpMetrics::default();
        assert_eq!(m.render(), "");
        m.record(Family::Route, "/v1/databases", 200, 0.003);
        m.record(Family::Route, "/v1/databases", 404, 20.0);
        m.record(Family::Tool, "find", 200, 0.2);
        m.stream_opened(Family::Route, "/v1/changeStream");
        m.stream_opened(Family::Route, "/v1/changeStream");
        m.stream_closed(Family::Route, "/v1/changeStream");
        m.stream_closed(Family::Route, "/never");
        let text = m.render();
        for line in [
            "teseladb_http_requests_total{route=\"/v1/databases\"} 2",
            "teseladb_http_duration_seconds_bucket{route=\"/v1/databases\",le=\"0.001\"} 0",
            "teseladb_http_duration_seconds_bucket{route=\"/v1/databases\",le=\"0.005\"} 1",
            "teseladb_http_duration_seconds_bucket{route=\"/v1/databases\",le=\"10\"} 1",
            "teseladb_http_duration_seconds_bucket{route=\"/v1/databases\",le=\"+Inf\"} 2",
            "teseladb_http_duration_seconds_count{route=\"/v1/databases\"} 2",
            "teseladb_http_errors_total{route=\"/v1/databases\",status=\"404\"} 1",
            "teseladb_http_sse_streams_open{route=\"/v1/changeStream\"} 1",
            "teseladb_mcp_tool_requests_total{tool=\"find\"} 1",
            "# TYPE teseladb_mcp_tool_duration_seconds histogram",
        ] {
            assert!(text.lines().any(|l| l == line), "{line} in\n{text}");
        }
        assert!(!text.contains("/never"));
        assert_eq!(escaped("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }
}
