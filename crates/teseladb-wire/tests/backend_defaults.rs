//! What a [`Backend`] answers when it implements only what it must: no LDAP, keytab, identity
//! provider or STS (013 FR-025), no leases of other nodes to kill, no user or privileges outside
//! an operation, and the default STS of AWS.

use std::collections::BTreeMap;

use teseladb_auth::users::{User, UserName};
use teseladb_bson::Document;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::diagnostics::ProcessFacts;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::{Backend, LdapLogin, Versions, WriteOp};
use teseladb_wire::error::{AUTHENTICATION_FAILED, CommandError, INTERNAL_ERROR, OPERATION_FAILED};

const NOW_MS: u64 = 1_700_000_000_000;

/// A node with only the required methods; the stored user, when set, is that document.
#[derive(Default)]
struct Bare {
    stored_user: Option<Vec<u8>>,
}

fn none() -> CommandError {
    CommandError::new(INTERNAL_ERROR, "not in this test")
}

impl Backend for Bare {
    fn collection(&mut self, _ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError> {
        Ok(None)
    }
    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError> {
        Ok(Vec::new())
    }
    fn databases(&mut self) -> Result<Vec<String>, CommandError> {
        Ok(Vec::new())
    }
    fn database_mark(&mut self, _db: &str) -> Result<Option<teseladb_hlc::Hlc>, CommandError> {
        Ok(None)
    }
    fn catalog_write(
        &mut self,
        _cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError> {
        Err(none())
    }
    fn indexes(&mut self, _collection: u32) -> Result<Vec<IndexEntry>, CommandError> {
        Ok(Vec::new())
    }
    fn index_write(&mut self, _cmd: IndexCommand) -> Result<(), CommandError> {
        Err(none())
    }
    fn write(&mut self, _op: WriteOp) -> Result<Vec<u8>, CommandError> {
        Err(none())
    }
    fn read_versions(&mut self, _keys: &[Vec<u8>]) -> Result<Versions, CommandError> {
        Err(none())
    }
    fn transact(
        &mut self,
        _read_ts: teseladb_hlc::Hlc,
        _writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        _txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<teseladb_hlc::Hlc, CommandError> {
        Err(none())
    }
    fn voters(&mut self, _start: &[u8], _end: &[u8]) -> Result<usize, CommandError> {
        Ok(1)
    }
    fn note_plan(&mut self, _: teseladb_bson::Document) {}

    fn end_operation(&mut self) {}
    fn txn_pause(&mut self) -> bool {
        false
    }
    fn txn_wait_clock(&mut self, _until_ms: u64) -> bool {
        false
    }

    fn txn_route(&self, _key: &[u8]) -> Option<teseladb_types::GroupId> {
        None
    }
    fn set_read_mark(&mut self, _at: Option<teseladb_txn::causal::ReadMark>) {}
    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark> {
        None
    }
    fn cluster_mark(&self) -> teseladb_hlc::Hlc {
        teseladb_hlc::Hlc { physical: self.now_ms(), logical: 0 }
    }
    fn txn_memory_budget(&self) -> usize {
        teseladb_txn::coordinator::MEMORY_BUDGET
    }
    fn txn_send(
        &mut self,
        _g: teseladb_types::GroupId,
        _op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, teseladb_range::host::RangeError> {
        Err(teseladb_range::host::RangeError::Unavailable)
    }
    fn txn_get(
        &mut self,
        _key: &[u8],
        _at: teseladb_hlc::Hlc,
        _view: &teseladb_range::push::TxnView,
        _uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError> {
        Ok(None)
    }
    fn txn_scan(
        &mut self,
        _start: &[u8],
        _end: &[u8],
        _limit: usize,
        _at: teseladb_hlc::Hlc,
        _view: &teseladb_range::push::TxnView,
        _uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        Ok(Vec::new())
    }
    fn scan(
        &mut self,
        _start: &[u8],
        _end: &[u8],
        _limit: usize,
        _concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        Ok(Vec::new())
    }
    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
        Ok(BTreeMap::new())
    }
    fn set_cluster_parameter(&mut self, _name: &str, _value: Vec<u8>) -> Result<(), CommandError> {
        Err(none())
    }
    fn now_ms(&self) -> u64 {
        NOW_MS
    }
    fn wall_ms(&self) -> u64 {
        NOW_MS
    }
    fn random(&mut self) -> u64 {
        4
    }
    fn node(&self) -> u64 {
        1
    }
    fn principal(
        &mut self,
        _kind: Principal,
        _db: &str,
        _name: &str,
    ) -> Result<Option<Vec<u8>>, CommandError> {
        Ok(self.stored_user.clone())
    }
    fn principals(
        &mut self,
        _kind: Principal,
        _db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        Ok(Vec::new())
    }
    fn security_write(
        &mut self,
        _ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        Err(none())
    }
    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        out.fill(4);
        Ok(())
    }
    fn authorization_lease(&self) -> bool {
        true
    }
    fn log_warning(&mut self, _id: i32, _msg: &str, _attr: &Document) {}

    fn log_slow_query(&mut self, _attr: &Document) {}
    fn rotate_certificates(&mut self, _message: Option<&str>) -> Result<(), CommandError> {
        Ok(())
    }
}

#[test]
fn a_bare_backend_has_no_external_identities() {
    let mut b = Bare::default();
    assert_eq!(b.ldap_login("alice", b"pw").unwrap(), LdapLogin::Refused);
    assert!(b.kerberos_keys().is_empty());
    assert!(b.oidc_providers().0.is_empty());
    assert_eq!(b.oidc_keys("https://idp.test", false).unwrap_err().code, OPERATION_FAILED);
    assert_eq!(b.aws_sts_url(), teseladb_auth::aws::DEFAULT_STS_URL);
    let req = teseladb_auth::aws::StsRequest { headers: Vec::new(), body: "" };
    assert_eq!(b.aws_sts(&req).unwrap_err().code, AUTHENTICATION_FAILED);
    assert_eq!(b.authenticated_roles(), None);
    b.invalidate_user_cache();
}

#[test]
fn a_bare_backend_knows_nothing_outside_an_operation() {
    let mut b = Bare::default();
    assert_eq!(b.authenticated_user(), None);
    assert_eq!(b.privileges(), None);
    assert_eq!(b.now_micros(), NOW_MS * 1000);
    assert!(b.kill_everywhere(&KillScope::All).is_ok());
    b.set_deadline(Some(NOW_MS));
    assert_eq!(b.process(), ProcessFacts::default());
    assert_eq!(b.stored_bytes(b"a", b"b").unwrap(), 0);
}

#[test]
fn a_stored_user_is_read_as_refdb_reads_it() {
    let name = UserName::new("alice", "admin");
    let mut b = Bare::default();
    assert_eq!(b.user(&name).unwrap(), None);
    // Without a string `user` it names no user; without credentials RefDb does not use it
    // (measured: UnsupportedFormat).
    b.stored_user = Some(Document::new().with("x", 1).to_bytes().unwrap());
    assert_eq!(b.user(&name).unwrap(), None);
    let good = User::new(UserName::new("alice", "$external"));
    let mut bare = good.to_document();
    bare.remove("credentials");
    b.stored_user = Some(bare.to_bytes().unwrap());
    let e = b.user(&name).unwrap_err();
    assert_eq!((e.code.code, e.code.name), (12, "UnsupportedFormat"));
    b.stored_user = Some(good.to_document().to_bytes().unwrap());
    assert_eq!(b.user(&name).unwrap(), Some(good));
}
