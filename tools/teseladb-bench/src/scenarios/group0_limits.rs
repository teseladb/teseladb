//! `group0-limits` scenario (007 FR-014, FR-024, SC-008, D-012): what Group 0's state machine
//! takes before the descriptors have to go to the Meta2 ranges. Group 0 starts with one range over
//! three nodes and splits its last range, one `SplitRange` and `CompleteSplit` after another, at
//! growing keys of a collection (its prefix and a 16-byte counter, the size of a document key with
//! an ObjectId), as a collection loaded in order does. Measured, on the
//! machine's clock, without the log's disk:
//!
//! - `group0-limits.splits-per-second`: completed splits the state machine applies per second;
//!   D-012 hands over at 50 per second sustained, so it has to stay well above that;
//! - `group0-limits.snapshot-bytes-per-range`: what each descriptor adds to the snapshot, and
//!   `group0-limits.ranges-at-threshold`, how many ranges fill the 64 MiB of D-012;
//! - `group0-limits.snapshot-ms` and `group0-limits.restore-ms`: encoding and restoring the
//!   snapshot at the end, scaled to 64 MiB.

use std::time::Instant;

use teseladb_keys::space::KeyBuilder;
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_meta::meta2::DEFAULT_SNAPSHOT_BYTES;
use teseladb_multiraft::StateMachine;
use teseladb_types::{GroupId, NodeId};

use crate::cli::Args;
use crate::report::{Report, Target};

/// Ranges at the end of the run.
const RANGES: u64 = 200_000;
/// D-012's rate of completed splits, which has to stay far below what Group 0 applies.
const SPLITS_PER_SECOND: f64 = 50.0;

/// Runs the scenario.
pub fn run(_args: &Args) -> Result<Report, String> {
    run_with(RANGES)
}

fn node(i: u64) -> NodeAddrs {
    NodeAddrs {
        name: format!("n{i}").into_bytes(),
        peer_addr: format!("n{i}:7421").into_bytes(),
        http_addr: format!("n{i}:8080").into_bytes(),
        native_addr: format!("n{i}:27017").into_bytes(),
        cores: 8,
    }
}

/// The `i`-th cut of collection 1.
fn key(i: u64) -> Vec<u8> {
    let mut k = KeyBuilder::collection_start(0, 1).as_bytes().to_vec();
    k.extend_from_slice(&u128::from(i).to_be_bytes());
    k
}

/// Group 0 with three nodes and one range of documents over them.
fn group0() -> Result<(Group0Sm, u64), String> {
    let mut sm = Group0Sm::default();
    let mut index = 0;
    let mut apply = |sm: &mut Group0Sm, cmd: Group0Command| {
        index += 1;
        sm.apply_command(index, &cmd)
    };
    let boot = Group0Command::Bootstrap {
        cluster_id: [1; 16],
        first_node: node(1),
        cert_fingerprint: [1; 32],
    };
    apply(&mut sm, boot);
    for i in 2..=3u64 {
        apply(&mut sm, Group0Command::AddNode { node: node(i), cert_fingerprint: [i as u8; 32] });
    }
    let replicas = vec![NodeId(1), NodeId(2), NodeId(3)];
    let create = Group0Command::CreateRange { start: Vec::new(), end: Vec::new(), replicas };
    range_of(apply(&mut sm, create), format_args!("the first range"))?;
    Ok((sm, index))
}

/// The range a command of Group 0 created or completed; any other result is an error about
/// `what`.
fn range_of(result: Group0Result, what: std::fmt::Arguments<'_>) -> Result<GroupId, String> {
    match result {
        Group0Result::Range(g) => Ok(g),
        other => Err(format!("{what}: {other:?}")),
    }
}

fn run_with(ranges: u64) -> Result<Report, String> {
    let (mut sm, mut index) = group0()?;
    // The last range (no end) is the one cut each time, at keys that grow.
    let mut last = GroupId(1);
    let t0 = Instant::now();
    for i in 1..ranges {
        let at = key(i);
        index += 1;
        let split = Group0Command::SplitRange { group: last, at: at.clone() };
        let new = range_of(sm.apply_command(index, &split), format_args!("split {i} refused"))?;
        index += 1;
        let done = Group0Command::CompleteSplit { group: last, at, at_index: index };
        if sm.apply_command(index, &done) != Group0Result::Range(new) {
            return Err(format!("split {i} not completed"));
        }
        last = new;
    }
    let splits_per_second = (ranges - 1) as f64 / t0.elapsed().as_secs_f64();
    let t1 = Instant::now();
    let snap = sm.snapshot();
    let snapshot_s = t1.elapsed().as_secs_f64();
    let t2 = Instant::now();
    let mut back = Group0Sm::default();
    back.restore(&snap).map_err(|e| format!("restore: {e:?}"))?;
    let restore_s = t2.elapsed().as_secs_f64();
    if back.ranges.len() as u64 != ranges {
        return Err(format!("{} ranges restored of {ranges}", back.ranges.len()));
    }
    let per_range = snap.len() as f64 / ranges as f64;
    let scale = DEFAULT_SNAPSHOT_BYTES as f64 / snap.len() as f64;
    let mut report = Report::new("group0-limits");
    report.push(
        "group0-limits.splits-per-second",
        "1/s",
        splits_per_second,
        Some(Target::AtLeast(SPLITS_PER_SECOND * 100.0)),
    );
    report.push("group0-limits.snapshot-bytes-per-range", "B", per_range, None);
    report.push(
        "group0-limits.ranges-at-threshold",
        "n",
        DEFAULT_SNAPSHOT_BYTES as f64 / per_range,
        None,
    );
    report.push("group0-limits.snapshot-ms", "ms", snapshot_s * scale * 1_000.0, None);
    report.push("group0-limits.restore-ms", "ms", restore_s * scale * 1_000.0, None);
    report.push("group0-limits.ranges", "n", ranges as f64, None);
    report.notes.push(
        "Group 0's state machine in this process, without the log's disk; the snapshot times are \
         scaled to 64 MiB"
            .into(),
    );
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn only_a_range_is_a_range() {
        assert_eq!(range_of(Group0Result::Range(GroupId(4)), format_args!("x")), Ok(GroupId(4)));
        let (sm, index) = group0().unwrap();
        let mut sm = sm;
        // A split of a range that does not exist is refused.
        let split = Group0Command::SplitRange { group: GroupId(99), at: key(1) };
        let e = range_of(sm.apply_command(index + 1, &split), format_args!("split 1")).unwrap_err();
        assert!(e.starts_with("split 1: "), "{e}");
    }

    #[test]
    fn a_short_run_measures_each_figure() {
        let r = run_with(500).unwrap();
        let value = |name: &str| {
            r.metrics.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}")).value
        };
        assert_eq!(value("group0-limits.ranges"), 500.0);
        let per_range = value("group0-limits.snapshot-bytes-per-range");
        assert!((50.0..1_000.0).contains(&per_range), "{per_range}");
        assert!(value("group0-limits.splits-per-second") > 0.0);
        assert!(value("group0-limits.ranges-at-threshold") > 10_000.0);
    }
}
