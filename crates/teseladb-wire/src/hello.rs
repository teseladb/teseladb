//! The handshake (FR-003, FR-004; contract `hello-response.md`): `hello`, `isMaster` and
//! `ismaster` answered as a `refrouter` answers them, with the compression negotiation, the client
//! metadata rules, the awaitable `hello` of the topology monitors and `loadBalanced`.
//!
//! Field order and messages are those of a `refrouter` (measured against RefDb 7.0's; the version
//! fields are 9.0's, D-024).

use std::sync::Arc;

use teseladb_auth::clustertime::{KEY_LEN, KeyRing, Time};
use teseladb_auth::users::UserName;
use teseladb_topology::replset::{HelloContext, LastWrite, Limits, MemberId, ReplicaSet};
use teseladb_bson::{Document, Value};

use crate::commands::auth::{AUTHENTICATE, SASL_START, SPECULATIVE_AUTHENTICATE};
use crate::compressed::Compressor;
use crate::connection::HelloState;
use crate::error::{
    AWAIT_WITHOUT_TOPOLOGY, CLIENT_METADATA_APP_NAME_TOO_LARGE, CLIENT_METADATA_CANNOT_BE_MUTATED,
    CLIENT_METADATA_DOCUMENT_TOO_LARGE, CLIENT_METADATA_MISSING_FIELD, CommandError,
    IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, NEGATIVE_TOPOLOGY_COUNTER, TOPOLOGY_COUNTER_AHEAD,
    TYPE_MISMATCH,
};
use crate::types::{AWAIT_TYPES, HELLO_OK_TYPES, number, truthy, wrong_type, wrong_types};
use crate::{
    LOGICAL_SESSION_TIMEOUT_MINUTES, MAX_BSON_OBJECT_SIZE, MAX_MESSAGE_SIZE, MAX_WIRE_VERSION,
    MAX_WRITE_BATCH_SIZE, MIN_WIRE_VERSION,
};

/// Command names of the handshake.
pub const HELLO: &str = "hello";
/// Legacy name, still sent by drivers without `apiVersion` (DV-021).
pub const IS_MASTER: &str = "isMaster";
/// Legacy name in lower case.
pub const IS_MASTER_LOWER: &str = "ismaster";

/// `msg` of a `refrouter`.
pub const ISDBGRID: &str = "isdbgrid";
/// Largest client metadata document.
const MAX_METADATA: usize = 512;
/// Longest `application.name`.
const MAX_APP_NAME: usize = 128;
/// Bytes of the zero signature of `$clusterTime` (until 013 signs it).
const SIGNATURE_HASH_LEN: usize = KEY_LEN;

/// Whether `name` is a handshake command.
pub fn is_hello(name: &str) -> bool {
    [HELLO, IS_MASTER, IS_MASTER_LOWER].contains(&name)
}

/// This process's `topologyVersion`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Topology {
    /// `processId`: chosen when the node starts.
    pub process_id: [u8; 12],
    /// `counter`: grows when the node changes state (draining, shutting down).
    pub counter: i64,
}

/// A cluster time: seconds and increment of a BSON timestamp (005 FR-024).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClusterTime {
    /// Seconds.
    pub time: u32,
    /// Increment within the second.
    pub increment: u32,
}

/// What a reply needs from the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeInfo {
    /// `topologyVersion`.
    pub topology: Topology,
    /// `$clusterTime` and `operationTime`.
    pub cluster_time: ClusterTime,
    /// Wall clock in milliseconds since 1970 (`localTime`).
    pub now_ms: i64,
    /// The keys that sign `$clusterTime` and check the one a client sends, on a node with
    /// authentication; `None` signs with zeros and takes any signature (013 FR-014).
    pub keys: Option<Arc<KeyRing>>,
    /// The replica set this node is a member of, in the replica set presentation (018 FR-007);
    /// `None`: the node answers as a `refrouter`.
    pub replica_set: Option<Arc<ReplicaView>>,
}

/// A node's view of the replica set presentation: the set, the member it is and its
/// `lastWrite`.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplicaView {
    /// The set.
    pub set: ReplicaSet,
    /// This node's member.
    pub me: MemberId,
    /// `lastWrite`, from the closed mark.
    pub last: LastWrite,
}

// Priorities are validated within 0..=1000, never NaN.
impl Eq for ReplicaView {}

/// The node's limits, as its `hello` announces them.
pub const LIMITS: Limits = Limits {
    max_bson_object_size: MAX_BSON_OBJECT_SIZE,
    max_message_size: MAX_MESSAGE_SIZE as i32,
    max_write_batch_size: MAX_WRITE_BATCH_SIZE,
    logical_session_timeout_minutes: LOGICAL_SESSION_TIMEOUT_MINUTES,
    min_wire_version: MIN_WIRE_VERSION,
    max_wire_version: MAX_WIRE_VERSION,
};

/// A validated handshake request.
#[derive(Clone, Debug, PartialEq)]
pub struct HelloRequest {
    legacy: bool,
    hello_ok: bool,
    compression: Option<Vec<Compressor>>,
    load_balanced: bool,
    /// `topologyVersion` (process and counter) and `maxAwaitTimeMS`.
    wait: Option<(([u8; 12], i64), u64)>,
    client: bool,
    /// `client.application.name`.
    app_name: Option<String>,
    /// The client metadata document.
    metadata: Option<Document>,
    /// The user `saslSupportedMechs` named (boxed, like the next two, to keep the request
    /// small in [`crate::dispatch::Action`]).
    sasl: Option<Box<UserName>>,
    /// Its mechanisms, once the node looked the user up (013).
    mechanisms: Option<Box<[&'static str]>>,
    /// `speculativeAuthenticate`, validated.
    speculative: Option<Box<Document>>,
}

/// How the request goes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Answer now.
    Now,
    /// Answer when the topology changes or after this many milliseconds.
    Wait(u64),
}

/// Shortest wait of an awaitable `hello`: a `refrouter` 9.0 answers after at least 500 ms
/// whatever `maxAwaitTimeMS` says (measured: 100 and 300 wait 0.5 s, 700 waits 0.7 s).
pub const MIN_AWAIT_MS: u64 = 500;

/// The SASL mechanisms a `refrouter` 9.0 lists in `saslSupportedMechs` (measured).
pub const SASL_MECHANISMS: [&str; 2] = ["SCRAM-SHA-1", "SCRAM-SHA-256"];

fn field(name: &str) -> String {
    format!("{HELLO}.{name}")
}

fn missing(path: &str) -> CommandError {
    let msg = format!("BSON field '{path}' is missing but a required field");
    CommandError::new(IDL_FAILED_TO_PARSE, msg)
}

/// `topologyVersion`: an object with exactly `processId` (ObjectId) and `counter` (long).
fn topology_version(v: &Value) -> Result<([u8; 12], i64), CommandError> {
    let path = field("topologyVersion");
    let Value::Document(d) = v else { return Err(wrong_type(&path, v, "object")) };
    let (mut pid, mut counter) = (None, None);
    for (k, x) in d.iter() {
        let p = format!("{path}.{k}");
        match (k, x) {
            ("processId", Value::ObjectId(id)) => pid = Some(*id),
            ("processId", other) => return Err(wrong_type(&p, other, "objectId")),
            ("counter", Value::Int64(n)) => counter = Some(*n),
            ("counter", other) => return Err(wrong_type(&p, other, "long")),
            _ => {
                return Err(CommandError::new(
                    IDL_UNKNOWN_FIELD,
                    format!("BSON field '{p}' is an unknown field."),
                ));
            }
        }
    }
    // A `refrouter` 9.0 reports a missing `counter` first (measured).
    let counter = counter.ok_or_else(|| missing(&format!("{path}.counter")))?;
    let pid = pid.ok_or_else(|| missing(&format!("{path}.processId")))?;
    Ok((pid, counter))
}

fn compression(v: &Value) -> Result<Vec<Compressor>, CommandError> {
    let path = field("compression");
    let Value::Array(items) = v else { return Err(wrong_type(&path, v, "array")) };
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Value::String(name) = item else {
            return Err(wrong_type(&format!("{path}.{i}"), item, "string"));
        };
        if let Some(c) = Compressor::from_name(name).filter(|c| !out.contains(c)) {
            out.push(c);
        }
    }
    Ok(out)
}

fn metadata_missing(what: &str) -> CommandError {
    CommandError::new(
        CLIENT_METADATA_MISSING_FIELD,
        format!("Missing required {what} in the client metadata document"),
    )
}

/// A sub-document of the client metadata with its required string fields.
fn metadata_part(client: &Document, part: &str, required: &[&str]) -> Result<(), CommandError> {
    let d = match client.get(part) {
        None => return Err(metadata_missing(&format!("sub-document '{part}'"))),
        Some(Value::Document(d)) => d,
        Some(_) => {
            let msg = format!(
                "The '{part}' field is required to be a BSON document in the client metadata document"
            );
            return Err(CommandError::new(TYPE_MISMATCH, msg));
        }
    };
    for f in required {
        if !matches!(d.get(f), Some(Value::String(_))) {
            return Err(metadata_missing(&format!("field '{part}.{f}'")));
        }
    }
    Ok(())
}

/// The `client` document drivers send in their first `hello`.
fn client_metadata(v: &Value) -> Result<&Document, CommandError> {
    let Value::Document(client) = v else {
        return Err(CommandError::new(
            TYPE_MISMATCH,
            "The client metadata document must be a document",
        ));
    };
    if client.to_bytes().map_or(usize::MAX, |b| b.len()) > MAX_METADATA {
        let msg = format!(
            "The client metadata document must be less then or equal to {MAX_METADATA} bytes"
        );
        return Err(CommandError::new(CLIENT_METADATA_DOCUMENT_TOO_LARGE, msg));
    }
    if let Some(Value::Document(app)) = client.get("application")
        && matches!(app.get("name"), Some(Value::String(n)) if n.len() > MAX_APP_NAME)
    {
        let msg = format!(
            "The 'application.name' field must be less then or equal to {MAX_APP_NAME} bytes in the client metadata document"
        );
        return Err(CommandError::new(CLIENT_METADATA_APP_NAME_TOO_LARGE, msg));
    }
    metadata_part(client, "driver", &["name", "version"])?;
    metadata_part(client, "os", &["type"])?;
    Ok(client)
}

/// `speculativeAuthenticate`: a non-empty document whose command is `saslStart` or
/// `authenticate` (its other faults make the field absent from the reply, measured).
fn speculative(v: &Value) -> Result<Document, CommandError> {
    let path = field(SPECULATIVE_AUTHENTICATE);
    let Value::Document(d) = v else { return Err(wrong_type(&path, v, "object")) };
    match d.iter().next() {
        None => {
            let msg = format!("{path} must be a non-empty Object");
            Err(CommandError::new(crate::error::BAD_VALUE, msg))
        }
        Some((SASL_START | AUTHENTICATE, _)) => Ok(d.clone()),
        Some((other, _)) => {
            let msg = format!("{path} unknown command: {other}");
            Err(CommandError::new(crate::error::SPECULATIVE_UNKNOWN_COMMAND, msg))
        }
    }
}

fn display_number(v: &Value) -> String {
    match v {
        Value::Int32(n) => n.to_string(),
        Value::Int64(n) => n.to_string(),
        other => number(other).map_or_else(String::new, |x| x.to_string()),
    }
}

impl HelloRequest {
    /// Whether the request came as `isMaster` or `ismaster`, which answer `ismaster` instead of
    /// `isWritablePrimary`.
    pub fn legacy(&self) -> bool {
        self.legacy
    }

    /// The user `saslSupportedMechs` named.
    pub fn sasl_user(&self) -> Option<&UserName> {
        self.sasl.as_deref()
    }

    /// The mechanisms to list for [`HelloRequest::sasl_user`].
    pub fn set_mechanisms(&mut self, mechanisms: Vec<&'static str>) {
        self.mechanisms = Some(mechanisms.into_boxed_slice());
    }

    /// The validated `speculativeAuthenticate` document.
    pub fn speculative(&self) -> Option<&Document> {
        self.speculative.as_deref()
    }

    /// Whether the client sent `helloOk`.
    pub fn hello_ok(&self) -> bool {
        self.hello_ok
    }

    /// Reads and validates the command `cmd` named `name`, on a connection in `state`.
    pub fn parse(
        name: &str,
        cmd: &Document,
        state: &HelloState,
    ) -> Result<HelloRequest, CommandError> {
        let mut req = HelloRequest {
            legacy: name != HELLO,
            hello_ok: false,
            compression: None,
            load_balanced: false,
            wait: None,
            client: false,
            app_name: None,
            metadata: None,
            sasl: None,
            mechanisms: None,
            speculative: None,
        };
        let (mut tv, mut max_await, mut client) = (None, None, None);
        for (k, v) in cmd.iter() {
            match k {
                "helloOk" => {
                    req.hello_ok =
                        truthy(v).ok_or_else(|| wrong_types(&field(k), v, &HELLO_OK_TYPES))?;
                }
                "compression" => req.compression = Some(compression(v)?),
                "loadBalanced" => match v {
                    Value::Boolean(b) => req.load_balanced = *b,
                    other => return Err(wrong_type(&field(k), other, "bool")),
                },
                "saslSupportedMechs" => match v {
                    Value::String(u) if !u.contains('.') => {
                        return Err(CommandError::new(
                            crate::error::BAD_VALUE,
                            "UserName must contain a '.' separated database.user pair",
                        ));
                    }
                    Value::String(u) => req.sasl = UserName::from_dotted(u).map(Box::new),
                    Value::Document(d) => {
                        let text = |f| match d.get(f) {
                            Some(Value::String(s)) => s.clone(),
                            _ => String::new(),
                        };
                        req.sasl = Some(Box::new(UserName::new(text("user"), text("db"))));
                    }
                    other => return Err(wrong_types(&field(k), other, &["string", "object"])),
                },
                "topologyVersion" => tv = Some(topology_version(v)?),
                "maxAwaitTimeMS" => {
                    let ms = number(v).ok_or_else(|| wrong_types(&field(k), v, &AWAIT_TYPES))?;
                    // The field's own rule comes before the rules between fields (measured).
                    if ms < 0.0 {
                        let msg = format!(
                            "BSON field 'maxAwaitTimeMS' value must be >= 0, actual value '{}'",
                            display_number(v)
                        );
                        return Err(CommandError::new(crate::error::BAD_VALUE, msg));
                    }
                    max_await = Some(ms);
                }
                "client" => client = Some(v),
                SPECULATIVE_AUTHENTICATE => req.speculative = Some(Box::new(speculative(v)?)),
                _ => {}
            }
        }
        if let Some(c) = client {
            if state.greeted {
                let msg = "The client metadata document may only be sent in the first hello";
                return Err(CommandError::new(CLIENT_METADATA_CANNOT_BE_MUTATED, msg));
            }
            let d = client_metadata(c)?;
            req.client = true;
            req.metadata = Some(d.clone());
            if let Some(Value::Document(app)) = d.get("application")
                && let Some(Value::String(n)) = app.get("name")
            {
                req.app_name = Some(n.clone());
            }
        }
        req.wait = match (tv, max_await) {
            (None, None) => None,
            (None, Some(_)) => {
                let msg = "A request with 'maxAwaitTimeMS' must include a 'topologyVersion'";
                return Err(CommandError::new(AWAIT_WITHOUT_TOPOLOGY, msg));
            }
            (Some(_), None) => {
                let msg = "A request with a 'topologyVersion' must include 'maxAwaitTimeMS'";
                return Err(CommandError::new(AWAIT_WITHOUT_TOPOLOGY, msg));
            }
            (Some((_, counter)), Some(_)) if counter < 0 => {
                let msg = "topologyVersion must have a non-negative counter";
                return Err(CommandError::new(NEGATIVE_TOPOLOGY_COUNTER, msg));
            }
            (Some(t), Some(ms)) => Some((t, ms as u64)),
        };
        Ok(req)
    }

    /// Whether to answer now or wait for a topology change; records the negotiation on the
    /// connection.
    pub fn accept(
        &self,
        state: &mut HelloState,
        topology: &Topology,
    ) -> Result<Next, CommandError> {
        let next = match self.wait {
            Some(((pid, counter), _))
                if pid == topology.process_id && counter > topology.counter =>
            {
                let msg = format!(
                    "Received a topology version with counter: {counter} which is greater than the refrouter topology version counter: {}",
                    topology.counter
                );
                return Err(CommandError::new(TOPOLOGY_COUNTER_AHEAD, msg));
            }
            Some(((pid, counter), ms))
                if pid == topology.process_id && counter == topology.counter =>
            {
                Next::Wait(ms.max(MIN_AWAIT_MS))
            }
            _ => Next::Now,
        };
        if let Some(list) = &self.compression {
            state.compressors =
                list.iter().copied().filter(|c| state.enabled.contains(c)).collect();
        }
        if self.app_name.is_some() {
            state.app_name = self.app_name.clone();
        }
        if self.metadata.is_some() {
            state.metadata = self.metadata.clone();
        }
        state.greeted = true;
        Ok(next)
    }

    /// The reply, without `$clusterTime` and `operationTime`.
    pub fn response(&self, state: &HelloState, connection_id: i32, node: &NodeInfo) -> Document {
        if let Some(view) = &node.replica_set {
            return self.member_response(state, connection_id, node, view);
        }
        let mut d = Document::new();
        if !state.compressors.is_empty() {
            let names: Vec<Value> =
                state.compressors.iter().map(|c| Value::from(c.name())).collect();
            d.push("compression", names);
        }
        d.push(if self.legacy { IS_MASTER_LOWER } else { "isWritablePrimary" }, true);
        d.push("msg", ISDBGRID);
        let tv = Document::new()
            .with("processId", Value::ObjectId(node.topology.process_id))
            .with("counter", node.topology.counter);
        d.push("topologyVersion", tv);
        if self.hello_ok {
            d.push("helloOk", true);
        }
        d.push("maxBsonObjectSize", MAX_BSON_OBJECT_SIZE);
        d.push("maxMessageSizeBytes", MAX_MESSAGE_SIZE as i32);
        d.push("maxWriteBatchSize", MAX_WRITE_BATCH_SIZE);
        d.push("localTime", Value::DateTime(node.now_ms));
        d.push("logicalSessionTimeoutMinutes", LOGICAL_SESSION_TIMEOUT_MINUTES);
        d.push("connectionId", connection_id);
        d.push("maxWireVersion", MAX_WIRE_VERSION);
        d.push("minWireVersion", MIN_WIRE_VERSION);
        // A `refrouter` 9.0 names its mechanisms for any user, also one that does not exist
        // (measured), so the answer says nothing about which users exist.
        if self.sasl.is_some() {
            let mechs = self.mechanisms.as_deref().unwrap_or(&SASL_MECHANISMS);
            let mechs: Vec<Value> = mechs.iter().map(|&m| Value::from(m)).collect();
            d.push("saslSupportedMechs", mechs);
        }
        if self.load_balanced {
            d.push("serviceId", Value::ObjectId(node.topology.process_id));
        }
        d.push("ok", 1.0);
        d
    }

    /// The reply of a member of the replica set presentation (contract `topology-modes.md`):
    /// `loadBalanced` adds no `serviceId`, as on a `refnode` that is not behind a balancer.
    fn member_response(
        &self,
        state: &HelloState,
        connection_id: i32,
        node: &NodeInfo,
        view: &ReplicaView,
    ) -> Document {
        let ctx = HelloContext {
            legacy: self.legacy,
            hello_ok: self.hello_ok,
            compressors: state.compressors.iter().map(|c| c.name()).collect(),
            process_id: node.topology.process_id,
            counter: node.topology.counter,
            now_ms: node.now_ms,
            connection_id,
            limits: LIMITS,
        };
        let mut d = view.set.hello(&ctx, view.me, &view.last);
        if self.sasl.is_some() {
            let mechs = self.mechanisms.as_deref().unwrap_or(&SASL_MECHANISMS);
            let mechs: Vec<Value> = mechs.iter().map(|&m| Value::from(m)).collect();
            d.remove("ok");
            d.push("saslSupportedMechs", mechs);
            d.push("ok", 1.0);
        }
        d
    }
}

/// `$clusterTime` and `operationTime` appended to a reply, as every reply of a `refrouter` has them:
/// signed with the node's key for that time, or with zeros and key 0 without one.
pub fn with_cluster_time(mut d: Document, node: &NodeInfo) -> Document {
    let t = node.cluster_time;
    let ts = Value::Timestamp { time: t.time, increment: t.increment };
    let (hash, id) = signature(t, node.keys.as_deref());
    let signature = Document::new()
        .with("hash", Value::Binary { subtype: 0, bytes: hash.to_vec() })
        .with("keyId", id);
    d.push(
        "$clusterTime",
        Document::new().with("clusterTime", ts.clone()).with("signature", signature),
    );
    d.push("operationTime", ts);
    d
}

/// The signature of cluster time `t`: with the node's key for that time, or zeros and key 0.
fn signature(t: ClusterTime, keys: Option<&KeyRing>) -> ([u8; SIGNATURE_HASH_LEN], i64) {
    let at = Time { secs: t.time, inc: t.increment };
    keys.and_then(|k| k.signature(at)).unwrap_or(([0; SIGNATURE_HASH_LEN], 0))
}

/// Moves the cluster time that [`with_cluster_time`] appended to reply `d` to the later time `t`,
/// signed again: what a reply gives once its command ran (008 FR-013). It changes the values in
/// place, without allocating (012 FR-008). `false`, with `d` untouched, when `d` does not end
/// with the two fields that function appends.
pub fn restamp_cluster_time(d: &mut Document, t: ClusterTime, keys: Option<&KeyRing>) -> bool {
    let n = d.len();
    let ends_stamped = {
        let mut tail = d.iter().skip(n.saturating_sub(2)).map(|(k, _)| k);
        n >= 2 && tail.next() == Some("$clusterTime") && tail.next() == Some("operationTime")
    };
    if !ends_stamped {
        return false;
    }
    let ts = Value::Timestamp { time: t.time, increment: t.increment };
    let (hash, id) = signature(t, keys);
    if let Some(Value::Document(ct)) = d.get_mut("$clusterTime") {
        if let Some(v) = ct.get_mut("clusterTime") {
            *v = ts.clone();
        }
        if let Some(Value::Document(sig)) = ct.get_mut("signature") {
            if let Some(Value::Binary { bytes, .. }) = sig.get_mut("hash") {
                bytes.clear();
                bytes.extend_from_slice(&hash);
            }
            if let Some(v) = sig.get_mut("keyId") {
                *v = Value::Int64(id);
            }
        }
    }
    if let Some(v) = d.get_mut("operationTime") {
        *v = ts;
    }
    true
}
