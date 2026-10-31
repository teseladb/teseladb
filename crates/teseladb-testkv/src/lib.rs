//! Test state machine (specification 001, FR-008): a map of byte keys with `put`, `get`, `cas`
//! and `delete`, to exercise the Multi-Raft engine and the simulator before the document ranges
//! exist. It is never linked into the `teseladb` binary.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

use std::collections::BTreeMap;

use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_types::{DecodeError, Decoder, Encoder};

/// Operation codes of the encoded commands.
const OP_PUT: u8 = 1;
const OP_GET: u8 = 2;
const OP_CAS: u8 = 3;
const OP_DELETE: u8 = 4;

/// A command for [`TestKv`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Sets `key` to `value`.
    Put {
        /// Key.
        key: Vec<u8>,
        /// Value.
        value: Vec<u8>,
    },
    /// Reads `key` through the log.
    Get {
        /// Key.
        key: Vec<u8>,
    },
    /// Sets `key` to `new` (`None` deletes it) only if its value is `expected` (`None` = absent).
    Cas {
        /// Key.
        key: Vec<u8>,
        /// Value it must have.
        expected: Option<Vec<u8>>,
        /// Value it gets.
        new: Option<Vec<u8>>,
    },
    /// Deletes `key`.
    Delete {
        /// Key.
        key: Vec<u8>,
    },
}

fn put_opt(e: &mut Encoder, v: &Option<Vec<u8>>) {
    match v {
        Some(v) => {
            e.bool(true).bytes(v);
        }
        None => {
            e.bool(false);
        }
    }
}

fn get_opt(d: &mut Decoder<'_>) -> Result<Option<Vec<u8>>, DecodeError> {
    Ok(if d.bool()? { Some(d.bytes()?.to_vec()) } else { None })
}

impl Command {
    /// Encodes the command as an entry payload.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        match self {
            Command::Put { key, value } => {
                e.u8(OP_PUT).bytes(key).bytes(value);
            }
            Command::Get { key } => {
                e.u8(OP_GET).bytes(key);
            }
            Command::Cas { key, expected, new } => {
                e.u8(OP_CAS).bytes(key);
                put_opt(&mut e, expected);
                put_opt(&mut e, new);
            }
            Command::Delete { key } => {
                e.u8(OP_DELETE).bytes(key);
            }
        }
        e.finish()
    }

    /// Decodes a payload written by [`Command::encode`].
    pub fn decode(b: &[u8]) -> Result<Command, DecodeError> {
        let mut d = Decoder::new(b);
        let at = d.position();
        let c = match d.u8()? {
            OP_PUT => Command::Put { key: d.bytes()?.to_vec(), value: d.bytes()?.to_vec() },
            OP_GET => Command::Get { key: d.bytes()?.to_vec() },
            OP_CAS => {
                let key = d.bytes()?.to_vec();
                Command::Cas { key, expected: get_opt(&mut d)?, new: get_opt(&mut d)? }
            }
            OP_DELETE => Command::Delete { key: d.bytes()?.to_vec() },
            _ => return Err(DecodeError::InvalidValue { offset: at }),
        };
        d.finish()?;
        Ok(c)
    }
}

/// Result of a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvOutput {
    /// `put` and `delete`: the value before.
    Previous(Option<Vec<u8>>),
    /// `get`: the value.
    Value(Option<Vec<u8>>),
    /// `cas`: whether it swapped, and the value before.
    Swapped(bool, Option<Vec<u8>>),
}

/// The state machine: a sorted map and the last applied index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestKv {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
    applied: u64,
}

impl TestKv {
    /// Value of a key.
    pub fn get(&self, key: &[u8]) -> Option<&[u8]> {
        self.map.get(key).map(Vec::as_slice)
    }

    /// Number of keys.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether it has no keys.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Every key and value, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
        self.map.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))
    }

    fn set(&mut self, key: Vec<u8>, value: Option<Vec<u8>>) -> Option<Vec<u8>> {
        match value {
            Some(v) => self.map.insert(key, v),
            None => self.map.remove(&key),
        }
    }
}

impl StateMachine for TestKv {
    type Output = KvOutput;

    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<KvOutput, ApplyError> {
        let cmd = Command::decode(payload).map_err(|e| ApplyError(format!("{e:?}")))?;
        self.applied = ctx.index;
        Ok(match cmd {
            Command::Put { key, value } => KvOutput::Previous(self.set(key, Some(value))),
            Command::Get { key } => KvOutput::Value(self.map.get(&key).cloned()),
            Command::Cas { key, expected, new } => {
                let current = self.map.get(&key).cloned();
                if current == expected {
                    self.set(key, new);
                    KvOutput::Swapped(true, current)
                } else {
                    KvOutput::Swapped(false, current)
                }
            }
            Command::Delete { key } => KvOutput::Previous(self.set(key, None)),
        })
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.applied).var(self.map.len() as u64);
        for (k, v) in &self.map {
            e.bytes(k).bytes(v);
        }
        e.finish()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        let read = |snap: &[u8]| -> Result<TestKv, DecodeError> {
            let mut d = Decoder::new(snap);
            let applied = d.u64()?;
            let n = d.var()?;
            let mut map = BTreeMap::new();
            for _ in 0..n {
                let k = d.bytes()?.to_vec();
                map.insert(k, d.bytes()?.to_vec());
            }
            d.finish()?;
            Ok(TestKv { map, applied })
        };
        *self = read(snap).map_err(|e| RestoreError(format!("{e:?}")))?;
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}
