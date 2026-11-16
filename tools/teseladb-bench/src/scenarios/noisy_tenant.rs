//! `noisy-tenant` scenario (007 SC-010): two tenants on one node the scenario starts in a process
//! of its own (D4) with authentication. `noisy` has a cap of 100 commands per second and a `maxTimeMS` of 30 s, and
//! tries as many commands as its connections can send; `quiet` has no cap and reads one document
//! at a time. The scenario times the reads of `quiet` alone, then while `noisy` sends cheap
//! commands, then while it sends aggregations and `$where`; the p99 of `quiet` with `noisy` may
//! be at most 10 % over the p99 alone. Last it runs the `reference` load as `quiet`, without
//! limits and then with limits it never reaches, to time what checking them costs: each p99 at
//! most 3 % over the one without limits, and the throughput at least the one without over 1.03
//! (the budget of the plan of 007 for the token bucket, the concurrency count and the quota).

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use teseladb_auth::scram::{ClientSession, Mechanism};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::node::NodeConfig;
use teseladb_server::procs::{NodeArgs, NodeProc};
use teseladb_types::Histogram;

use super::{DataDir, at};
use crate::cli::Args;
use crate::client::{Client, context, debug, panicked};
use crate::report::{Report, Target};

/// SC-010: the p99 of `quiet` with `noisy` at most 10 % over the p99 alone.
const P99_RATIO: f64 = 1.10;
/// The plan of 007: checking the limits that a tenant does not reach costs at most 3 % of p99.
const LIMITS_P99_RATIO: f64 = 1.03;
/// The cap of `noisy` in commands per second, and its `maxTimeMS` (SC-010).
const NOISY_OPS: &str = "100";
const NOISY_MAX_TIME_MS: &str = "30000";
/// Limits of `quiet` for the `reference` load that it never reaches: what is timed is checking
/// them on each command.
const UNREACHED_LIMITS: [&str; 18] = [
    "--storage-bytes",
    "1099511627776",
    "--max-connections",
    "10000",
    "--ops-per-second",
    "10000000",
    "--max-time-ms",
    NOISY_MAX_TIME_MS,
    "--max-concurrent-ops",
    "10000",
    "--max-write-bytes-per-second",
    "1099511627776",
    "--max-databases",
    "10000",
    "--max-collections",
    "10000",
    "--max-search-indexes",
    "10000",
];
/// The kinds of operation of the `reference` load whose p99 is compared.
const REFERENCE_KINDS: [&str; 4] = ["write", "read", "query", "search"];
/// The connections of `noisy`, each sending without a pause.
const NOISY_CONNECTIONS: usize = 8;
/// The cores of the node.
const CORES: u16 = 2;
/// The documents each tenant reads.
const DOCS: i32 = 1000;
/// The turns of the loop of the `$where` of `noisy`: a few milliseconds of one core for each
/// document it reads.
const WHERE_TURNS: u32 = 200_000;
/// The documents each `$where` of `noisy` reads.
const WHERE_DOCS: i32 = 3;
const READY: Duration = Duration::from_secs(60);
const ADMIN: &str = "admin";
const DB: &str = "app";
const COLL: &str = "c";
const NOISY: &str = "noisy";
const QUIET: &str = "quiet";
const ROOT: &str = "root";
const ROOT_PASSWORD: &str = "bench-root-pw-31";
const PASSWORD: &str = "bench-tenant-pw-31";

/// What `noisy` sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Noise {
    /// A point read.
    Cheap,
    /// An aggregation over the collection and a `find` with `$where`, in turns.
    Heavy,
}

/// The recipe of this scenario's node ([`crate::nodes`]).
pub const RECIPE: &str = "noisy-tenant";

/// The configuration of the node of [`RECIPE`]: [`CORES`] cores, authentication with the root
/// user [`ROOT`], and an administration port.
///
/// # Errors
///
/// When the root user cannot be made.
pub fn node_config(_a: &NodeArgs) -> Result<NodeConfig, String> {
    let scratch = DataDir::new(None, "noisy-tenant-root")?;
    let root_user = root_user(&scratch)?;
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        auth: true,
        ..ClientConfig::new(Vec::new())
    };
    Ok(NodeConfig {
        cores: Some(CORES),
        client: Some(client),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        root_user: Some(root_user),
        ..NodeConfig::default()
    })
}

/// The node, in a process of its own (D4), with its two tenants and their users.
struct Cluster {
    node: NodeProc,
}

impl Cluster {
    fn start() -> Result<Cluster, String> {
        let node = crate::nodes::start(RECIPE, &[], None, &[], READY, "the node did not start")?;
        let cluster = Cluster { node };
        for tenant in [NOISY, QUIET] {
            cluster.admin(&["tenant", "create", tenant])?;
        }
        cluster.admin(&[
            "tenant",
            "limits",
            NOISY,
            "--ops-per-second",
            NOISY_OPS,
            "--max-time-ms",
            NOISY_MAX_TIME_MS,
        ])?;
        let mut root = cluster.login(ROOT, ROOT_PASSWORD)?;
        for tenant in [NOISY, QUIET] {
            let create = Document::new()
                .with("createUser", tenant)
                .with("pwd", PASSWORD)
                .with("roles", Value::Array(vec![Value::from("readWriteAnyDatabase")]))
                .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
                .with("$tenant", tenant);
            root.command(ADMIN, create)?;
            let docs = (0..DOCS)
                .map(|i| Value::Document(Document::new().with("_id", i).with("n", i)))
                .collect();
            let insert = Document::new()
                .with("insert", COLL)
                .with("documents", Value::Array(docs))
                .with("$tenant", tenant);
            root.until_ready(DB, insert)?;
        }
        Ok(cluster)
    }

    /// `teseladb admin --node <administration port> <args>`.
    fn admin(&self, args: &[&str]) -> Result<(), String> {
        let at = self.node.admin_addr().ok_or("the node has no administration port")?.to_string();
        let argv: Vec<String> =
            ["admin", "--node", &at].iter().chain(args).map(|s| (*s).to_owned()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        match teseladb_server::run(&argv, &mut out, &mut err, &|| true) {
            0 => Ok(()),
            _ => Err(format!("{args:?}: {}", String::from_utf8_lossy(&err))),
        }
    }

    /// A connection signed in as `user` with SCRAM-SHA-256.
    fn login(&self, user: &str, password: &str) -> Result<Client, String> {
        login_at(self.addr()?, user, password)
    }

    /// The address of the node's clients' port.
    fn addr(&self) -> Result<SocketAddr, String> {
        Ok(*self.node.client_addrs().first().ok_or("the node has no clients' port")?)
    }
}

/// A connection to `addr` signed in as `user` with SCRAM-SHA-256.
fn login_at(addr: SocketAddr, user: &str, password: &str) -> Result<Client, String> {
    let mut c = Client::connect(addr)?;
    let mech = Mechanism::ScramSha256;
    let prepared = mech.prepare_password(user, password, false).map_err(debug)?;
    let (mut s, first) = ClientSession::start(mech, user, &prepared, "noisy-tenant-nonce");
    let bin = |bytes: Vec<u8>| Value::Binary { subtype: 0, bytes };
    let start = Document::new()
        .with("saslStart", 1)
        .with("mechanism", mech.name())
        .with("payload", bin(first))
        .with("options", Document::new().with("skipEmptyExchange", true));
    let r = reply(&c.command(ADMIN, start)?)?;
    let Some(Value::Binary { bytes, .. }) = r.get("payload") else {
        return Err(format!("no payload in {r:?}"));
    };
    let fin = s.respond(bytes).map_err(debug)?;
    let next =
        Document::new().with("saslContinue", 1).with("conversationId", 1).with("payload", bin(fin));
    c.command(ADMIN, next)?;
    Ok(c)
}

/// The `root` user of a credentials file in `dir`, as a first start creates it (013 FR-004).
fn root_user(dir: &DataDir) -> Result<Vec<u8>, String> {
    let file = dir.path.join(teseladb_server::pki::ROOT_CREDENTIALS);
    std::fs::write(&file, format!("user={ROOT}\npassword={ROOT_PASSWORD}\n")).map_err(at(&file))?;
    let mut random = |b: &mut [u8]| {
        b.fill(7);
        Ok(())
    };
    let user = teseladb_server::pki::root_user(&dir.path, &mut random).map_err(debug)?;
    std::fs::remove_file(&file).map_err(at(&file))?;
    user.ok_or_else(|| "no root user in the credentials".to_owned())
}

fn reply(bytes: &[u8]) -> Result<Document, String> {
    Ok(Document::from(RawDoc::from_bytes(bytes).map_err(debug)?))
}

/// The point read of `quiet`, the `n`-th of the run.
fn point_read(n: u64) -> Document {
    let id = i32::try_from(n % u64::from(DOCS.unsigned_abs())).unwrap_or_default();
    Document::new().with("find", COLL).with("filter", Document::new().with("_id", id))
}

/// The `n`-th command of `noisy`.
fn noise(kind: Noise, n: u64) -> Document {
    match kind {
        Noise::Cheap => point_read(n),
        Noise::Heavy if n.is_multiple_of(2) => {
            let modulo = Value::Array(vec![Value::from("$n"), Value::Int32(7)]);
            let group = Document::new()
                .with("_id", Document::new().with("$mod", modulo))
                .with("total", Document::new().with("$sum", "$n"));
            let stage = Value::Document(Document::new().with("$group", group));
            Document::new()
                .with("aggregate", COLL)
                .with("pipeline", Value::Array(vec![stage]))
                .with("cursor", Document::new())
        }
        Noise::Heavy => {
            let js = format!(
                "function() {{ for (var i = 0; i < {WHERE_TURNS}; i++) {{}} return true; }}"
            );
            // The range keeps the call to three documents, whatever order the scan reads them in.
            let first = Document::new().with("$lt", WHERE_DOCS);
            let filter = Document::new().with("_id", first).with("$where", js);
            Document::new().with("find", COLL).with("filter", filter)
        }
    }
}

/// What the connections of `noisy` sent and what the node served them.
#[derive(Default)]
struct Counts {
    sent: AtomicU64,
    served: AtomicU64,
}

/// The connections of `noisy` sending `kind` until `stop`.
fn start_noise(
    cluster: &Cluster,
    kind: Noise,
    stop: &Arc<AtomicBool>,
    counts: &Arc<Counts>,
) -> Result<Vec<JoinHandle<Result<(), String>>>, String> {
    let mut threads = Vec::with_capacity(NOISY_CONNECTIONS);
    for _ in 0..NOISY_CONNECTIONS {
        let mut c = cluster.login(NOISY, PASSWORD)?;
        let (stop, counts) = (stop.clone(), counts.clone());
        threads.push(std::thread::spawn(move || {
            let mut n = 0u64;
            while !stop.load(Ordering::Relaxed) {
                let r = c.send(DB, noise(kind, n)).map_err(context(format!("noisy {kind:?}")))?;
                counts.sent.fetch_add(1, Ordering::Relaxed);
                if crate::client::check(&r).is_ok() {
                    counts.served.fetch_add(1, Ordering::Relaxed);
                }
                n += 1;
            }
            Ok(())
        }));
    }
    Ok(threads)
}

/// The point reads of `quiet` for about the run's duration, each timed alone.
fn quiet_reads(args: &Args, quiet: &mut Client) -> Result<Histogram, String> {
    let mut n = 0u64;
    super::wire::each(args.warmup, args.duration, || {
        n += 1;
        quiet.command(DB, point_read(n)).map(drop)
    })
}

/// The reads of `quiet` while `noisy` sends `kind`: their latencies, and what `noisy` sent and
/// was served per second.
fn with_noise(
    args: &Args,
    cluster: &Cluster,
    quiet: &mut Client,
    kind: Noise,
) -> Result<(Histogram, f64, f64), String> {
    let stop = Arc::new(AtomicBool::new(false));
    let counts = Arc::new(Counts::default());
    let threads = start_noise(cluster, kind, &stop, &counts)?;
    let start = Instant::now();
    let h = quiet_reads(args, quiet);
    let secs = start.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    for t in threads {
        t.join().map_err(panicked("a connection of noisy"))??;
    }
    let per_second = |c: &AtomicU64| c.load(Ordering::Relaxed) as f64 / secs;
    Ok((h?, per_second(&counts.sent), per_second(&counts.served)))
}

/// The p99 of `h` over the p99 of `alone`.
fn ratio(h: &Histogram, alone: &Histogram) -> f64 {
    h.percentile(99.0) as f64 / alone.percentile(99.0).max(1) as f64
}

/// The value of metric `name` of `r`.
fn metric(r: &Report, name: &str) -> Result<f64, String> {
    r.metrics
        .iter()
        .find(|m| m.name == name)
        .map(|m| m.value)
        .ok_or_else(|| format!("no {name} in the reference load"))
}

/// The `reference` load as `quiet`, without limits and then with [`UNREACHED_LIMITS`].
fn reference_with_limits(
    args: &Args,
    cluster: &Cluster,
    report: &mut Report,
) -> Result<(), String> {
    let addr = cluster.addr()?;
    let login = || login_at(addr, QUIET, PASSWORD);
    let free =
        super::reference::measure(args, &login).map_err(context("reference without limits"))?;
    let mut opts = vec!["tenant", "limits", QUIET];
    opts.extend(UNREACHED_LIMITS);
    cluster.admin(&opts)?;
    let held = super::reference::measure(args, &login).map_err(context("reference with limits"))?;
    for kind in REFERENCE_KINDS {
        let p99 = format!("{kind}.p99");
        let (alone, limited) = (metric(&free, &p99)?, metric(&held, &p99)?);
        report.push(&format!("reference.free.{p99}"), "ns", alone, None);
        report.push(&format!("reference.limited.{p99}"), "ns", limited, None);
        let r = limited / alone.max(1.0);
        report.push(
            &format!("reference.{kind}.p99_ratio"),
            "ratio",
            r,
            Some(Target::AtMost(LIMITS_P99_RATIO)),
        );
    }
    let (alone, limited) = (metric(&free, "throughput")?, metric(&held, "throughput")?);
    report.push("reference.free.throughput", "op/s", alone, None);
    report.push("reference.limited.throughput", "op/s", limited, None);
    let r = limited / alone.max(f64::MIN_POSITIVE);
    report.push(
        "reference.throughput_ratio",
        "ratio",
        r,
        Some(Target::AtLeast(1.0 / LIMITS_P99_RATIO)),
    );
    for (name, r) in [("free", &free), ("limited", &held)] {
        report.push(
            &format!("reference.{name}.errors"),
            "op",
            metric(r, "errors")?,
            Some(Target::AtMost(0.0)),
        );
    }
    Ok(())
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    if args.target.is_some() {
        return Err("noisy-tenant measures a node it starts; it takes no --target".into());
    }
    let cluster = Cluster::start()?;
    let mut quiet = cluster.login(QUIET, PASSWORD)?;
    let alone = quiet_reads(args, &mut quiet).map_err(context("quiet alone"))?;
    let mut report = Report::new("noisy-tenant");
    report.latency("quiet.alone", &alone, None, None);
    let cap = NOISY_OPS.parse::<f64>().map_err(debug)? * P99_RATIO;
    for (kind, name) in [(Noise::Cheap, "cheap"), (Noise::Heavy, "heavy")] {
        let (h, sent, served) =
            with_noise(args, &cluster, &mut quiet, kind).map_err(context(name))?;
        report.latency(&format!("quiet.{name}"), &h, None, None);
        let r = ratio(&h, &alone);
        report.push(
            &format!("quiet.{name}.p99_ratio"),
            "ratio",
            r,
            Some(Target::AtMost(P99_RATIO)),
        );
        report.push(&format!("noisy.{name}.sent"), "ops/s", sent, None);
        report.push(&format!("noisy.{name}.served"), "ops/s", served, Some(Target::AtMost(cap)));
    }
    reference_with_limits(args, &cluster, &mut report)?;
    report.notes.push(format!(
        "one node of {CORES} cores; noisy capped at {NOISY_OPS} commands per second and \
         maxTimeMS {NOISY_MAX_TIME_MS} with {NOISY_CONNECTIONS} connections; quiet reads one \
         document at a time"
    ));
    drop(quiet);
    drop(cluster.node.stop());
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_short_run_measures_quiet_alone_and_with_each_noise() {
        let args = Args {
            scenario: "noisy-tenant".into(),
            duration: Duration::from_millis(300),
            warmup: Duration::from_millis(20),
            ..Args::default()
        };
        let r = run(&args).unwrap();
        let names = [
            "quiet.alone.p99",
            "quiet.cheap.p99_ratio",
            "quiet.heavy.p99_ratio",
            "reference.read.p99_ratio",
            "reference.search.p99_ratio",
            "reference.throughput_ratio",
        ];
        for name in names {
            assert!(r.metrics.iter().any(|m| m.name == name), "{name} in {r:?}");
        }
        // A run this short fits in the burst of the cap: the cap itself is measured by the
        // scenario's 10 s windows and by `tests/tenant_rate_limits.rs` of the server.
        let sent = r.metrics.iter().find(|m| m.name == "noisy.heavy.sent").unwrap();
        assert!(sent.value > 0.0, "{r:?}");
        // The reference load ran as `quiet`, with and without limits, without errors.
        for name in ["reference.free.errors", "reference.limited.errors"] {
            assert_eq!(metric(&r, name).unwrap(), 0.0, "{name} in {r:?}");
        }
        assert!(metric(&r, "reference.limited.throughput").unwrap() > 0.0, "{r:?}");
    }

    #[test]
    fn a_target_is_refused() {
        let args = Args { target: Some("h:1".into()), ..Args::default() };
        assert!(run(&args).unwrap_err().contains("no --target"));
    }

    #[test]
    fn the_heavy_noise_takes_turns() {
        assert!(noise(Noise::Heavy, 0).get("aggregate").is_some());
        assert!(noise(Noise::Heavy, 1).get("find").is_some());
        assert_eq!(noise(Noise::Cheap, 5), point_read(5));
    }
}
