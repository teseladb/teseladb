//! A mutable copy of a document for the update operators: documents and arrays whose children
//! can be changed in place, converted back to a [`Document`] at the end.
//!
//! The copy of a stored document is lazy (003 FR-020): its embedded documents and arrays stay
//! as their BSON bytes ([`MVal::Packed`]) until an operator walks into them, and one that no
//! operator touches is written back from those bytes without being read into values.

use std::borrow::Cow;

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::error::Result;
use crate::value::OwnedValue;

/// A value of the mutable copy.
#[derive(Clone, Debug)]
pub(crate) enum MVal {
    Doc(MDoc),
    Arr(Vec<MVal>),
    Scalar(Value),
    /// An embedded document or array not read yet: its BSON bytes. The walks of the operators
    /// unpack it ([`MVal::unpack`]) before they look inside.
    Packed(Packed),
}

/// The BSON of an embedded document or array of the stored document, not read yet.
#[derive(Clone, Debug)]
pub(crate) struct Packed {
    array: bool,
    bytes: Box<[u8]>,
}

impl Packed {
    fn raw(&self) -> RawDoc<'_> {
        RawDoc::from_stored(&self.bytes).expect("the frame of an embedded document already read")
    }
}

impl PartialEq for MVal {
    fn eq(&self, other: &MVal) -> bool {
        self.to_value() == other.to_value()
    }
}

/// A document of the mutable copy: its fields in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MDoc {
    pub fields: Vec<(String, MVal)>,
}

impl MDoc {
    pub(crate) fn from_raw(d: RawDoc<'_>) -> MDoc {
        MDoc { fields: d.iter().map(|(k, v)| (k.to_owned(), MVal::from_raw(v))).collect() }
    }

    /// The copy of `d` that keeps its embedded documents and arrays as their bytes until they
    /// are walked into.
    pub(crate) fn from_raw_lazy(d: RawDoc<'_>) -> MDoc {
        MDoc { fields: d.iter().map(|(k, v)| (k.to_owned(), MVal::from_raw_lazy(v))).collect() }
    }

    pub(crate) fn get(&self, name: &str) -> Option<&MVal> {
        self.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    pub(crate) fn get_mut(&mut self, name: &str) -> Option<&mut MVal> {
        self.fields.iter_mut().find(|(k, _)| k == name).map(|(_, v)| v)
    }

    /// Sets `name`: in its place if it is there, at the end otherwise.
    pub(crate) fn set(&mut self, name: &str, v: MVal) {
        match self.get_mut(name) {
            Some(slot) => *slot = v,
            None => self.fields.push((name.to_owned(), v)),
        }
    }

    pub(crate) fn remove(&mut self, name: &str) -> Option<MVal> {
        let i = self.fields.iter().position(|(k, _)| k == name)?;
        Some(self.fields.remove(i).1)
    }

    pub(crate) fn to_document(&self) -> Document {
        let mut d = Document::new();
        for (k, v) in &self.fields {
            d.push(k.as_str(), v.to_value());
        }
        d
    }
}

impl MVal {
    pub(crate) fn from_raw(v: RawValue<'_>) -> MVal {
        match v {
            RawValue::Document(d) => MVal::Doc(MDoc::from_raw(d)),
            RawValue::Array(a) => MVal::Arr(a.iter().map(MVal::from_raw).collect()),
            v => MVal::Scalar(Value::from(v)),
        }
    }

    /// `v` with its embedded documents and arrays kept as their bytes ([`MVal::Packed`]).
    pub(crate) fn from_raw_lazy(v: RawValue<'_>) -> MVal {
        match v {
            RawValue::Document(d) => {
                MVal::Packed(Packed { array: false, bytes: d.as_bytes().into() })
            }
            RawValue::Array(a) => {
                MVal::Packed(Packed { array: true, bytes: a.as_doc().as_bytes().into() })
            }
            v => MVal::Scalar(Value::from(v)),
        }
    }

    /// Reads a packed value one level down, in place: its children stay packed.
    pub(crate) fn unpack(&mut self) {
        if let MVal::Packed(p) = self {
            *self = p.expand();
        }
    }

    /// The value with a packed one read one level down: what a walk that only looks sees.
    pub(crate) fn expanded(&self) -> Cow<'_, MVal> {
        match self {
            MVal::Packed(p) => Cow::Owned(p.expand()),
            other => Cow::Borrowed(other),
        }
    }

    pub(crate) fn from_value(v: &Value) -> MVal {
        match v {
            Value::Document(d) => MVal::Doc(MDoc {
                fields: d.iter().map(|(k, v)| (k.to_owned(), MVal::from_value(v))).collect(),
            }),
            Value::Array(items) => MVal::Arr(items.iter().map(MVal::from_value).collect()),
            v => MVal::Scalar(v.clone()),
        }
    }

    pub(crate) fn to_value(&self) -> Value {
        match self {
            MVal::Doc(d) => Value::Document(d.to_document()),
            MVal::Arr(items) => Value::Array(items.iter().map(MVal::to_value).collect()),
            MVal::Scalar(v) => v.clone(),
            MVal::Packed(p) => p.to_value(),
        }
    }

    /// The value as BSON, to compare or match it.
    pub(crate) fn owned(&self) -> Result<OwnedValue> {
        OwnedValue::new(self.to_value())
    }

    /// The BSON type alias, for messages.
    pub(crate) fn type_name(&self) -> &'static str {
        match self {
            MVal::Doc(_) => crate::types::OBJECT,
            MVal::Arr(_) => crate::types::ARRAY,
            MVal::Scalar(v) => crate::types::name_of(v.element_type()),
            MVal::Packed(p) if p.array => crate::types::ARRAY,
            MVal::Packed(_) => crate::types::OBJECT,
        }
    }
}

impl Packed {
    /// The value one level down.
    fn expand(&self) -> MVal {
        let raw = self.raw();
        if self.array {
            MVal::Arr(raw.iter().map(|(_, v)| MVal::from_raw_lazy(v)).collect())
        } else {
            MVal::Doc(MDoc::from_raw_lazy(raw))
        }
    }

    /// The value, unchanged: a document in canonical BSON keeps its bytes, so writing it back
    /// copies them ([`Document::from_canonical`]); anything else is read into values.
    fn to_value(&self) -> Value {
        let raw = self.raw();
        if self.array {
            Value::Array(raw.iter().map(|(_, v)| kept_value(v)).collect())
        } else {
            kept_value(RawValue::Document(raw))
        }
    }
}

/// `v` as a value, its documents in canonical BSON kept as their bytes.
fn kept_value(v: RawValue<'_>) -> Value {
    match v {
        RawValue::Document(d) if crate::canonical_bson::is_canonical(d) => {
            Value::Document(Document::from_canonical(d))
        }
        RawValue::Array(a) => Value::Array(a.iter().map(kept_value).collect()),
        v => Value::from(v),
    }
}
