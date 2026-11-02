//! Writes with Queryable Encryption payloads (FR-003): each insert and update payload of an
//! indexed field becomes the indexed value the collection keeps, one new ESC position and one ECOC
//! entry per tag, and its tags in `__safeContent__` (measured on a `refnode` 9.0.4).

use teseladb_bson::types as t;
use teseladb_bson::{Document, Value};

use crate::error::{CONTENTION_EXCEEDED, FleError, MISMATCHED_KEY_ID, UNEXPECTED_PAYLOAD, err};
use crate::esc::{EscKeys, EscReader, Overlay, state};
use crate::fields::{EncryptedField, EncryptedFieldConfig, QueryType, SAFE_CONTENT};
use crate::iev::{Block, IndexedValue, Kind};
use crate::payloads::{InsertUpdatePayload, TokenSet, subtype_of};
use crate::subtype::BlobSubtype;
use crate::tokens::{self, Token};

/// An error of a write: the payload's, or the ESC read's.
#[derive(Debug, PartialEq)]
pub enum WriteError<E> {
    /// The payload or the document is refused.
    Fle(FleError),
    /// Reading the ESC failed.
    Read(E),
}

impl<E> From<FleError> for WriteError<E> {
    fn from(e: FleError) -> WriteError<E> {
        WriteError::Fle(e)
    }
}

/// An entry of the ECOC: the field and the encrypted ESC token compaction reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EcocEntry {
    /// `fieldName`.
    pub field: String,
    /// `value`.
    pub value: Vec<u8>,
}

/// What a write of one document adds besides the document.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Added {
    /// The ECOC entries, in the order of the tags.
    pub ecoc: Vec<EcocEntry>,
    /// The tags, in the order of the fields.
    pub tags: Vec<Token>,
}

/// Random IVs for the server's encryptions.
pub trait Ivs {
    /// A fresh 16-byte IV.
    fn iv(&mut self) -> [u8; 16];
}

impl<F: FnMut() -> [u8; 16]> Ivs for F {
    fn iv(&mut self) -> [u8; 16] {
        self()
    }
}

/// A UUID as RefDb writes it in messages.
pub fn uuid_text(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// The indexed value of one payload, with its ESC positions taken in `esc`.
pub fn index<R: EscReader>(
    field: &EncryptedField,
    p: &InsertUpdatePayload,
    esc: &mut Overlay<'_, R>,
    ivs: &mut impl Ivs,
    added: &mut Added,
) -> Result<IndexedValue, WriteError<R::Error>> {
    if p.index_key_id != field.key_id {
        return Err(err(
            MISMATCHED_KEY_ID,
            format!(
                "Mismatched keyId for field '{}' expected {}, found {}",
                field.path,
                uuid_text(&field.key_id),
                uuid_text(&p.index_key_id)
            ),
        )
        .into());
    }
    let configured = field.query().map_or(0, |q| q.contention);
    if i128::from(p.contention) > i128::from(configured) {
        return Err(err(
            CONTENTION_EXCEEDED,
            format!(
                "contentionFactor {} in payload for field '{}' exceeds collection's configured \
                 contention {configured}",
                p.contention, field.path
            ),
        )
        .into());
    }
    let kind = match (&p.edges, &p.text) {
        (Some(_), _) => Kind::Range,
        (None, Some(text)) => Kind::Text {
            substrings: u32::try_from(text.substrings.len()).unwrap_or(u32::MAX),
            suffixes: u32::try_from(text.suffixes.len()).unwrap_or(u32::MAX),
        },
        (None, None) => Kind::Equality,
    };
    let mut blocks = Vec::new();
    for set in p.all_sets() {
        let (tag, count) = take_position(set, esc)?;
        blocks.push(Block::new(&set.server, (count, p.contention), tag, (ivs.iv(), ivs.iv())));
        added.tags.push(tag);
        added
            .ecoc
            .push(EcocEntry { field: field.path.clone(), value: set.encrypted_tokens.clone() });
    }
    let server_value = crate::cipher::encrypt(&p.server_encryption, ivs.iv(), &p.value);
    Ok(IndexedValue { kind, key_id: p.index_key_id, bson_type: p.bson_type, server_value, blocks })
}

/// Takes the next ESC position of one token set: its tag and count.
fn take_position<R: EscReader>(
    set: &TokenSet,
    esc: &mut Overlay<'_, R>,
) -> Result<(Token, u64), WriteError<R::Error>> {
    let keys = EscKeys::of(&set.esc);
    let count = state(esc, &keys).map_err(WriteError::Read)?.count + 1;
    esc.write(keys.position_id(count));
    Ok((tokens::tag(&tokens::edc_twice_derived(&set.edc), count), count))
}

/// The value at dotted `path` of `doc`, through embedded documents.
pub fn get_path<'a>(doc: &'a Document, path: &str) -> Option<&'a Value> {
    let mut cur = doc;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let v = cur.get(part)?;
        if parts.peek().is_none() {
            return Some(v);
        }
        cur = v.as_document()?;
    }
    None
}

fn get_path_mut<'a>(doc: &'a mut Document, path: &str) -> Option<&'a mut Value> {
    match path.split_once('.') {
        None => doc.get_mut(path),
        Some((head, rest)) => match doc.get_mut(head)? {
            Value::Document(d) => get_path_mut(d, rest),
            _ => None,
        },
    }
}

/// Replaces every insert payload of `doc` by its indexed value: the document to keep, without
/// `__safeContent__` (the caller adds the tags to what the document had).
pub fn transform<R: EscReader>(
    config: &EncryptedFieldConfig,
    doc: &mut Document,
    esc: &mut Overlay<'_, R>,
    ivs: &mut impl Ivs,
) -> Result<Added, WriteError<R::Error>> {
    let mut added = Added::default();
    for field in &config.fields {
        let Some(v) = get_path_mut(doc, &field.path) else { continue };
        let Some((subtype, bytes)) = subtype_of(v) else { continue };
        match subtype {
            BlobSubtype::Fle2InsertUpdate => {
                let p = InsertUpdatePayload::parse(bytes)?;
                let iev = index(field, &p, esc, ivs, &mut added)?;
                *v = Value::Binary { subtype: t::BINARY_ENCRYPTED, bytes: iev.to_bytes() };
            }
            // What the client sends for an unindexed field is what the collection keeps.
            BlobSubtype::Fle2Unindexed => {}
            // A placeholder or a find payload is no value; the implicit schema refuses it.
            BlobSubtype::Fle2Placeholder
            | BlobSubtype::Fle2FindEquality
            | BlobSubtype::Fle2FindRange
            | BlobSubtype::Fle2FindText => {}
            other => {
                return Err(err(
                    UNEXPECTED_PAYLOAD,
                    format!("Unexpected encrypted payload type: {}", other as u8),
                )
                .into());
            }
        }
    }
    Ok(added)
}

/// Adds `tags` to the `__safeContent__` of `doc`, created at its end if missing.
pub fn add_tags(doc: &mut Document, tags: &[Token]) {
    let values =
        tags.iter().map(|t| Value::Binary { subtype: t::BINARY_GENERIC, bytes: t.0.to_vec() });
    match doc.get_mut(SAFE_CONTENT) {
        Some(Value::Array(a)) => a.extend(values),
        _ => {
            doc.insert(SAFE_CONTENT, Value::Array(values.collect()));
        }
    }
}

/// The ECOC document of an entry, with its `_id`.
pub fn ecoc_doc(id: [u8; 12], e: &EcocEntry) -> Document {
    Document::new()
        .with("_id", Value::ObjectId(id))
        .with("fieldName", e.field.as_str())
        .with("value", Value::Binary { subtype: t::BINARY_GENERIC, bytes: e.value.clone() })
}

/// The ESC document of a position.
pub fn esc_doc(id: &Token) -> Document {
    Document::new().with("_id", Value::Binary { subtype: t::BINARY_GENERIC, bytes: id.0.to_vec() })
}

/// Whether the field is indexed by a query of `kind`.
pub fn indexed_by(field: &EncryptedField, kind: QueryType) -> bool {
    field.has(kind)
}
