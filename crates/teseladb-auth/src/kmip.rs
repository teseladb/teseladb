//! The messages of a KMIP client (013 FR-028): the TTLV encoding of the OASIS KMIP Specification
//! (v1.2, §9.1) and the four requests a node sends for its master key, as RefDb Enterprise 9.0.4
//! sends them (measured against a KMIP server on 31-10-2026): `Create` of an AES-256 key for
//! encryption and decryption, `Get` of its bytes, `Activate`, and `Get Attributes` of its `State`.
//! Each request is one batch item; the connection and TLS are the caller's.

use std::fmt;

/// A TTLV tag (three bytes, `0x42xxxx` for the standard ones).
pub mod tag {
    /// Request Message.
    pub const REQUEST_MESSAGE: u32 = 0x42_0078;
    /// Response Message.
    pub const RESPONSE_MESSAGE: u32 = 0x42_007B;
    /// Request Header.
    pub const REQUEST_HEADER: u32 = 0x42_0077;
    /// Response Header.
    pub const RESPONSE_HEADER: u32 = 0x42_007A;
    /// Protocol Version.
    pub const PROTOCOL_VERSION: u32 = 0x42_0069;
    /// Protocol Version Major.
    pub const PROTOCOL_VERSION_MAJOR: u32 = 0x42_006A;
    /// Protocol Version Minor.
    pub const PROTOCOL_VERSION_MINOR: u32 = 0x42_006B;
    /// Batch Count.
    pub const BATCH_COUNT: u32 = 0x42_000D;
    /// Batch Item.
    pub const BATCH_ITEM: u32 = 0x42_000F;
    /// Operation.
    pub const OPERATION: u32 = 0x42_005C;
    /// Request Payload.
    pub const REQUEST_PAYLOAD: u32 = 0x42_0079;
    /// Response Payload.
    pub const RESPONSE_PAYLOAD: u32 = 0x42_007C;
    /// Object Type.
    pub const OBJECT_TYPE: u32 = 0x42_0057;
    /// Template-Attribute.
    pub const TEMPLATE_ATTRIBUTE: u32 = 0x42_0091;
    /// Attribute.
    pub const ATTRIBUTE: u32 = 0x42_0008;
    /// Attribute Name.
    pub const ATTRIBUTE_NAME: u32 = 0x42_000A;
    /// Attribute Value.
    pub const ATTRIBUTE_VALUE: u32 = 0x42_000B;
    /// Unique Identifier.
    pub const UNIQUE_IDENTIFIER: u32 = 0x42_0094;
    /// Result Status.
    pub const RESULT_STATUS: u32 = 0x42_007F;
    /// Result Reason.
    pub const RESULT_REASON: u32 = 0x42_007E;
    /// Result Message.
    pub const RESULT_MESSAGE: u32 = 0x42_007D;
    /// Symmetric Key.
    pub const SYMMETRIC_KEY: u32 = 0x42_008F;
    /// Key Block.
    pub const KEY_BLOCK: u32 = 0x42_0040;
    /// Key Format Type.
    pub const KEY_FORMAT_TYPE: u32 = 0x42_0042;
    /// Key Value.
    pub const KEY_VALUE: u32 = 0x42_0045;
    /// Key Material.
    pub const KEY_MATERIAL: u32 = 0x42_0043;
    /// Key (inside a Transparent Symmetric Key).
    pub const KEY: u32 = 0x42_003F;
    /// Data.
    pub const DATA: u32 = 0x42_00C2;
    /// IV/Counter/Nonce.
    pub const IV_COUNTER_NONCE: u32 = 0x42_003D;
}

/// Operations.
pub mod operation {
    /// Create.
    pub const CREATE: u32 = 1;
    /// Get.
    pub const GET: u32 = 10;
    /// Get Attributes.
    pub const GET_ATTRIBUTES: u32 = 11;
    /// Activate.
    pub const ACTIVATE: u32 = 18;
    /// Encrypt.
    pub const ENCRYPT: u32 = 31;
}

/// Object Type: Symmetric Key.
const SYMMETRIC_KEY: u32 = 2;
/// Cryptographic Algorithm: AES.
const AES: u32 = 3;
/// Cryptographic Usage Mask: Encrypt (4) and Decrypt (8).
const ENCRYPT_DECRYPT: i32 = 12;
/// Bits of the key a node asks for.
pub const KEY_BITS: i32 = 256;
/// Result Status: Success.
const SUCCESS: u32 = 0;
/// State: Active.
pub const ACTIVE: u32 = 2;
/// Key Format Type: Raw.
const RAW: u32 = 1;
/// Key Format Type: Transparent Symmetric Key.
const TRANSPARENT_SYMMETRIC_KEY: u32 = 7;

const CRYPTOGRAPHIC_ALGORITHM: &str = "Cryptographic Algorithm";
const CRYPTOGRAPHIC_LENGTH: &str = "Cryptographic Length";
const CRYPTOGRAPHIC_USAGE_MASK: &str = "Cryptographic Usage Mask";
const STATE: &str = "State";

/// Bytes of a TTLV header: tag (3), type (1), length (4).
pub const HEADER_LEN: usize = 8;
/// Nesting a message may have; a KMIP response of these operations has six levels.
const MAX_DEPTH: usize = 16;

/// A KMIP protocol version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Version {
    /// Major.
    pub major: i32,
    /// Minor.
    pub minor: i32,
}

/// KMIP 1.2, RefDb's default.
pub const V1_2: Version = Version { major: 1, minor: 2 };
/// KMIP 1.0, with `--kmipUseLegacyProtocol`.
pub const V1_0: Version = Version { major: 1, minor: 0 };

/// A TTLV value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Structure: the items it holds.
    Structure(Vec<Item>),
    /// Integer.
    Integer(i32),
    /// Long Integer.
    LongInteger(i64),
    /// Big Integer, big-endian two's complement, a multiple of 8 bytes.
    BigInteger(Vec<u8>),
    /// Enumeration.
    Enumeration(u32),
    /// Boolean.
    Boolean(bool),
    /// Text String (UTF-8).
    TextString(String),
    /// Byte String.
    ByteString(Vec<u8>),
    /// Date-Time, seconds since the epoch.
    DateTime(i64),
    /// Interval, seconds.
    Interval(u32),
}

/// A tagged value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Its tag.
    pub tag: u32,
    /// Its value.
    pub value: Value,
}

/// Why a message could not be used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KmipError {
    /// The bytes are not a TTLV message, or not the answer the request asked for.
    Malformed(&'static str),
    /// The server refused the operation: its Result Reason and Result Message.
    Failed {
        /// Result Reason (0 when the server gave none).
        reason: u32,
        /// Result Message.
        message: String,
    },
}

impl fmt::Display for KmipError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KmipError::Malformed(what) => write!(f, "malformed KMIP response: {what}"),
            KmipError::Failed { reason, message } => write!(f, "code: {reason} error: {message}"),
        }
    }
}

impl std::error::Error for KmipError {}

fn bad(what: &'static str) -> KmipError {
    KmipError::Malformed(what)
}

impl Item {
    /// An item.
    pub fn new(tag: u32, value: Value) -> Item {
        Item { tag, value }
    }

    /// Its TTLV bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        let (kind, body): (u8, Vec<u8>) = match &self.value {
            Value::Structure(items) => {
                let mut b = Vec::new();
                for i in items {
                    i.encode_into(&mut b);
                }
                (1, b)
            }
            Value::Integer(v) => (2, v.to_be_bytes().to_vec()),
            Value::LongInteger(v) => (3, v.to_be_bytes().to_vec()),
            Value::BigInteger(v) => (4, v.clone()),
            Value::Enumeration(v) => (5, v.to_be_bytes().to_vec()),
            Value::Boolean(v) => (6, u64::from(*v).to_be_bytes().to_vec()),
            Value::TextString(v) => (7, v.as_bytes().to_vec()),
            Value::ByteString(v) => (8, v.clone()),
            Value::DateTime(v) => (9, v.to_be_bytes().to_vec()),
            Value::Interval(v) => (10, v.to_be_bytes().to_vec()),
        };
        out.extend_from_slice(&self.tag.to_be_bytes()[1..]);
        out.push(kind);
        // A KMIP message is far below 4 GiB: the requests here are a few hundred bytes.
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out.resize(out.len() + padding(body.len()), 0);
    }

    /// The item at the start of `bytes` and the bytes it takes.
    pub fn decode(bytes: &[u8]) -> Result<(Item, usize), KmipError> {
        decode_at(bytes, 0)
    }

    /// The items of a structure.
    pub fn items(&self) -> &[Item] {
        match &self.value {
            Value::Structure(items) => items,
            _ => &[],
        }
    }

    /// The first item of a structure with `tag`.
    pub fn child(&self, tag: u32) -> Option<&Item> {
        self.items().iter().find(|i| i.tag == tag)
    }

    fn need(&self, tag: u32, what: &'static str) -> Result<&Item, KmipError> {
        self.child(tag).ok_or(bad(what))
    }
}

/// Zero bytes after a value of `len` bytes up to the next multiple of 8.
fn padding(len: usize) -> usize {
    (8 - len % 8) % 8
}

/// The length of the whole message whose first bytes are `head`, once its header is in.
pub fn message_len(head: &[u8]) -> Option<usize> {
    let len = head.get(4..HEADER_LEN)?;
    // A `u32` always fits a `usize` on the 64-bit targets TeselaDB runs on.
    let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    Some(HEADER_LEN + len + padding(len))
}

fn fixed<const N: usize>(body: &[u8]) -> Result<[u8; N], KmipError> {
    body.try_into().map_err(|_| bad("a fixed-length value of the wrong length"))
}

fn decode_at(bytes: &[u8], depth: usize) -> Result<(Item, usize), KmipError> {
    if depth > MAX_DEPTH {
        return Err(bad("too deeply nested"));
    }
    let head = bytes.get(..HEADER_LEN).ok_or(bad("a cut header"))?;
    let tag = u32::from_be_bytes([0, head[0], head[1], head[2]]);
    let len = u32::from_be_bytes([head[4], head[5], head[6], head[7]]) as usize;
    let total = HEADER_LEN + len + padding(len);
    let body = bytes.get(HEADER_LEN..HEADER_LEN + len).ok_or(bad("a cut value"))?;
    let pad = bytes.get(HEADER_LEN + len..total).ok_or(bad("a cut value"))?;
    if pad.iter().any(|&b| b != 0) {
        return Err(bad("padding that is not zero"));
    }
    let value = match head[3] {
        1 => {
            let mut items = Vec::new();
            let mut at = 0;
            while at < body.len() {
                let (item, used) = decode_at(&body[at..], depth + 1)?;
                items.push(item);
                at += used;
            }
            Value::Structure(items)
        }
        2 => Value::Integer(i32::from_be_bytes(fixed(body)?)),
        3 => Value::LongInteger(i64::from_be_bytes(fixed(body)?)),
        4 if len.is_multiple_of(8) => Value::BigInteger(body.to_vec()),
        5 => Value::Enumeration(u32::from_be_bytes(fixed(body)?)),
        6 => match u64::from_be_bytes(fixed(body)?) {
            0 => Value::Boolean(false),
            1 => Value::Boolean(true),
            _ => return Err(bad("a boolean that is neither 0 nor 1")),
        },
        7 => Value::TextString(
            String::from_utf8(body.to_vec()).map_err(|_| bad("a text string that is not UTF-8"))?,
        ),
        8 => Value::ByteString(body.to_vec()),
        9 => Value::DateTime(i64::from_be_bytes(fixed(body)?)),
        10 => Value::Interval(u32::from_be_bytes(fixed(body)?)),
        _ => return Err(bad("an unknown item type")),
    };
    Ok((Item { tag, value }, total))
}

fn int(tag: u32, v: i32) -> Item {
    Item::new(tag, Value::Integer(v))
}

fn enumeration(tag: u32, v: u32) -> Item {
    Item::new(tag, Value::Enumeration(v))
}

fn text(tag: u32, v: &str) -> Item {
    Item::new(tag, Value::TextString(v.to_owned()))
}

fn structure(tag: u32, items: Vec<Item>) -> Item {
    Item::new(tag, Value::Structure(items))
}

fn attribute(name: &str, value: Value) -> Item {
    structure(
        tag::ATTRIBUTE,
        vec![text(tag::ATTRIBUTE_NAME, name), Item::new(tag::ATTRIBUTE_VALUE, value)],
    )
}

/// What a node asks a KMIP server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// A new AES-256 key for encryption and decryption.
    CreateKey,
    /// The bytes of the key with this identifier.
    Get(String),
    /// Activates the key with this identifier.
    Activate(String),
    /// The state of the key with this identifier.
    State(String),
    /// `data` encrypted under the key with this identifier, with the server's parameters (the
    /// audit log's key, 013 FR-031).
    Encrypt(String, Vec<u8>),
}

impl Request {
    /// Its operation.
    pub fn operation(&self) -> u32 {
        match self {
            Request::CreateKey => operation::CREATE,
            Request::Get(_) => operation::GET,
            Request::Activate(_) => operation::ACTIVATE,
            Request::State(_) => operation::GET_ATTRIBUTES,
            Request::Encrypt(..) => operation::ENCRYPT,
        }
    }

    /// The request message in protocol `version`.
    pub fn encode(&self, version: Version) -> Vec<u8> {
        let payload = match self {
            Request::CreateKey => vec![
                enumeration(tag::OBJECT_TYPE, SYMMETRIC_KEY),
                structure(
                    tag::TEMPLATE_ATTRIBUTE,
                    vec![
                        attribute(CRYPTOGRAPHIC_ALGORITHM, Value::Enumeration(AES)),
                        attribute(CRYPTOGRAPHIC_LENGTH, Value::Integer(KEY_BITS)),
                        attribute(CRYPTOGRAPHIC_USAGE_MASK, Value::Integer(ENCRYPT_DECRYPT)),
                    ],
                ),
            ],
            Request::Get(id) | Request::Activate(id) => vec![text(tag::UNIQUE_IDENTIFIER, id)],
            Request::State(id) => {
                vec![text(tag::UNIQUE_IDENTIFIER, id), text(tag::ATTRIBUTE_NAME, STATE)]
            }
            Request::Encrypt(id, data) => vec![
                text(tag::UNIQUE_IDENTIFIER, id),
                Item::new(tag::DATA, Value::ByteString(data.clone())),
            ],
        };
        let header = structure(
            tag::REQUEST_HEADER,
            vec![
                structure(
                    tag::PROTOCOL_VERSION,
                    vec![
                        int(tag::PROTOCOL_VERSION_MAJOR, version.major),
                        int(tag::PROTOCOL_VERSION_MINOR, version.minor),
                    ],
                ),
                int(tag::BATCH_COUNT, 1),
            ],
        );
        let item = structure(
            tag::BATCH_ITEM,
            vec![
                enumeration(tag::OPERATION, self.operation()),
                structure(tag::REQUEST_PAYLOAD, payload),
            ],
        );
        structure(tag::REQUEST_MESSAGE, vec![header, item]).encode()
    }

    /// The answer `bytes` (one whole response message) gives to this request.
    pub fn answer(&self, bytes: &[u8]) -> Result<Answer, KmipError> {
        let (message, used) = Item::decode(bytes)?;
        if used != bytes.len() || message.tag != tag::RESPONSE_MESSAGE {
            return Err(bad("not one response message"));
        }
        let item = message.need(tag::BATCH_ITEM, "no batch item")?;
        if item.child(tag::OPERATION).map(|o| &o.value)
            != Some(&Value::Enumeration(self.operation()))
        {
            return Err(bad("the answer of another operation"));
        }
        let status = item.need(tag::RESULT_STATUS, "no result status")?;
        if status.value != Value::Enumeration(SUCCESS) {
            let reason = match item.child(tag::RESULT_REASON).map(|r| &r.value) {
                Some(Value::Enumeration(r)) => *r,
                _ => 0,
            };
            let message = match item.child(tag::RESULT_MESSAGE).map(|m| &m.value) {
                Some(Value::TextString(m)) => m.clone(),
                _ => String::new(),
            };
            return Err(KmipError::Failed { reason, message });
        }
        let payload = item.need(tag::RESPONSE_PAYLOAD, "no response payload")?;
        let id = match payload.need(tag::UNIQUE_IDENTIFIER, "no unique identifier")?.value {
            Value::TextString(ref id) => id.clone(),
            _ => return Err(bad("a unique identifier that is not text")),
        };
        match self {
            Request::CreateKey => Ok(Answer::Created(id)),
            Request::Activate(_) => Ok(Answer::Activated(id)),
            Request::Get(_) => key_material(payload).map(Answer::Key),
            Request::State(_) => state(payload).map(Answer::State),
            Request::Encrypt(..) => {
                let bytes = |t: u32| match payload.child(t).map(|i| &i.value) {
                    Some(Value::ByteString(b)) => Some(b.clone()),
                    _ => None,
                };
                let data = bytes(tag::DATA).ok_or_else(|| bad("no data"))?;
                Ok(Answer::Encrypted { data, iv: bytes(tag::IV_COUNTER_NONCE).unwrap_or_default() })
            }
        }
    }
}

/// The bytes of the AES-256 key in a Get payload.
fn key_material(payload: &Item) -> Result<Vec<u8>, KmipError> {
    let block = payload
        .need(tag::SYMMETRIC_KEY, "no symmetric key")?
        .need(tag::KEY_BLOCK, "no key block")?;
    let format = block.need(tag::KEY_FORMAT_TYPE, "no key format type")?;
    let material =
        block.need(tag::KEY_VALUE, "no key value")?.need(tag::KEY_MATERIAL, "no key material")?;
    let bytes = match (&format.value, &material.value) {
        (Value::Enumeration(RAW), Value::ByteString(b)) => b,
        (Value::Enumeration(TRANSPARENT_SYMMETRIC_KEY), Value::Structure(_)) => {
            match &material.need(tag::KEY, "no key")?.value {
                Value::ByteString(b) => b,
                _ => return Err(bad("a key that is not bytes")),
            }
        }
        _ => return Err(bad("a key format other than raw or transparent")),
    };
    if bytes.len() * 8 != KEY_BITS as usize {
        return Err(bad("a key that is not 256 bits"));
    }
    Ok(bytes.clone())
}

/// The State attribute in a Get Attributes payload.
fn state(payload: &Item) -> Result<u32, KmipError> {
    let attr = payload.need(tag::ATTRIBUTE, "no attribute")?;
    match (attr.child(tag::ATTRIBUTE_NAME), attr.child(tag::ATTRIBUTE_VALUE)) {
        (Some(name), Some(Item { value: Value::Enumeration(s), .. }))
            if name.value == Value::TextString(STATE.to_owned()) =>
        {
            Ok(*s)
        }
        _ => Err(bad("no state attribute")),
    }
}

/// What a request got.
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    /// The identifier of the new key.
    Created(String),
    /// The key's 32 bytes.
    Key(Vec<u8>),
    /// The identifier of the activated key.
    Activated(String),
    /// The key's state ([`ACTIVE`] when it can be used).
    State(u32),
    /// What an Encrypt gave: the data and the IV the server chose (empty without one).
    Encrypted {
        /// The encrypted data.
        data: Vec<u8>,
        /// The IV, counter or nonce.
        iv: Vec<u8>,
    },
}

impl fmt::Debug for Answer {
    /// Without the key's bytes.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Answer::Created(id) => write!(f, "Created({id:?})"),
            Answer::Key(_) => f.write_str("Key"),
            Answer::Activated(id) => write!(f, "Activated({id:?})"),
            Answer::State(s) => write!(f, "State({s})"),
            Answer::Encrypted { .. } => f.write_str("Encrypted"),
        }
    }
}
