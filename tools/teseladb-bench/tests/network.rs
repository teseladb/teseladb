//! The network scenarios end to end (002 FR-018, FR-019, FR-020): YCSB and `reference` against a
//! local cluster started in the process, and the errors of `--target`.

use std::process::{Command, Output};

fn bench(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_teseladb-bench")).args(args).output().expect("run")
}

fn metrics(out: &Output) -> String {
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn ycsb_and_reference_run_against_a_local_cluster() {
    let short = ["--duration", "500ms", "--warmup", "100ms", "--format", "json"];
    let ycsb = metrics(&bench(
        &[&["--scenario", "ycsb-f", "--records", "50", "--concurrency", "2"][..], &short[..]]
            .concat(),
    ));
    for name in ["read.p50", "rmw.p99", "throughput", "\"errors\",\"unit\":\"op\",\"value\":0"] {
        assert!(ycsb.contains(name), "{name}: {ycsb}");
    }
    let open = metrics(&bench(
        &[&["--scenario", "ycsb-e", "--records", "50", "--rate", "200"][..], &short[..]].concat(),
    ));
    assert!(open.contains("scan.p50") && open.contains("200 op/s"), "{open}");
    let reference =
        metrics(&bench(&[&["--scenario", "reference", "--rate", "100"][..], &short[..]].concat()));
    assert!(reference.contains("100 op/s"), "{reference}");
    for name in ["write.p50", "write.p99", "read.p50", "read.p99", "search.p50", "search.p99"] {
        assert!(reference.contains(name), "{name}: {reference}");
    }
}

/// `vector-search` (011 T032) on a few hundred vectors: the scenario of SC-005 with a million
/// runs the same code.
#[test]
fn vector_search_runs_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "0ms", "--format", "json"];
    let out = metrics(&bench(
        &[&["--scenario", "vector-search", "--records", "300"][..], &short[..]].concat(),
    ));
    for name in ["query.p50", "recall@10"] {
        assert!(out.contains(name), "{name}: {out}");
    }
}

#[test]
fn a_target_that_is_not_there_or_not_refdb_is_an_error() {
    for (target, want) in [
        ("http://x", "refdb://host:port"),
        ("refdb://127.0.0.1:1", "127.0.0.1:1"),
        ("refdb://nohost.invalid:1", "nohost.invalid"),
    ] {
        let out = bench(&[
            "--scenario",
            "ycsb-a",
            "--records",
            "1",
            "--duration",
            "100ms",
            "--target",
            target,
        ]);
        assert_eq!(out.status.code(), Some(2), "{target}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains(want),
            "{target}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let out = bench(&["--scenario", "ycsb-g"]);
    assert_eq!(out.status.code(), Some(2));
}

/// `wire` and `crud` (006 FR-029, SC-004) against a local cluster, with their targets.
#[test]
fn wire_and_crud_run_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "50ms", "--format", "json"];
    let wire = metrics(&bench(&[&["--scenario", "wire"][..], &short[..]].concat()));
    for name in ["hello.p50", "ping.p50", "whatsmyuri.p99", "\"target\""] {
        assert!(wire.contains(name), "{name}: {wire}");
    }
    let crud = metrics(&bench(&[&["--scenario", "crud"][..], &short[..]].concat()));
    for name in ["insert.p50", "find.p50", "update.p99", "delete.p50", "1 KiB"] {
        assert!(crud.contains(name), "{name}: {crud}");
    }
}

/// `aggregate` and `sort` (010 FR-021, SC-004) against a local cluster: the query by an index
/// of equality that gives 10 documents with its target, a `$group` and the sorts.
#[test]
fn aggregate_and_sort_run_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "50ms", "--records", "200", "--format", "json"];
    let agg = metrics(&bench(&[&["--scenario", "aggregate"][..], &short[..]].concat()));
    for name in ["equality_10.p50", "group.p50", "match_group.p99", "200 documents"] {
        assert!(agg.contains(name), "{name}: {agg}");
    }
    let sort = metrics(&bench(&[&["--scenario", "sort"][..], &short[..]].concat()));
    for name in ["indexed_top10.p50", "memory_top10.p50", "aggregate_sort.p99"] {
        assert!(sort.contains(name), "{name}: {sort}");
    }
    // The harness of SC-003 (T027), over 300 documents instead of 50 GB.
    let large =
        metrics(&bench(&["--scenario", "group-large", "--records", "300", "--format", "json"]));
    for name in ["load_seconds", "group_seconds", "resident_per_core", "300 documents of 1 KiB"] {
        assert!(large.contains(name), "{name}: {large}");
    }
}

/// `timeseries` (015 FR-012, SC-003, SC-004) against a local cluster: the ingest rate, an hour of
/// one sensor with its target and the space against a collection that is not a time series.
#[test]
fn timeseries_runs_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "50ms", "--format", "json"];
    let ts = metrics(&bench(&[&["--scenario", "timeseries"][..], &short[..]].concat()));
    for name in ["ingest", "hour_of_one_sensor.p50", "space_ratio", "10 sensors"] {
        assert!(ts.contains(name), "{name}: {ts}");
    }
}

/// `tls-handshake` and `scram` (013 FR-021, SC-006) against a secure node the bench starts: the
/// handshake with its target, the login in two round trips; neither takes `--target`.
#[test]
fn tls_handshake_and_scram_run_against_a_secure_node() {
    let short = ["--duration", "200ms", "--warmup", "20ms", "--format", "json"];
    let tls = metrics(&bench(&[&["--scenario", "tls-handshake"][..], &short[..]].concat()));
    assert!(tls.contains("handshake.p50") && tls.contains("\"target\""), "{tls}");
    let scram = metrics(&bench(&[&["--scenario", "scram"][..], &short[..]].concat()));
    assert!(scram.contains("login.p50"), "{scram}");
    assert!(scram.contains("\"round_trips\",\"unit\":\"count\",\"value\":2"), "{scram}");
    for scenario in ["tls-handshake", "scram"] {
        let out = bench(&["--scenario", scenario, "--target", "refdb://127.0.0.1:1"]);
        assert_eq!(out.status.code(), Some(2), "{scenario}");
        assert!(String::from_utf8_lossy(&out.stderr).contains("takes no --target"));
    }
}

/// `txn`, `txn-readers` and `contention` (008 FR-017, SC-004, SC-005) against a local cluster:
/// the transactions of one and two ranges with their targets, the readers before and while ten
/// transactions hold their intents with the rise of their p99, and the hot documents with the
/// share of conflicts and of reads restarted for their uncertainty window (from the admin port).
#[test]
fn transaction_scenarios_run_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "50ms", "--format", "json"];
    let txn = metrics(&bench(&[&["--scenario", "txn"][..], &short[..]].concat()));
    for name in ["one_range.p50", "two_ranges.p50", "split in two", "\"target\""] {
        assert!(txn.contains(name), "{name}: {txn}");
    }
    let readers = metrics(&bench(&[&["--scenario", "txn-readers"][..], &short[..]].concat()));
    for name in ["write.p50", "read_clean.p99", "read_intents.p99", "read_intents.p99_rise"] {
        assert!(readers.contains(name), "{name}: {readers}");
    }
    let hot = metrics(&bench(
        &[&["--scenario", "contention", "--concurrency", "4"][..], &short[..]].concat(),
    ));
    for name in ["commit.p50", "conflict_aborts", "uncertainty_restarts", "throughput", "4 clients"]
    {
        assert!(hot.contains(name), "{name}: {hot}");
    }
}

/// `index-write` and `index-read` (009 FR-018, SC-002) against a local cluster: a write with three
/// indexes in the document's range and with its index entries in another range, and the read by
/// an index; `reference` queries by its index too (002 FR-020).
#[test]
fn index_write_and_read_run_against_a_local_cluster() {
    let short = ["--duration", "300ms", "--warmup", "50ms", "--records", "200", "--format", "json"];
    let write = metrics(&bench(&[&["--scenario", "index-write"][..], &short[..]].concat()));
    for name in ["same_range.p50", "other_range.p50", "other_range.p99", "\"target\""] {
        assert!(write.contains(name), "{name}: {write}");
    }
    let read = metrics(&bench(&[&["--scenario", "index-read"][..], &short[..]].concat()));
    for name in ["index_read.p50", "index_read.p99", "200 documents"] {
        assert!(read.contains(name), "{name}: {read}");
    }
    let reference =
        metrics(&bench(&[&["--scenario", "reference", "--rate", "100"][..], &short[..]].concat()));
    assert!(reference.contains("query.p50"), "{reference}");
}
