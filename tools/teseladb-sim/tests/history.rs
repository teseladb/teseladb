//! The history in the format of the contract `history-format.md` (002 FR-007): one JSON line per
//! event (`invoke`, then `ok`, `fail` or `info`), the group in the key, the write concern of the
//! writes, and a reader that pairs each invocation with its end by process.

use teseladb_sim::faults::FaultPlan;
use teseladb_sim::history::{History, OpKind, OpRecord, OpResult};
use teseladb_sim::{SimConfig, Verdict, run_seed};
use teseladb_types::Nanos;

fn rec(client: u32, op_id: u64, kind: OpKind, invoke: u64) -> OpRecord {
    OpRecord {
        client,
        op_id,
        kind,
        group: "g1".into(),
        key: "k7".into(),
        expected: None,
        new: None,
        invoke: Nanos(invoke),
        complete: None,
        result: OpResult::Unknown,
        node: 2,
        error: None,
        ended: None,
        w1: false,
    }
}

fn sample() -> History {
    let cas_ok = OpRecord {
        new: Some("a".into()),
        complete: Some(Nanos(20)),
        result: OpResult::Ok(None),
        ..rec(1, 1, OpKind::Cas, 10)
    };
    let read = OpRecord {
        complete: Some(Nanos(30)),
        result: OpResult::Ok(Some("a".into())),
        ..rec(2, 1, OpKind::Read, 25)
    };
    let cas_fail = OpRecord {
        expected: Some("z".into()),
        new: Some("b".into()),
        complete: Some(Nanos(40)),
        result: OpResult::Fail,
        error: Some("CasMismatch".into()),
        ..rec(1, 2, OpKind::Cas, 35)
    };
    let timeout = OpRecord {
        expected: Some("a".into()),
        new: Some("c".into()),
        error: Some("Timeout".into()),
        ended: Some(Nanos(5_000)),
        ..rec(3, 1, OpKind::Cas, 45)
    };
    let open = OpRecord { group: "g2".into(), ..rec(2, 2, OpKind::Read, 50) };
    History { ops: vec![cas_ok, read, cas_fail, timeout, open] }
}

#[test]
fn every_event_is_a_line_of_the_contract() {
    let text = sample().to_jsonl();
    let lines: Vec<&str> = text.lines().collect();
    // Five invocations, four ends (the last read is still in flight), in time order.
    assert_eq!(lines.len(), 9, "{text}");
    assert_eq!(
        lines[0],
        r#"{"process":1,"type":"invoke","f":"cas","key":"g1/k7","value":[null,"a"],"time":10,"node":2,"wc":{"w":"majority","j":true}}"#
    );
    assert_eq!(
        lines[1],
        r#"{"process":1,"type":"ok","f":"cas","key":"g1/k7","value":[null,"a"],"time":20,"node":2,"wc":{"w":"majority","j":true}}"#
    );
    assert_eq!(
        lines[2],
        r#"{"process":2,"type":"invoke","f":"read","key":"g1/k7","time":25,"node":2}"#
    );
    assert_eq!(
        lines[3],
        r#"{"process":2,"type":"ok","f":"read","key":"g1/k7","value":"a","time":30,"node":2}"#
    );
    assert!(lines[5].contains(r#""type":"fail""#) && lines[5].contains(r#""error":"CasMismatch""#));
    assert_eq!(
        lines[8],
        r#"{"process":3,"type":"info","f":"cas","key":"g1/k7","value":["a","c"],"time":5000,"node":2,"wc":{"w":"majority","j":true},"error":"Timeout"}"#
    );
    let times: Vec<u64> = lines
        .iter()
        .map(|l| teseladb_json::parse(l.as_bytes()).unwrap().get("time").unwrap().as_u64().unwrap())
        .collect();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
}

#[test]
fn the_reader_gives_back_the_same_history() {
    let h = sample();
    let back = History::from_jsonl(&format!("{}\n\n", h.to_jsonl())).unwrap();
    // The reader numbers each process's operations in order; the rest comes back as it was.
    assert_eq!(back, h);
}

#[test]
fn malformed_histories_say_where() {
    let invoke = r#"{"process":1,"type":"invoke","f":"read","key":"g1/k1","time":1,"node":0}"#;
    let ok = r#"{"process":1,"type":"ok","f":"read","key":"g1/k1","value":null,"time":2,"node":0}"#;
    for (text, what) in [
        ("{".to_owned(), "line 1"),
        ("{}".to_owned(), "missing `process`"),
        (ok.to_owned(), "line 1: process 1 ends an operation it did not invoke"),
        (format!("{invoke}\n{invoke}"), "line 2: process 1 invokes with another operation in flight"),
        (invoke.replace("read", "append"), "unknown `f`"),
        (invoke.replace(r#""type":"invoke","#, ""), "missing `type`"),
        (invoke.replace("invoke", "start"), "unknown `type`"),
        (invoke.replace("g1/k1", "k1"), "`key` is not <group>/<key>"),
        (format!("{invoke}\n{}", ok.replace("g1/k1", "g1/k2")), "line 2: process 1 ends another operation"),
        (format!("{invoke}\n{}", ok.replace("g1/k1", "g9/k1")), "line 2: process 1 ends another operation"),
        (format!("{invoke}\n{}", ok.replace(r#""f":"read""#, r#""f":"cas""#)), "line 2: process 1 ends another operation"),
        (invoke.replace(r#""time":1"#, r#""time":"x""#), "missing `time`"),
        // A corrupt history whose clock goes back (002 T038): an end before its invocation, and
        // an invocation before the end of the same process's previous operation.
        (
            format!("{}\n{}", invoke.replace(r#""time":1"#, r#""time":5"#), ok),
            "line 2: process 1 ends its operation before invoking it",
        ),
        (
            format!("{}\n{}\n{invoke}", invoke, ok.replace(r#""time":2"#, r#""time":3"#)),
            "line 3: process 1 invokes before its previous operation ended",
        ),
        (invoke.replace(r#""process":1"#, r#""process":99999999999"#), "`process` out of range"),
        (
            r#"{"process":1,"type":"invoke","f":"cas","key":"g1/k1","value":[1,2],"time":1,"node":0}"#
                .to_owned(),
            "`value` of a cas is [expected, new]",
        ),
    ] {
        let e = History::from_jsonl(&text).unwrap_err();
        assert!(e.contains(what), "{text}: {e}");
    }
}

#[test]
fn a_runs_history_follows_the_contract_and_reads_back_linearizable() {
    let cfg = SimConfig {
        workload: "testkv".into(),
        faults: FaultPlan::Heavy,
        history: true,
        ..SimConfig::default()
    };
    let mut given_up = 0;
    for seed in 1..=10 {
        let r = run_seed(&cfg, seed);
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        let text = r.history.expect("asked for");
        // Reading it back checks that no process invokes with another operation in flight.
        let h = History::from_jsonl(&text).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(h.ops.len() > 100, "seed {seed}: {} operations", h.ops.len());
        given_up += h.ops.iter().filter(|o| o.ended.is_some()).count();
        teseladb_sim::checkers::linearizable::check(&h).unwrap();
    }
    // Under heavy faults some operation is given up without an answer: its `info` line is read back.
    assert!(given_up > 0, "no operation was given up in ten seeds");
    // Without asking for it, and for a workload without clients, there is none.
    assert_eq!(run_seed(&SimConfig { history: false, ..cfg.clone() }, 1).history, None);
    let storage = SimConfig { workload: "storage".into(), ..cfg };
    assert_eq!(run_seed(&storage, 1).history, None);
}

#[test]
fn a_failed_cas_reads_back_as_failed_even_without_an_error() {
    // An external history (or a workload) may leave `error` empty: a `fail` of an evaluated cas
    // is still a `CasMismatch`, never a refusal that imposes nothing.
    let failed = OpRecord {
        expected: Some("x".into()),
        new: Some("y".into()),
        complete: Some(Nanos(5)),
        result: OpResult::Fail,
        ..rec(1, 1, OpKind::Cas, 1)
    };
    let h = History { ops: vec![failed.clone()] };
    let back = History::from_jsonl(&h.to_jsonl()).unwrap();
    assert_eq!(back.ops[0].result, OpResult::Fail);
    assert_eq!(back.ops[0].error.as_deref(), Some("CasMismatch"));
    // An `info` without an error has none.
    let lost = OpRecord { ended: Some(Nanos(9)), ..rec(2, 1, OpKind::Read, 3) };
    let line = History { ops: vec![lost] }.to_jsonl();
    assert!(!line.contains("error"), "{line}");
}

#[test]
fn a_lost_write_acknowledged_with_w1_is_not_a_violation_of_an_external_history() {
    // 002 T042, D-034: an external history (Docker) says `ok` for a write acknowledged with
    // `w: 1`; its leader may lose it before the majority has it, so it reads back as an outcome
    // that is not known, as the simulator records it, and a later read that misses it is no
    // violation. The same write confirmed by the majority is.
    let lines = |w: &str| {
        let write = |kind: &str, time: u32| {
            format!(
                r#"{{"process":1,"type":"{kind}","f":"cas","key":"kv/k1","value":[null,"a"],"time":{time},"node":1,"wc":{{"w":{w},"j":true}}}}"#
            )
        };
        [
            write("invoke", 10),
            write("ok", 20),
            r#"{"process":2,"type":"invoke","f":"read","key":"kv/k1","time":30,"node":2}"#
                .to_owned(),
            r#"{"process":2,"type":"ok","f":"read","key":"kv/k1","value":null,"time":40,"node":2}"#
                .to_owned(),
        ]
        .join("\n")
    };
    let w1 = History::from_jsonl(&lines("1")).unwrap();
    assert_eq!(w1.ops[0].result, OpResult::Unknown);
    assert_eq!(w1.ops[0].error.as_deref(), Some("W1Acknowledged"));
    assert_eq!(w1.ops[0].ended, Some(Nanos(20)));
    teseladb_sim::checkers::linearizable::check(&w1).unwrap();
    let majority = History::from_jsonl(&lines(r#""majority""#)).unwrap();
    assert!(teseladb_sim::checkers::linearizable::check(&majority).is_err());
    // Written back, the w: 1 write keeps its write concern and its outcome.
    let back = History::from_jsonl(&w1.to_jsonl()).unwrap();
    assert_eq!(back.ops[0].result, OpResult::Unknown);
    assert!(back.ops[0].w1);
}
