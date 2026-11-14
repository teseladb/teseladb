//! Entry size: an entry holds up to 64 MiB, travels alone in its `AppendReq` and
//! reaches every replica whole; a larger one is refused, and no configuration raises the limit.

mod common;

use common::*;
use teseladb_raft::{EntryKind, MAX_ENTRY_BYTES, ProposeError, RaftConfig};

/// The data a node applied in its last normal entry.
fn last_applied(net: &Net, n: teseladb_raft::NodeId) -> Option<teseladb_types::Bytes> {
    net.nodes[&n]
        .applied
        .iter()
        .rev()
        .find(|e| e.kind == EntryKind::Normal && !e.data.is_empty())
        .map(|e| e.data.clone())
}

#[test]
fn the_default_limit_of_an_entry_is_64_mib() {
    assert_eq!(MAX_ENTRY_BYTES, 64 << 20);
    assert_eq!(RaftConfig::new(id(1)).max_entry_bytes, MAX_ENTRY_BYTES);
}

#[test]
fn an_entry_of_64_mib_replicates_whole_and_a_larger_one_is_refused() {
    let mut net = Net::new(3, 0, 23);
    let l = net.elect(200);
    let big: Vec<u8> = (0..MAX_ENTRY_BYTES).map(|i| (i % 251).to_le_bytes()[0]).collect();
    assert_eq!(net.propose(l, &vec![0; MAX_ENTRY_BYTES + 1]), Err(ProposeError::TooLarge));
    net.propose(l, &big).expect("propose succeeds");
    // Small entries behind it go on replicating in the following messages.
    net.propose(l, b"after").expect("propose succeeds");
    net.ticks(10);
    for n in 1..=3 {
        let applied = &net.nodes[&id(n)].applied;
        let datas: Vec<usize> = applied
            .iter()
            .filter(|e| e.kind == EntryKind::Normal && !e.data.is_empty())
            .map(|e| e.data.len())
            .collect();
        assert_eq!(datas, vec![MAX_ENTRY_BYTES, 5], "node {n}");
        let whole =
            applied.iter().find(|e| e.data.len() == MAX_ENTRY_BYTES).expect("find succeeds");
        assert!(whole.data.as_ref() == big.as_slice(), "node {n} applied the same bytes");
        assert_eq!(last_applied(&net, id(n)).as_deref(), Some(&b"after"[..]));
    }
}

#[test]
fn no_configuration_takes_an_entry_or_a_message_above_64_mib() {
    let ok = RaftConfig::new(id(1));
    let entry = RaftConfig { max_entry_bytes: MAX_ENTRY_BYTES + 1, ..ok.clone() };
    assert_eq!(entry.validate(), Err("an entry or a message above 64 MiB"));
    let msg = RaftConfig { max_msg_bytes: MAX_ENTRY_BYTES + 1, ..ok.clone() };
    assert_eq!(msg.validate(), Err("an entry or a message above 64 MiB"));
    let at_limit =
        RaftConfig { max_entry_bytes: MAX_ENTRY_BYTES, max_msg_bytes: MAX_ENTRY_BYTES, ..ok };
    assert_eq!(at_limit.validate(), Ok(()));
}
