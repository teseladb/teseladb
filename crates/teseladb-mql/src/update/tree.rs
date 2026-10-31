//! A mutable copy of a document for the update operators: documents and arrays whose children
//! can be changed in place, converted back to a [`Document`] at the end.

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::error::Result;
use crate::value::OwnedValue;

/// A value of the mutable copy.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum MVal {
    Doc(MDoc),
    Arr(Vec<MVal>),
    Scalar(Value),
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
        }
    }
}
