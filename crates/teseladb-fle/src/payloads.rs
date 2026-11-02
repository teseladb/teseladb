//! The payloads drivers send (FR-001): the insert and update payload of an indexed field (subtype
//! 11) and the find payloads of equality (12), range (13) and text (18) queries. Each is one byte
//! of subtype followed by a BSON document whose fields are single letters
//! (librefcrypt's `mc-fle2-*-payload-*`).

use teseladb_bson::types as t;
use teseladb_bson::{RawDoc, RawValue, Value};

use crate::error::{BAD_VALUE, FleError, INVALID_BSON, OVERFLOW, err};
use crate::idl::Fields;
use crate::subtype::BlobSubtype;
use crate::tokens::Token;

/// The tokens of one edge of a range value or one string of a text value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenSet {
    /// `EDCDerivedFromDataTokenAndContentionFactor`.
    pub edc: Token,
    /// `ESCDerivedFromDataTokenAndContentionFactor`.
    pub esc: Token,
    /// `ServerDerivedFromDataToken`.
    pub server: Token,
    /// The ESC token encrypted with the ECOC token, for the compaction log.
    pub encrypted_tokens: Vec<u8>,
}

/// The token sets of a text value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextTokenSets {
    /// The whole string (always there once read).
    pub exact: Option<TokenSet>,
    /// Each substring.
    pub substrings: Vec<TokenSet>,
    /// Each suffix.
    pub suffixes: Vec<TokenSet>,
    /// Each prefix.
    pub prefixes: Vec<TokenSet>,
}

/// `FLE2InsertUpdatePayloadV2`: an indexed field of an insert or an update.
#[derive(Clone, Debug, PartialEq)]
pub struct InsertUpdatePayload {
    /// The value's own tokens.
    pub tokens: TokenSet,
    /// The index key's id (`u`).
    pub index_key_id: [u8; 16],
    /// The BSON type of the value in clear (`t`).
    pub bson_type: u8,
    /// The user key id and the value the client encrypted (`v`).
    pub value: Vec<u8>,
    /// `ServerDataEncryptionLevel1Token` (`e`).
    pub server_encryption: Token,
    /// The contention factor the driver drew (`k`).
    pub contention: u64,
    /// The edges of a range value (`g`).
    pub edges: Option<Vec<TokenSet>>,
    /// Range options: sparsity, precision and trim factor.
    pub sparsity: Option<i64>,
    /// Precision.
    pub precision: Option<i32>,
    /// Trim factor.
    pub trim_factor: Option<i32>,
    /// Index minimum.
    pub min: Option<Value>,
    /// Index maximum.
    pub max: Option<Value>,
    /// The token sets of a text value (`b`).
    pub text: Option<TextTokenSets>,
}

/// The BSON document after the subtype byte of a payload.
pub fn body(bytes: &[u8]) -> Result<RawDoc<'_>, FleError> {
    let Some((_, doc)) = bytes.split_first() else {
        return Err(err(
            OVERFLOW,
            "buffer size too small to read (1) bytes out of buffer[0] at offset: 0",
        ));
    };
    RawDoc::from_bytes(doc).map_err(|_| {
        err(
            INVALID_BSON,
            "BSON data has to be at least 5 bytes in element with field name '?' in object with \
             unknown _id",
        )
    })
}

const ROOT: &str = "root";

const TOKEN_SET_FIELDS: [&str; 4] = ["d", "s", "l", "p"];
const EDC_CF: &str = "EDCDerivedFromDataTokenAndContentionFactor";
const ESC_CF: &str = "ESCDerivedFromDataTokenAndContentionFactor";
const SERVER_DERIVED: &str = "ServerDerivedFromDataToken";
const SERVER_ENCRYPTION: &str = "ServerDataEncryptionLevel1Token";

fn token_set(doc: RawDoc<'_>, context: &str) -> Result<TokenSet, FleError> {
    let f = Fields::read(doc, context, &TOKEN_SET_FIELDS)?;
    let (edc, esc) = (f.token("d", EDC_CF)?, f.token("s", ESC_CF)?);
    let (server, p) = (f.token("l", SERVER_DERIVED)?, f.binary("p", t::BINARY_GENERIC)?);
    f.require(&TOKEN_SET_FIELDS)?;
    Ok(TokenSet {
        edc: edc.unwrap_or_default(),
        esc: esc.unwrap_or_default(),
        server: server.unwrap_or_default(),
        encrypted_tokens: p.unwrap_or_default().to_vec(),
    })
}

fn token_sets(values: Vec<RawValue<'_>>, context: &str) -> Result<Vec<TokenSet>, FleError> {
    values
        .into_iter()
        .map(|v| match v {
            RawValue::Document(d) => token_set(d, context),
            other => Err(crate::idl::wrong_type(context, &other, &[t::DOCUMENT])),
        })
        .collect()
}

fn text_sets(doc: RawDoc<'_>) -> Result<TextTokenSets, FleError> {
    const FIELDS: [&str; 4] = ["e", "s", "u", "p"];
    let f = Fields::read(doc, "TextSearchTokenSets", &FIELDS)?;
    let exact = f.document("e")?.map(|d| token_set(d, "e")).transpose()?;
    let list = |k: &str| -> Result<Vec<TokenSet>, FleError> {
        token_sets(f.array(k)?.unwrap_or_default(), k)
    };
    let (substrings, suffixes, prefixes) = (list("s")?, list("u")?, list("p")?);
    f.require(&FIELDS)?;
    Ok(TextTokenSets { exact, substrings, suffixes, prefixes })
}

impl InsertUpdatePayload {
    const FIELDS: [&str; 16] =
        ["d", "s", "p", "u", "t", "v", "e", "l", "k", "g", "sp", "pn", "tf", "mn", "mx", "b"];
    const REQUIRED: [&str; 9] = ["d", "s", "p", "u", "t", "v", "e", "l", "k"];

    /// Reads the payload of a binary whose first byte is 11.
    pub fn parse(bytes: &[u8]) -> Result<InsertUpdatePayload, FleError> {
        let f = Fields::read(body(bytes)?, ROOT, &Self::FIELDS)?;
        let (edc, esc) = (f.token("d", EDC_CF)?, f.token("s", ESC_CF)?);
        let p = f.binary("p", t::BINARY_GENERIC)?.unwrap_or_default().to_vec();
        let index_key_id = f.uuid("u")?.unwrap_or_default();
        let bson_type = f.int32("t")?.unwrap_or_default();
        let value = f.binary("v", t::BINARY_GENERIC)?.unwrap_or_default().to_vec();
        let server_encryption = f.token("e", SERVER_ENCRYPTION)?.unwrap_or_default();
        let server = f.token("l", SERVER_DERIVED)?.unwrap_or_default();
        let k = f.int64("k")?.unwrap_or_default();
        let contention = u64::try_from(k).map_err(|_| {
            err(BAD_VALUE, format!("BSON field 'k' value must be >= 0, actual value '{k}'"))
        })?;
        let edges = f.array("g")?.map(|g| token_sets(g, "EdgeTokenSetV2")).transpose()?;
        let text = f.document("b")?.map(text_sets).transpose()?;
        f.require(&Self::REQUIRED)?;
        let (edc, esc) = (edc.unwrap_or_default(), esc.unwrap_or_default());
        Ok(InsertUpdatePayload {
            tokens: TokenSet { edc, esc, server, encrypted_tokens: p },
            index_key_id,
            bson_type: u8::try_from(bson_type).unwrap_or(0),
            value,
            server_encryption,
            contention,
            edges,
            sparsity: f.int64("sp")?,
            precision: f.int32("pn")?,
            trim_factor: f.int32("tf")?,
            min: f.get("mn").map(Value::from),
            max: f.get("mx").map(Value::from),
            text,
        })
    }

    /// Every token set of the value: the value's own, then its edges or its text strings, in the
    /// order of the indexed value's metadata blocks.
    pub fn all_sets(&self) -> Vec<&TokenSet> {
        if let Some(edges) = &self.edges {
            return edges.iter().collect();
        }
        if let Some(text) = &self.text {
            let mut sets: Vec<&TokenSet> = text.exact.iter().collect();
            sets.extend(&text.substrings);
            sets.extend(&text.suffixes);
            sets.extend(&text.prefixes);
            return sets;
        }
        vec![&self.tokens]
    }
}

/// The tokens of one value to look for: one per edge of a range or the value of an equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindTokens {
    /// `EDCDerivedFromDataToken`.
    pub edc: Token,
    /// `ESCDerivedFromDataToken`.
    pub esc: Token,
    /// `ServerDerivedFromDataToken`.
    pub server: Token,
}

const FIND_FIELDS: [&str; 3] = ["d", "s", "l"];

fn find_tokens(doc: RawDoc<'_>, context: &str) -> Result<FindTokens, FleError> {
    let f = Fields::read(doc, context, &FIND_FIELDS)?;
    tokens_of(&f)
}

/// The three find tokens, after their types, then whether they are all there.
fn tokens_of(f: &Fields<'_>) -> Result<FindTokens, FleError> {
    let edc = f.token("d", "EDCDerivedFromDataToken")?.unwrap_or_default();
    let esc = f.token("s", "ESCDerivedFromDataToken")?.unwrap_or_default();
    let server = f.token("l", SERVER_DERIVED)?.unwrap_or_default();
    f.require(&FIND_FIELDS)?;
    Ok(FindTokens { edc, esc, server })
}

/// `FLE2FindEqualityPayloadV2`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindEqualityPayload {
    /// The value's tokens.
    pub tokens: FindTokens,
    /// The highest contention factor to look at (`cm`).
    pub max_contention: u64,
}

fn max_contention(f: &Fields<'_>) -> Result<u64, FleError> {
    let cm = f.int64("cm")?.unwrap_or(0);
    u64::try_from(cm).map_err(|_| {
        err(BAD_VALUE, format!("BSON field 'cm' value must be >= 0, actual value '{cm}'"))
    })
}

impl FindEqualityPayload {
    /// Reads the payload of a binary whose first byte is 12.
    pub fn parse(bytes: &[u8]) -> Result<FindEqualityPayload, FleError> {
        let f = Fields::read(body(bytes)?, ROOT, &["d", "s", "l", "cm"])?;
        let max_contention = max_contention(&f)?;
        Ok(FindEqualityPayload { tokens: tokens_of(&f)?, max_contention })
    }
}

/// `FLE2FindRangePayloadV2`: one side of a range query, or its stub.
#[derive(Clone, Debug, PartialEq)]
pub struct FindRangePayload {
    /// The edges to look for and the highest contention factor, absent in the stub of the
    /// second bound of a two-sided query.
    pub edges: Option<(Vec<FindTokens>, u64)>,
    /// Pairs the two operators of one query (`payloadId`).
    pub payload_id: i32,
    /// The first operator (`firstOperator`).
    pub first_operator: i32,
    /// The second operator (`secondOperator`).
    pub second_operator: Option<i32>,
}

impl FindRangePayload {
    const FIELDS: [&str; 9] =
        ["payload", "payloadId", "firstOperator", "secondOperator", "sp", "pn", "tf", "mn", "mx"];

    /// Reads the payload of a binary whose first byte is 13.
    pub fn parse(bytes: &[u8]) -> Result<FindRangePayload, FleError> {
        let f = Fields::read(body(bytes)?, ROOT, &Self::FIELDS)?;
        f.require(&["payloadId", "firstOperator"])?;
        let edges = match f.document("payload")? {
            None => None,
            Some(p) => {
                let pf = Fields::read(p, "payload", &["g", "cm"])?;
                pf.require(&["g", "cm"])?;
                let g = pf.array("g")?.unwrap_or_default();
                let tokens = g
                    .into_iter()
                    .map(|v| match v {
                        RawValue::Document(d) => find_tokens(d, "EdgeFindTokenSetV2"),
                        other => Err(crate::idl::wrong_type("g", &other, &[t::DOCUMENT])),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Some((tokens, max_contention(&pf)?))
            }
        };
        Ok(FindRangePayload {
            edges,
            payload_id: f.int32("payloadId")?.unwrap_or(0),
            first_operator: f.int32("firstOperator")?.unwrap_or(0),
            second_operator: f.int32("secondOperator")?,
        })
    }
}

/// `FLE2FindTextPayload`: the tokens of a text query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FindTextPayload {
    /// The tokens of a whole string match.
    pub exact: Option<FindTokens>,
    /// The tokens of a substring match.
    pub substring: Option<FindTokens>,
    /// The tokens of a suffix match.
    pub suffix: Option<FindTokens>,
    /// The tokens of a prefix match.
    pub prefix: Option<FindTokens>,
    /// The highest contention factor to look at.
    pub max_contention: u64,
    /// Case folding.
    pub case_fold: bool,
    /// Diacritic folding.
    pub diacritic_fold: bool,
}

impl FindTextPayload {
    const FIELDS: [&str; 7] = ["ts", "cm", "cf", "df", "ss", "fs", "ps"];

    /// Reads the payload of a binary whose first byte is 18.
    pub fn parse(bytes: &[u8]) -> Result<FindTextPayload, FleError> {
        let f = Fields::read(body(bytes)?, ROOT, &Self::FIELDS)?;
        let ts = f.document("ts")?;
        let (max_contention, case_fold) = (max_contention(&f)?, f.boolean("cf")?.unwrap_or(false));
        let diacritic_fold = f.boolean("df")?.unwrap_or(false);
        let tf = ts.map(|d| Fields::read(d, "ts", &["e", "s", "u", "p"])).transpose()?;
        let one = |k: &str| -> Result<Option<FindTokens>, FleError> {
            match &tf {
                Some(tf) => tf.document(k)?.map(|d| find_tokens(d, k)).transpose(),
                None => Ok(None),
            }
        };
        let payload = FindTextPayload {
            exact: one("e")?,
            substring: one("s")?,
            suffix: one("u")?,
            prefix: one("p")?,
            max_contention,
            case_fold,
            diacritic_fold,
        };
        f.require(&["ts", "cm", "cf", "df"])?;
        Ok(payload)
    }

    /// The tokens of the query, whichever of the four kinds it is.
    pub fn tokens(&self) -> Option<&FindTokens> {
        self.exact
            .as_ref()
            .or(self.substring.as_ref())
            .or(self.suffix.as_ref())
            .or(self.prefix.as_ref())
    }
}

/// The subtype of an encrypted binary value, if it is one.
pub fn subtype_of(v: &Value) -> Option<(BlobSubtype, &[u8])> {
    match v {
        Value::Binary { subtype: t::BINARY_ENCRYPTED, bytes } => {
            BlobSubtype::of(bytes).map(|s| (s, bytes.as_slice()))
        }
        _ => None,
    }
}
