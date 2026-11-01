//! 007 T045: `SetSettings` (kind 51), the change of the documents of `config.settings` in Group 0
//! (FR-017): every setting of the batch changes, or none if one of them is not as it was read; a
//! value `None` removes the setting.

use teseladb_meta::command::{Group0Command, NodeAddrs, SettingChange, kind};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_meta::request::Requests;
use teseladb_types::NodeId;

fn booted() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let first = NodeAddrs {
        name: b"n1".to_vec(),
        peer_addr: b"n1:7421".to_vec(),
        http_addr: b"n1:8080".to_vec(),
        native_addr: b"n1:27017".to_vec(),
        cores: 1,
    };
    let boot = Group0Command::Bootstrap {
        cluster_id: [1; 16],
        first_node: first,
        cert_fingerprint: [1; 32],
    };
    assert_eq!(sm.apply_command(1, &boot), Group0Result::Node(NodeId(1)));
    sm
}

fn change(key: &[u8], expect: Option<&[u8]>, value: Option<&[u8]>) -> SettingChange {
    SettingChange {
        key: key.to_vec(),
        expect: expect.map(<[u8]>::to_vec),
        value: value.map(<[u8]>::to_vec),
    }
}

#[test]
fn settings_change_together_or_not_at_all() {
    let mut sm = booted();
    let set = Group0Command::SetSettings(vec![
        change(b"a", None, Some(b"1")),
        change(b"b", None, Some(b"2")),
    ]);
    assert_eq!(set.kind(), kind::SET_SETTINGS);
    assert_eq!(Group0Command::decode(&set.encode()), Ok(set.clone()));
    assert_eq!(sm.apply_command(2, &set), Group0Result::Ok);
    assert_eq!(sm.settings.get(&b"a"[..]), Some(&b"1".to_vec()));
    // `b` is not absent any more: nothing changes, `a` neither.
    let stale = Group0Command::SetSettings(vec![
        change(b"a", Some(b"1"), Some(b"3")),
        change(b"b", None, Some(b"9")),
    ]);
    assert_eq!(sm.apply_command(3, &stale), Group0Result::Err(Group0Error::SettingConflict));
    assert_eq!(sm.settings.get(&b"a"[..]), Some(&b"1".to_vec()));
    // Removed when read as it is.
    let remove = Group0Command::SetSettings(vec![change(b"b", Some(b"2"), None)]);
    assert_eq!(sm.apply_command(4, &remove), Group0Result::Ok);
    assert!(!sm.settings.contains_key(&b"b"[..]));
    // The refusal travels in the answers of idempotent commands.
    let mut r = Requests::default();
    let id = teseladb_meta::request::RequestId { origin: NodeId(1), id: 1 };
    r.record(id, Group0Result::Err(Group0Error::SettingConflict));
    let mut e = teseladb_types::Encoder::new();
    r.encode(&mut e);
    let bytes = e.finish();
    assert_eq!(Requests::decode(&mut teseladb_types::Decoder::new(&bytes)), Ok(r));
}
