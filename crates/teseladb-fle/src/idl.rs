//! Reading the BSON of a payload or an option the way RefDb's IDL parser does, with its
//! messages (measured on a `refnode` 9.0.4): an unknown field fails where it is, a missing
//! required field after the walk, a field of the wrong type with its alias.

use teseladb_bson::types as t;
use teseladb_bson::{RawDoc, RawValue};
use teseladb_mql::types::name_of;

use crate::error::{FleError, IDL_FAILED_TO_PARSE, IDL_UNKNOWN_FIELD, TYPE_MISMATCH, err};
use crate::tokens::Token;

/// RefDb's name of a binary subtype in its messages.
pub fn subtype_name(subtype: u8) -> &'static str {
    match subtype {
        t::BINARY_GENERIC => "general",
        t::BINARY_FUNCTION => "function",
        t::BINARY_OLD => "byte(deprecated)",
        t::BINARY_UUID_OLD => "UUID(deprecated)",
        t::BINARY_UUID => "UUID",
        t::BINARY_MD5 => "MD5",
        t::BINARY_ENCRYPTED => "encrypt",
        t::BINARY_COLUMN => "column",
        t::BINARY_SENSITIVE => "sensitive",
        9 => "vector",
        0x80..=0xff => "Custom",
        _ => "invalid",
    }
}

/// The fields of a document, checked against the ones its IDL struct knows.
pub struct Fields<'a> {
    context: String,
    fields: Vec<(&'a str, RawValue<'a>)>,
}

impl<'a> Fields<'a> {
    /// The fields of `doc` under `context` (`root`, `create.encryptedFields`…): the first one
    /// not in `known` fails.
    pub fn read(doc: RawDoc<'a>, context: &str, known: &[&str]) -> Result<Fields<'a>, FleError> {
        let mut fields = Vec::new();
        for (k, v) in doc.iter() {
            if !known.contains(&k) {
                let msg = format!("BSON field '{context}.{k}' is an unknown field.");
                return Err(err(IDL_UNKNOWN_FIELD, msg));
            }
            fields.push((k, v));
        }
        Ok(Fields { context: context.to_string(), fields })
    }

    /// The path of field `k` in messages.
    pub fn path(&self, k: &str) -> String {
        format!("{}.{k}", self.context)
    }

    /// Field `k`, if present.
    pub fn get(&self, k: &str) -> Option<RawValue<'a>> {
        self.fields.iter().find(|(n, _)| *n == k).map(|(_, v)| *v)
    }

    /// Fails for the first field of `required` that is missing.
    pub fn require(&self, required: &[&str]) -> Result<(), FleError> {
        match required.iter().find(|k| self.get(k).is_none()) {
            Some(k) => Err(missing(&self.path(k))),
            None => Ok(()),
        }
    }

    /// The bytes of binary field `k` of `subtype`.
    pub fn binary(&self, k: &str, subtype: u8) -> Result<Option<&'a [u8]>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Binary { subtype: s, bytes }) if s == subtype => Ok(Some(bytes)),
            Some(RawValue::Binary { subtype: s, .. }) => Err(err(
                TYPE_MISMATCH,
                format!(
                    "BSON field '{}' is the wrong binData type '{}', expected type '{}'",
                    self.path(k),
                    subtype_name(s),
                    subtype_name(subtype)
                ),
            )),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::BINARY])),
        }
    }

    /// A 32-byte token in general binary field `k`; `name` is the token's type in the message of a
    /// wrong length.
    pub fn token(&self, k: &str, name: &str) -> Result<Option<Token>, FleError> {
        self.binary(k, t::BINARY_GENERIC)?
            .map(|b| Token::from_slice(b).ok_or_else(|| token_length(name)))
            .transpose()
    }

    /// A UUID in field `k`.
    pub fn uuid(&self, k: &str) -> Result<Option<[u8; 16]>, FleError> {
        match self.binary(k, t::BINARY_UUID)? {
            None => Ok(None),
            Some(b) => <[u8; 16]>::try_from(b).map(Some).map_err(|_| {
                err(
                    crate::error::BAD_VALUE,
                    "uuid must be a 16-byte binary field with UUID (4) subtype",
                )
            }),
        }
    }

    /// A 32-bit integer in field `k`.
    pub fn int32(&self, k: &str) -> Result<Option<i32>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Int32(n)) => Ok(Some(n)),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::INT32])),
        }
    }

    /// A 64-bit integer in field `k`.
    pub fn int64(&self, k: &str) -> Result<Option<i64>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Int64(n)) => Ok(Some(n)),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::INT64])),
        }
    }

    /// A boolean in field `k`.
    pub fn boolean(&self, k: &str) -> Result<Option<bool>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Boolean(b)) => Ok(Some(b)),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::BOOLEAN])),
        }
    }

    /// A document in field `k`.
    pub fn document(&self, k: &str) -> Result<Option<RawDoc<'a>>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Document(d)) => Ok(Some(d)),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::DOCUMENT])),
        }
    }

    /// The elements of array field `k`.
    pub fn array(&self, k: &str) -> Result<Option<Vec<RawValue<'a>>>, FleError> {
        match self.get(k) {
            None => Ok(None),
            Some(RawValue::Array(a)) => Ok(Some(a.iter().collect())),
            Some(v) => Err(wrong_type(&self.path(k), &v, &[t::ARRAY])),
        }
    }
}

/// `BSON field '<path>' is missing but a required field`.
pub fn missing(path: &str) -> FleError {
    err(IDL_FAILED_TO_PARSE, format!("BSON field '{path}' is missing but a required field"))
}

/// `BSON field '<path>' is the wrong type '<given>', expected type '<expected>'` (or `types`
/// for several).
pub fn wrong_type(path: &str, v: &RawValue<'_>, expected: &[u8]) -> FleError {
    let given = name_of(v.element_type());
    let msg = match expected {
        [one] => {
            format!(
                "BSON field '{path}' is the wrong type '{given}', expected type '{}'",
                name_of(*one)
            )
        }
        many => {
            let names: Vec<&str> = many.iter().map(|e| name_of(*e)).collect();
            format!(
                "BSON field '{path}' is the wrong type '{given}', expected types '[{}]'",
                names.join(", ")
            )
        }
    };
    err(TYPE_MISMATCH, msg)
}

/// `Invalid prf length for <token>`.
pub fn token_length(name: &str) -> FleError {
    err(crate::error::TOKEN_LENGTH, format!("Invalid prf length for {name}"))
}
