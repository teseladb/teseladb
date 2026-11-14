//! `compactStructuredEncryptionData` and `cleanupStructuredEncryptionData` (FR-005): the ECOC
//! entries name, encrypted with each field's ECOC token, the values written since the last run; for
//! each one compaction replaces its new ESC positions with one anchor, and cleanup replaces its
//! positions and anchors with the null anchor. Range fields get padding anchors so that the number
//! of anchors does not tell how many edges the values shared. Measured on a reference replica set
//! member 9.0.4 with known keys.

use teseladb_bson::types as t;
use teseladb_bson::{RawDoc, RawValue};

use crate::cipher;
use crate::error::{FleError, IDL_UNKNOWN_FIELD, TYPE_MISMATCH, err};
use crate::esc::{EscKeys, EscReader, emu_binary, state};
use crate::fields::{EncryptedFieldConfig, QueryType};
use crate::idl::{missing, token_length, wrong_type};
use crate::tokens::{self, Token};

/// The anchor padding factor of a command that gives none.
pub const DEFAULT_PADDING_FACTOR: f64 = 1.0;
/// The default trim factor of a range field (the reference encryption library's
/// `mc_FLERangeTrimFactorDefault`).
const DEFAULT_TRIM_FACTOR: i64 = 6;

/// The compaction tokens of one field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldTokens {
    /// The field's path.
    pub path: String,
    /// Its ECOC token.
    pub ecoc: Token,
    /// `AnchorPaddingTokenRoot`, for a range field.
    pub anchor_padding: Option<Token>,
}

/// Reads `compactionTokens` (`ctx` names it in messages).
pub fn read_tokens(doc: RawDoc<'_>, ctx: &str) -> Result<Vec<FieldTokens>, FleError> {
    let mut out = Vec::new();
    for (path, v) in doc.iter() {
        let token =
            |bytes: &[u8]| Token::from_slice(bytes).ok_or_else(|| token_length("ECOCToken"));
        let (ecoc, anchor_padding) = match v {
            RawValue::Binary { subtype: t::BINARY_GENERIC, bytes } => (token(bytes)?, None),
            RawValue::Document(d) => {
                let mut ecoc = None;
                let mut pad = None;
                for (k, x) in d.iter() {
                    let slot = match k {
                        "ecoc" => &mut ecoc,
                        "anchorPaddingToken" => &mut pad,
                        other => {
                            return Err(err(
                                IDL_UNKNOWN_FIELD,
                                format!("BSON field '{ctx}.{path}.{other}' is an unknown field."),
                            ));
                        }
                    };
                    match x {
                        RawValue::Binary { subtype: t::BINARY_GENERIC, bytes } => {
                            *slot = Some(token(bytes)?);
                        }
                        other => {
                            return Err(wrong_type(
                                &format!("{ctx}.{path}.{k}"),
                                &other,
                                &[t::BINARY],
                            ));
                        }
                    }
                }
                let ecoc = ecoc.ok_or_else(|| missing(&format!("{ctx}.{path}.ecoc")))?;
                (ecoc, pad)
            }
            other => {
                return Err(err(
                    TYPE_MISMATCH,
                    format!(
                        "BSON field '{ctx}.{path}' is the wrong type '{}', expected types \
                         '[binData, object]'",
                        teseladb_mql::types::name_of(other.element_type())
                    ),
                ));
            }
        };
        out.push(FieldTokens { path: path.to_string(), ecoc, anchor_padding });
    }
    Ok(out)
}

/// A change to the ESC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EscOp {
    /// A new document, with its value for an anchor.
    Insert(Token, Option<Vec<u8>>),
    /// The new value of an anchor.
    Update(Token, Vec<u8>),
    /// A document to delete.
    Delete(Token),
}

/// The counters of the reply's `stats.esc`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EscStats {
    /// Documents read.
    pub read: u64,
    /// Documents inserted.
    pub inserted: u64,
    /// Anchors updated.
    pub updated: u64,
    /// Documents deleted.
    pub deleted: u64,
}

/// An [`EscReader`] that counts its reads.
struct Counting<'r, R> {
    inner: &'r mut R,
    reads: u64,
}

impl<R: EscReader> EscReader for Counting<'_, R> {
    type Error = R::Error;
    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, R::Error> {
        self.reads += 1;
        self.inner.get(id)
    }
}

/// One entry of the ECOC: the field and its encrypted ESC token, in the order written.
pub struct EcocEntry<'a> {
    /// `fieldName`.
    pub field: &'a str,
    /// `value`.
    pub value: &'a [u8],
}

/// The edges each value of range field `path` has: the root (without trimming), the leaf and the
/// prefixes at the levels the sparsity keeps from the trim factor (the reference encryption
/// library's `mc_edges_new`).
fn edges_per_value(config: &EncryptedFieldConfig, path: &str) -> Option<u64> {
    let q = config.field(path)?.query().filter(|q| q.kind == QueryType::Range)?;
    let ty = config.field(path)?.bson_type.as_ref()?.0;
    let bits =
        i64::from(crate::range::domain_bits(ty, q.min.as_ref(), q.max.as_ref(), q.precision));
    let sparsity = q.sparsity.unwrap_or(crate::fields::DEFAULT_SPARSITY).max(1);
    let trim = q.trim_factor.map_or(DEFAULT_TRIM_FACTOR.min(bits - 1), i64::from);
    let levels = (trim.max(1)..bits).filter(|i| i % sparsity == 0).count() as u64;
    let root = u64::from(trim == 0);
    Some(root + 1 + levels)
}

/// Compacts (or cleans up, `cleanup`) the ESC from the ECOC entries; the changes to make and
/// the counters. `ivs` draws the IVs of the anchors.
pub fn run<R: EscReader>(
    config: &EncryptedFieldConfig,
    tokens: &[FieldTokens],
    ecoc: &[EcocEntry<'_>],
    esc: &mut R,
    (cleanup, padding_factor): (bool, f64),
    ivs: &mut impl FnMut() -> [u8; 16],
) -> Result<(Vec<EscOp>, EscStats), R::Error> {
    let mut reader = Counting { inner: esc, reads: 0 };
    let mut ops = Vec::new();
    let mut stats = EscStats::default();
    for ft in tokens {
        // A range entry carries one more byte after the token: whether its edge is the leaf.
        let decrypted: Vec<(Token, bool)> = ecoc
            .iter()
            .filter(|e| e.field == ft.path)
            .filter_map(|e| {
                let plain = cipher::decrypt(&ft.ecoc, e.value)?;
                let token = Token::from_slice(plain.get(..tokens::TOKEN_LEN)?)?;
                Some((token, plain.get(tokens::TOKEN_LEN).is_some_and(|b| *b != 0)))
            })
            .collect();
        let mut unique: Vec<Token> = Vec::new();
        let mut leaves: Vec<Token> = Vec::new();
        for (tkn, leaf) in &decrypted {
            if !unique.contains(tkn) {
                unique.push(*tkn);
            }
            if *leaf && !leaves.contains(tkn) {
                leaves.push(*tkn);
            }
        }
        for tkn in &unique {
            let keys = EscKeys::of(tkn);
            let st = state(&mut reader, &keys)?;
            let positions = (st.anchored + 1..=st.count).map(|p| keys.position_id(p));
            if cleanup {
                let first_anchor = st.null.map_or(1, |(a, _)| a + 1);
                let anchors = (first_anchor..=st.apos).map(|a| keys.anchor_id(a));
                let gone: Vec<Token> = anchors.chain(positions).collect();
                if gone.is_empty() && st.null.is_some() {
                    continue;
                }
                stats.deleted += gone.len() as u64;
                ops.extend(gone.into_iter().map(EscOp::Delete));
                let value = keys.anchor_value(st.apos, st.count, ivs());
                if st.null.is_some() {
                    stats.updated += 1;
                    ops.push(EscOp::Update(keys.anchor_id(0), value));
                } else {
                    stats.inserted += 1;
                    ops.push(EscOp::Insert(keys.anchor_id(0), Some(value)));
                }
            } else if st.count > st.anchored {
                let gone: Vec<Token> = positions.collect();
                stats.deleted += gone.len() as u64;
                ops.extend(gone.into_iter().map(EscOp::Delete));
                stats.inserted += 1;
                ops.push(EscOp::Insert(
                    keys.anchor_id(st.apos + 1),
                    Some(keys.anchor_value(0, st.count, ivs())),
                ));
            }
        }
        let Some(root) = ft.anchor_padding else { continue };
        let pads = EscKeys {
            tag: tokens::anchor_padding_key(&root),
            value: tokens::anchor_padding_value(&root),
        };
        let null = reader.get(&pads.anchor_id(0))?.and_then(|v| pads.read_anchor(&v));
        let first = null.map_or(1, |(a, _)| a + 1);
        let last = emu_binary(first, |a| Ok(reader.get(&pads.anchor_id(a))?.is_some()))?;
        if cleanup {
            // The padding anchors give way to their null anchor, which keeps the last number.
            if last < first {
                continue;
            }
            stats.deleted += last - first + 1;
            ops.extend((first..=last).map(|a| EscOp::Delete(pads.anchor_id(a))));
            let value = pads.anchor_value(last, 0, ivs());
            if null.is_some() {
                stats.updated += 1;
                ops.push(EscOp::Update(pads.anchor_id(0), value));
            } else {
                stats.inserted += 1;
                ops.push(EscOp::Insert(pads.anchor_id(0), Some(value)));
            }
            continue;
        }
        let Some(per_value) = edges_per_value(config, &ft.path) else { continue };
        let wanted = (leaves.len() as u64 * per_value).saturating_sub(unique.len() as u64);
        let count = (padding_factor * wanted as f64).ceil() as u64;
        for a in last + 1..=last + count {
            stats.inserted += 1;
            ops.push(EscOp::Insert(pads.anchor_id(a), Some(pads.anchor_value(0, 0, ivs()))));
        }
    }
    stats.read = reader.reads;
    Ok((ops, stats))
}
