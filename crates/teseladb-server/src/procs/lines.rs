//! The lines between a node process and its parent: views, statuses, routes, write concerns and
//! errors as `key=value` words, bytes in hexadecimal.

use super::*;

/// The range errors without fields, which cross a line by name.
pub(super) const RANGE_ERRORS: &[RangeError] = &[
    RangeError::Timeout,
    RangeError::Overloaded,
    RangeError::OutOfRange,
    RangeError::Unsatisfiable,
    RangeError::TooLarge,
    RangeError::Unavailable,
    RangeError::SnapshotTooOld,
];

pub(super) fn data_error_text(e: &DataError) -> String {
    let code = match e {
        DataError::NoAnswer => "noanswer".to_owned(),
        DataError::Range(RangeError::NotLeader { hint }) => format!("notleader:{}", hint.0),
        DataError::Range(r) if RANGE_ERRORS.contains(r) => format!("range:{r:?}"),
        DataError::Range(_) | DataError::Unexpected(_) => "other".to_owned(),
    };
    format!("err {code} {e:?}")
}

pub(super) fn parse_data_error(line: &str) -> DataError {
    let mut w = line.splitn(3, ' ');
    let (Some("err"), Some(code)) = (w.next(), w.next()) else {
        return DataError::Unexpected(line.to_owned());
    };
    if code == "noanswer" {
        return DataError::NoAnswer;
    }
    if let Some(hint) = code.strip_prefix("notleader:").and_then(|h| h.parse().ok()) {
        return DataError::Range(RangeError::NotLeader { hint: NodeId(hint) });
    }
    let named = code.strip_prefix("range:").and_then(|n| {
        RANGE_ERRORS.iter().find(|r| format!("{r:?}") == n).cloned().map(DataError::Range)
    });
    named.unwrap_or_else(|| DataError::Unexpected(w.next().unwrap_or(line).to_owned()))
}

pub(super) fn wc_text(wc: WriteConcern) -> String {
    let w = match wc.w {
        W::Zero => "0".to_owned(),
        W::Nodes(n) => n.to_string(),
        W::Majority => "majority".to_owned(),
    };
    format!("{w} {}", u8::from(wc.j))
}

pub(super) fn parse_wc(w: &str, j: &str) -> Option<WriteConcern> {
    let w = match w {
        "majority" => W::Majority,
        "0" => W::Zero,
        n => W::Nodes(n.parse().ok()?),
    };
    Some(WriteConcern { w, j: j == "1" })
}

pub(super) fn hex(b: &[u8]) -> String {
    b.iter().fold(String::with_capacity(b.len() * 2), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

pub(super) fn unhex(s: &str) -> Option<Vec<u8>> {
    if s == NONE {
        return Some(Vec::new());
    }
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub(super) fn bytes_text(b: &[u8]) -> String {
    if b.is_empty() { NONE.to_owned() } else { hex(b) }
}

pub(super) fn addrs_text(a: &[SocketAddr]) -> String {
    if a.is_empty() {
        return NONE.to_owned();
    }
    a.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
}

pub(super) fn parse_addrs(s: &str) -> Result<Vec<SocketAddr>, String> {
    if s == NONE {
        return Ok(Vec::new());
    }
    s.split(',').map(|a| a.parse().map_err(|e| format!("{a}: {e}"))).collect()
}

pub(super) fn ids_text(ids: &[NodeId]) -> String {
    if ids.is_empty() {
        return NONE.to_owned();
    }
    ids.iter().map(|n| n.0.to_string()).collect::<Vec<_>>().join(",")
}

pub(super) fn parse_ids(s: &str) -> Option<Vec<NodeId>> {
    if s == NONE {
        return Some(Vec::new());
    }
    s.split(',').map(|n| n.parse().ok().map(NodeId)).collect()
}

pub(super) const ROLES: &[Role] =
    &[Role::Follower, Role::PreCandidate, Role::Candidate, Role::Leader, Role::Learner];

pub(super) fn role_text(r: Option<Role>) -> String {
    r.map_or_else(|| NONE.to_owned(), |r| format!("{r:?}"))
}

pub(super) fn parse_role(s: &str) -> Option<Option<Role>> {
    if s == NONE {
        return Some(None);
    }
    ROLES.iter().copied().find(|r| format!("{r:?}") == s).map(Some)
}

pub(super) const STATES: &[NodeState] =
    &[NodeState::Active, NodeState::Draining, NodeState::Removed];

pub(super) fn opt_text<T: ToString>(v: Option<T>) -> String {
    v.map_or_else(|| NONE.to_owned(), |v| v.to_string())
}

/// `key=value` fields of a line after its first word.
pub(super) fn fields(line: &str) -> Option<std::collections::HashMap<&str, &str>> {
    let mut words = line.split_whitespace();
    (words.next()? == "ok").then(|| words.filter_map(|w| w.split_once('=')).collect())
}

pub(super) fn view_text(v: &Group0View) -> String {
    let states: Vec<String> = v
        .node_states
        .iter()
        .map(|(n, s)| {
            let i = STATES.iter().position(|x| x == s).unwrap_or(0);
            format!("{}:{i}", n.0)
        })
        .collect();
    format!(
        "ok bootstrapped={} cluster={} role={} leader={} voters={} learners={} nodes={} \
         states={} addrs_current={} root_pending={} range={}",
        u8::from(v.bootstrapped),
        hex(&v.cluster_id),
        role_text(v.role),
        v.leader.0,
        ids_text(&v.voters),
        ids_text(&v.learners),
        ids_text(&v.nodes),
        if states.is_empty() { NONE.to_owned() } else { states.join(",") },
        u8::from(v.addrs_current),
        u8::from(v.root_pending),
        v.data_range.as_ref().map_or_else(|| NONE.to_owned(), range_text)
    )
}

pub(super) fn parse_view(line: &str) -> Option<Group0View> {
    let f = fields(line)?;
    let states = match *f.get("states")? {
        NONE => Vec::new(),
        s => s
            .split(',')
            .map(|p| {
                let (n, i) = p.split_once(':')?;
                Some((NodeId(n.parse().ok()?), *STATES.get(i.parse::<usize>().ok()?)?))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(Group0View {
        bootstrapped: *f.get("bootstrapped")? == "1",
        cluster_id: unhex(f.get("cluster")?)?.try_into().ok()?,
        role: parse_role(f.get("role")?)?,
        leader: NodeId(f.get("leader")?.parse().ok()?),
        voters: parse_ids(f.get("voters")?)?,
        learners: parse_ids(f.get("learners")?)?,
        nodes: parse_ids(f.get("nodes")?)?,
        node_states: states,
        addrs_current: *f.get("addrs_current")? == "1",
        root_pending: *f.get("root_pending")? == "1",
        data_range: match *f.get("range")? {
            NONE => None,
            r => Some(parse_range(r)?),
        },
    })
}

pub(super) fn range_text(d: &RangeDesc) -> String {
    let replicas: Vec<String> = d
        .replicas
        .iter()
        .map(|(n, r)| format!("{}:{}", n.0, u8::from(*r == ReplicaRole::Learner)))
        .collect();
    format!(
        "{}/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}/{}",
        d.group.0,
        bytes_text(&d.start),
        bytes_text(&d.end),
        d.core,
        if replicas.is_empty() { NONE.to_owned() } else { replicas.join(",") },
        d.replica_epoch,
        d.generation,
        opt_text(d.leader_hint.map(|n| n.0)),
        d.leader_term,
        d.pending_split.as_deref().map_or_else(|| NONE.to_owned(), |p| format!("x{}", hex(p))),
        u8::from(d.settling),
        d.moved_ms
    )
}

pub(super) fn parse_opt_u64(s: &str) -> Option<Option<u64>> {
    if s == NONE { Some(None) } else { s.parse().ok().map(Some) }
}

pub(super) fn parse_range(s: &str) -> Option<RangeDesc> {
    let p: Vec<&str> = s.split('/').collect();
    let [g, start, end, core, replicas, epoch, generation, hint, term, pending, settling, moved] =
        p.as_slice()
    else {
        return None;
    };
    let replicas = match *replicas {
        NONE => Vec::new(),
        r => r
            .split(',')
            .map(|x| {
                let (n, l) = x.split_once(':')?;
                let role = if l == "1" { ReplicaRole::Learner } else { ReplicaRole::Voter };
                Some((NodeId(n.parse().ok()?), role))
            })
            .collect::<Option<Vec<_>>>()?,
    };
    Some(RangeDesc {
        group: GroupId(g.parse().ok()?),
        start: unhex(start)?,
        end: unhex(end)?,
        core: core.parse().ok()?,
        replicas,
        replica_epoch: epoch.parse().ok()?,
        generation: generation.parse().ok()?,
        leader_hint: parse_opt_u64(hint)?.map(NodeId),
        leader_term: term.parse().ok()?,
        pending_split: match *pending {
            NONE => None,
            x => Some(unhex(x.strip_prefix('x')?)?),
        },
        settling: *settling == "1",
        moved_ms: moved.parse().ok()?,
    })
}

pub(super) fn status_text(s: &DataStatus) -> String {
    format!(
        "ok role={} leader={} term={} voters={} learners={} applied={} flushed={} \
         waiting_data={} ingested={} exports={} start={} end={} rebuilt={} rebuilding={}",
        role_text(s.role),
        s.leader.0,
        s.term,
        ids_text(&s.voters),
        ids_text(&s.learners),
        s.applied,
        s.flushed,
        u8::from(s.waiting_data),
        s.ingested,
        s.exports,
        bytes_text(&s.bounds.0),
        bytes_text(&s.bounds.1),
        s.rebuilt,
        u8::from(s.rebuilding)
    )
}

pub(super) fn parse_status(line: &str) -> Option<DataStatus> {
    let f = fields(line)?;
    let n = |k: &str| f.get(k)?.parse::<u64>().ok();
    Some(DataStatus {
        role: parse_role(f.get("role")?)?,
        leader: NodeId(n("leader")?),
        term: n("term")?,
        voters: parse_ids(f.get("voters")?)?,
        learners: parse_ids(f.get("learners")?)?,
        applied: n("applied")?,
        flushed: n("flushed")?,
        waiting_data: *f.get("waiting_data")? == "1",
        ingested: n("ingested")?,
        exports: f.get("exports")?.parse().ok()?,
        bounds: (unhex(f.get("start")?)?, unhex(f.get("end")?)?),
        rebuilt: n("rebuilt")?,
        rebuilding: *f.get("rebuilding")? == "1",
    })
}

pub(super) fn route_text(r: &RouteEntry) -> String {
    format!(
        "{}/{}/{}/{}/{}/{}/{}/{}/{}",
        r.group.0,
        bytes_text(&r.start),
        bytes_text(&r.end),
        r.core,
        ids_text(&r.voters),
        r.generation,
        r.replica_epoch,
        opt_text(r.leader_hint.map(|n| n.0)),
        r.moved_ms
    )
}

pub(super) fn parse_route(s: &str) -> Option<RouteEntry> {
    let p: Vec<&str> = s.split('/').collect();
    let [g, start, end, core, voters, generation, epoch, hint, moved] = p.as_slice() else {
        return None;
    };
    Some(RouteEntry {
        group: GroupId(g.parse().ok()?),
        start: unhex(start)?,
        end: unhex(end)?,
        core: core.parse().ok()?,
        voters: parse_ids(voters)?,
        generation: generation.parse().ok()?,
        replica_epoch: epoch.parse().ok()?,
        leader_hint: parse_opt_u64(hint)?.map(NodeId),
        moved_ms: moved.parse().ok()?,
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn views_statuses_and_routes_cross_a_line_unchanged() {
        let range = RangeDesc {
            group: GroupId(4),
            start: vec![1, 2],
            end: Vec::new(),
            core: 1,
            replicas: vec![(NodeId(1), ReplicaRole::Voter), (NodeId(3), ReplicaRole::Learner)],
            replica_epoch: 7,
            generation: 2,
            leader_hint: Some(NodeId(3)),
            leader_term: 9,
            pending_split: Some(Vec::new()),
            settling: true,
            moved_ms: 12,
        };
        let v = Group0View {
            bootstrapped: true,
            cluster_id: [5; 16],
            role: Some(Role::Leader),
            leader: NodeId(1),
            voters: vec![NodeId(1), NodeId(2)],
            learners: vec![NodeId(3)],
            nodes: vec![NodeId(1), NodeId(2), NodeId(3)],
            node_states: vec![(NodeId(2), NodeState::Draining)],
            addrs_current: true,
            data_range: Some(range),
            root_pending: false,
        };
        assert_eq!(parse_view(&view_text(&v)), Some(v.clone()));
        let empty = Group0View::default();
        assert_eq!(parse_view(&view_text(&empty)), Some(empty));
        let s = DataStatus {
            role: None,
            leader: NodeId(2),
            term: 3,
            voters: vec![NodeId(2)],
            learners: Vec::new(),
            applied: 10,
            flushed: 8,
            waiting_data: true,
            ingested: 1,
            exports: 2,
            bounds: (Vec::new(), vec![0xff]),
            rebuilt: 4,
            rebuilding: true,
        };
        assert_eq!(parse_status(&status_text(&s)), Some(s));
        let r = RouteEntry {
            group: GroupId(2),
            start: vec![0],
            end: vec![9, 9],
            core: 0,
            voters: vec![NodeId(1)],
            generation: 3,
            replica_epoch: 4,
            leader_hint: None,
            moved_ms: 5,
        };
        assert_eq!(parse_route(&route_text(&r)), Some(r));
    }

    #[test]
    fn write_concerns_and_data_errors_cross_a_line() {
        for w in [W::Zero, W::Nodes(2), W::Majority] {
            for j in [false, true] {
                let wc = WriteConcern { w, j };
                let text = wc_text(wc);
                let (w, j) = text.split_once(' ').unwrap();
                assert_eq!(parse_wc(w, j), Some(wc));
            }
        }
        assert_eq!(parse_wc("x", "1"), None);
        let mut errors =
            vec![DataError::NoAnswer, DataError::Range(RangeError::NotLeader { hint: NodeId(3) })];
        errors.extend(RANGE_ERRORS.iter().cloned().map(DataError::Range));
        for e in errors {
            assert_eq!(parse_data_error(&data_error_text(&e)), e);
        }
        let other = DataError::Unexpected("a reply of another kind".into());
        let text = data_error_text(&other);
        assert_eq!(parse_data_error(&text), DataError::Unexpected(format!("{other:?}")));
        assert_eq!(parse_data_error("none"), DataError::Unexpected("none".into()));
    }

    #[test]
    fn node_arguments_cross_a_command_line_unchanged() {
        let a = NodeArgs {
            recipe: "plain".into(),
            join: vec!["127.0.0.1:1".into(), "127.0.0.1:2".into()],
            dir: Some(PathBuf::from("/tmp/x")),
            params: vec!["split=10".into()],
            ports: Some(Ports {
                peer: "127.0.0.1:3".parse().unwrap(),
                clients: vec!["127.0.0.1:4".parse().unwrap()],
                admin: None,
                http: Vec::new(),
            }),
        };
        let args = a.to_args();
        assert_eq!(args[0], NODE_PROCESS);
        assert_eq!(NodeArgs::parse(&args[1..]), Ok(a.clone()));
        assert_eq!(a.param("split"), Some("10"));
        assert!(NodeArgs::parse(&[]).is_err());
        assert!(NodeArgs::parse(&["r".into(), "--dir".into()]).is_err());
        assert!(NodeArgs::parse(&["r".into(), "--x".into(), "1".into()]).is_err());
        assert!(NodeArgs::parse(&["r".into(), PORTS.into(), "1".into()]).is_err());
    }

    #[test]
    fn start_errors_come_back_with_their_kind() {
        let e = NodeError::Io(io::Error::new(io::ErrorKind::AddrInUse, "clients' port: busy"));
        let line = error_line(&e);
        let rest = line.strip_prefix("error ").unwrap();
        let (kind, message) = rest.split_once(' ').unwrap();
        let back = StartError { kind: kind.into(), message: message.into(), log: String::new() };
        let NodeError::Io(io) = back.node_error() else { panic!("{line}") };
        assert_eq!((io.kind(), io.to_string()), (io::ErrorKind::AddrInUse, e.to_string()));
        for e in
            [NodeError::Locked, NodeError::Join("j".into()), NodeError::DataFormat { found: 3 }]
        {
            let line = error_line(&e);
            let rest = line.strip_prefix("error ").unwrap();
            let (kind, message) = rest.split_once(' ').unwrap_or((rest, ""));
            let back =
                StartError { kind: kind.into(), message: message.into(), log: String::new() };
            assert_eq!(back.node_error().to_string(), e.to_string());
        }
        assert_eq!(parent::error_kind("Nothing"), io::ErrorKind::Other);
    }
}
