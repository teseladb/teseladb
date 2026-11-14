//! The description of an update (014 FR-009): what the leader computes while it applies the
//! operators, kept with the version, and the `updateDescription` of the event. Its shape is the one
//! a `refrouter` 9.0.4 gives (measured on 31-10): `updatedFields`, `removedFields`, `truncatedArrays`
//! as `{field, newSize}` and, with `showExpandedEvents` only, `disambiguatedPaths`, present even
//! when empty, with each path whose dotted form is ambiguous (a field named like a number) split
//! into its parts: field names as strings and array positions as integers.

use teseladb_bson::{BsonError, Document, RawDoc, Value};

/// A part of a path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathPart {
    /// A field name.
    Field(String),
    /// A position in an array.
    Index(u32),
}

/// What an update changed in a document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpdateDescription {
    /// Paths set, with their new values.
    pub updated: Vec<(String, Value)>,
    /// Paths removed.
    pub removed: Vec<String>,
    /// Arrays cut short, with their new size.
    pub truncated: Vec<(String, u32)>,
    /// Ambiguous paths of `updated` and `removed`, part by part.
    pub disambiguated: Vec<(String, Vec<PathPart>)>,
    /// It describes a pipeline update ([`crate::pipeline_diff`]), whose oplog entry is the diff of
    /// the document and not of its operators (018 FR-009).
    pub pipeline: bool,
}

/// Why a stored description does not read back.
#[derive(Clone, Debug, PartialEq)]
pub enum DescError {
    /// Not a BSON document.
    Bson(BsonError),
    /// A field missing or of another type.
    Shape(&'static str),
}

const UPDATED: &str = "updatedFields";
const REMOVED: &str = "removedFields";
const TRUNCATED: &str = "truncatedArrays";
const DISAMBIGUATED: &str = "disambiguatedPaths";
const FIELD: &str = "field";
const NEW_SIZE: &str = "newSize";
/// The mark a stored description of a pipeline update carries; the event does not show it.
const PIPELINE: &str = "$pipeline";
/// Where a change description's framing is cut short.
const UPDATES: &str = "changes";

/// A description holds values of a document two levels below it, of any size the document has.
pub(crate) const DESCRIPTION_LIMITS: teseladb_bson::Limits = teseladb_bson::Limits {
    max_size: 2 * teseladb_bson::MAX_SIZE,
    max_depth: teseladb_bson::MAX_DEPTH + 2,
};

impl UpdateDescription {
    /// The `updateDescription` of the event; `expanded` is `showExpandedEvents`.
    pub fn to_document(&self, expanded: bool) -> Document {
        let mut d = Document::new();
        let mut updated = Document::new();
        for (path, v) in &self.updated {
            updated.push(path.clone(), v.clone());
        }
        d.push(UPDATED, updated);
        d.push(REMOVED, self.removed.iter().map(|p| Value::from(p.as_str())).collect::<Vec<_>>());
        let truncated = self.truncated.iter().map(|(field, size)| {
            let mut t = Document::new();
            t.push(FIELD, field.as_str());
            t.push(NEW_SIZE, i32::try_from(*size).unwrap_or(i32::MAX));
            Value::Document(t)
        });
        d.push(TRUNCATED, truncated.collect::<Vec<_>>());
        if expanded {
            let mut paths = Document::new();
            for (path, parts) in &self.disambiguated {
                let parts = parts.iter().map(|p| match p {
                    PathPart::Field(f) => Value::from(f.as_str()),
                    PathPart::Index(i) => Value::Int32(i32::try_from(*i).unwrap_or(i32::MAX)),
                });
                paths.push(path.clone(), parts.collect::<Vec<_>>());
            }
            d.push(DISAMBIGUATED, paths);
        }
        d
    }

    /// The bytes kept with the version.
    pub fn encode(&self) -> Result<Vec<u8>, BsonError> {
        self.stored().to_bytes()
    }

    /// What is kept with the version: the event's description with every field, and the mark of
    /// a pipeline update.
    pub(crate) fn stored(&self) -> Document {
        let mut d = self.to_document(true);
        if self.pipeline {
            d.push(PIPELINE, true);
        }
        d
    }

    /// The change description of a write (005 FR-008): the description of each document it
    /// updated with operators, by the document's key, kept with the versions it writes. Each one
    /// goes as its key and its description, both after their length (four bytes, little endian):
    /// a write may update many documents, and their descriptions together may pass the size of a
    /// document; a description nests its values two levels below the document's.
    pub fn encode_all(updates: &[(Vec<u8>, UpdateDescription)]) -> Result<Vec<u8>, BsonError> {
        let mut out = Vec::new();
        for (key, d) in updates {
            let desc = d.stored().to_bytes_with(DESCRIPTION_LIMITS)?;
            for part in [key.as_slice(), desc.as_slice()] {
                // A key is a few bytes and a description at most `DESCRIPTION_LIMITS`.
                out.extend_from_slice(&(part.len() as u32).to_le_bytes());
                out.extend_from_slice(part);
            }
        }
        Ok(out)
    }

    /// Reads the bytes [`UpdateDescription::encode_all`] wrote.
    pub fn decode_all(mut bytes: &[u8]) -> Result<Vec<(Vec<u8>, UpdateDescription)>, DescError> {
        let take = |bytes: &mut &[u8]| -> Result<Vec<u8>, DescError> {
            let short = DescError::Shape(UPDATES);
            let (len, rest) = bytes.split_first_chunk::<4>().ok_or(short.clone())?;
            let len = u32::from_le_bytes(*len) as usize;
            let part = rest.get(..len).ok_or(short)?.to_vec();
            *bytes = &rest[len..];
            Ok(part)
        };
        let mut out = Vec::new();
        while !bytes.is_empty() {
            let key = take(&mut bytes)?;
            let desc = take(&mut bytes)?;
            let raw =
                RawDoc::from_bytes_with(&desc, DESCRIPTION_LIMITS).map_err(DescError::Bson)?;
            out.push((key, UpdateDescription::from_document(&Document::from(raw))?));
        }
        Ok(out)
    }

    /// Reads the bytes [`UpdateDescription::encode`] wrote.
    pub fn decode(bytes: &[u8]) -> Result<UpdateDescription, DescError> {
        let d = Document::from(RawDoc::from_bytes(bytes).map_err(DescError::Bson)?);
        UpdateDescription::from_document(&d)
    }

    /// Reads a description from its `updateDescription` document.
    pub(crate) fn from_document(d: &Document) -> Result<UpdateDescription, DescError> {
        let doc = |name| match d.get(name) {
            Some(Value::Document(v)) => Ok(v),
            _ => Err(DescError::Shape(name)),
        };
        let array = |name| match d.get(name) {
            Some(Value::Array(v)) => Ok(v),
            _ => Err(DescError::Shape(name)),
        };
        let updated = doc(UPDATED)?.iter().map(|(k, v)| (k.to_owned(), v.clone())).collect();
        let removed = array(REMOVED)?
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                _ => Err(DescError::Shape(REMOVED)),
            })
            .collect::<Result<_, _>>()?;
        let truncated = array(TRUNCATED)?
            .iter()
            .map(|v| match v {
                Value::Document(t) => match (t.get(FIELD), t.get(NEW_SIZE)) {
                    (Some(Value::String(f)), Some(Value::Int32(n))) => {
                        Ok((f.clone(), u32::try_from(*n).map_err(|_| DescError::Shape(NEW_SIZE))?))
                    }
                    _ => Err(DescError::Shape(TRUNCATED)),
                },
                _ => Err(DescError::Shape(TRUNCATED)),
            })
            .collect::<Result<_, _>>()?;
        let disambiguated = doc(DISAMBIGUATED)?
            .iter()
            .map(|(path, parts)| {
                let Value::Array(parts) = parts else {
                    return Err(DescError::Shape(DISAMBIGUATED));
                };
                let parts = parts
                    .iter()
                    .map(|p| match p {
                        Value::String(f) => Ok(PathPart::Field(f.clone())),
                        Value::Int32(i) => u32::try_from(*i)
                            .map(PathPart::Index)
                            .map_err(|_| DescError::Shape(DISAMBIGUATED)),
                        _ => Err(DescError::Shape(DISAMBIGUATED)),
                    })
                    .collect::<Result<_, _>>()?;
                Ok((path.to_owned(), parts))
            })
            .collect::<Result<_, _>>()?;
        let pipeline = d.get(PIPELINE) == Some(&Value::Boolean(true));
        Ok(UpdateDescription { updated, removed, truncated, disambiguated, pipeline })
    }
}
