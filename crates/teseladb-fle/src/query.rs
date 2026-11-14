//! Queries with find payloads (FR-004): each payload of an equality, range or text query becomes
//! the tags its value has in `__safeContent__`, read from the ESC counts, and the predicate a tag
//! lookup. A filter with no payload is left as it is. The shapes are those of a reference replica
//! set member 9.0.4's rewrite (`explain`): `{__safeContent__: {$elemMatch: {$in: [tags]}}}` in a
//! filter and `{$or: [{$in: [{$const: tag}, "$__safeContent__"]}, …]}` in an expression.

use teseladb_bson::types as t;
use teseladb_bson::{Document, Value};

use crate::error::{NOT_INDEXED_FOR, err};
use crate::esc::{EscKeys, EscReader, state};
use crate::fields::{EncryptedFieldConfig, QueryType, SAFE_CONTENT};
use crate::payloads::{
    FindEqualityPayload, FindRangePayload, FindTextPayload, FindTokens, subtype_of,
};
use crate::subtype::BlobSubtype;
use crate::tokens::{self, Token};
use crate::write::WriteError;

/// The tags of the values a find payload names.
fn tags_of<R: EscReader>(
    esc: &mut R,
    sets: &[FindTokens],
    max_contention: u64,
) -> Result<Vec<Token>, R::Error> {
    let mut out = Vec::new();
    for set in sets {
        for u in 0..=max_contention {
            let edc_cf = set.edc.derive_u64(u);
            let keys = EscKeys::of(&set.esc.derive_u64(u));
            let count = state(esc, &keys)?.count;
            let twice = tokens::edc_twice_derived(&edc_cf);
            out.extend((1..=count).map(|c| tokens::tag(&twice, c)));
        }
    }
    Ok(out)
}

/// What a payload asks for: the tags of its values, `None` for the stub of a range's second
/// bound, which asks for nothing more.
type Asked = Option<Vec<Token>>;

/// Rewrites the payloads of one collection's queries.
pub struct Rewriter<'a, R> {
    config: &'a EncryptedFieldConfig,
    esc: &'a mut R,
}

fn binary(tag: &Token) -> Value {
    Value::Binary { subtype: t::BINARY_GENERIC, bytes: tag.0.to_vec() }
}

/// `{__safeContent__: {$elemMatch: {$eq: tag}}}`, or `$in` for several.
fn elem_match(tags: &[Token]) -> Document {
    let inner = match tags {
        [one] => Document::new().with("$eq", binary(one)),
        many => Document::new().with("$in", many.iter().map(binary).collect::<Vec<_>>()),
    };
    Document::new().with("$elemMatch", inner)
}

/// `{$or: [{$in: [{$const: tag}, "$__safeContent__"]}, …]}`.
fn expr_any(tags: &[Token]) -> Value {
    let safe = format!("${SAFE_CONTENT}");
    let list: Vec<Value> = tags
        .iter()
        .map(|t| {
            Value::Document(Document::new().with(
                "$in",
                vec![
                    Value::Document(Document::new().with("$const", binary(t))),
                    Value::String(safe.clone()),
                ],
            ))
        })
        .collect();
    Value::Document(Document::new().with("$or", list))
}

const EQ: &str = "$eq";
const NE: &str = "$ne";
const IN: &str = "$in";
const NIN: &str = "$nin";
const NOT: &str = "$not";
const RANGE_OPS: [&str; 4] = ["$gt", "$gte", "$lt", "$lte"];
const LOGICAL: [&str; 3] = ["$and", "$or", "$nor"];

/// The text expressions of 9.0 and their argument with the payload.
const TEXT_EXPRESSIONS: [(&str, &str, QueryType); 4] = [
    ("$encStrStartsWith", "prefix", QueryType::Prefix),
    ("$encStrEndsWith", "suffix", QueryType::Suffix),
    ("$encStrContains", "substring", QueryType::Substring),
    ("$encStrNormalizedEq", "string", QueryType::Equality),
];

impl<'a, R: EscReader> Rewriter<'a, R> {
    /// A rewriter for a collection with `config`, reading its ESC through `esc`.
    pub fn new(config: &'a EncryptedFieldConfig, esc: &'a mut R) -> Rewriter<'a, R> {
        Rewriter { config, esc }
    }

    fn check_indexed(&self, path: &str, kind: QueryType) -> Result<(), crate::FleError> {
        let indexed = self.config.field(path).is_some_and(|f| f.has(kind));
        self.indexed_or_refuse(path, kind, indexed)
    }

    /// `$encStrNormalizedEq` asks for the whole string, which every text field indexes.
    fn check_text_indexed(&self, path: &str, kind: QueryType) -> Result<(), crate::FleError> {
        if kind != QueryType::Equality {
            return self.check_indexed(path, kind);
        }
        let text = [QueryType::Prefix, QueryType::Suffix, QueryType::Substring];
        let indexed = self.config.field(path).is_some_and(|f| text.iter().any(|k| f.has(*k)));
        self.indexed_or_refuse(path, kind, indexed)
    }

    fn indexed_or_refuse(
        &self,
        path: &str,
        kind: QueryType,
        indexed: bool,
    ) -> Result<(), crate::FleError> {
        if indexed {
            return Ok(());
        }
        Err(err(
            NOT_INDEXED_FOR,
            format!(
                "Encrypted payload for field '{path}' was generated for query type '{}' but the \
                 field is not indexed for that query type",
                kind.name()
            ),
        ))
    }

    /// The tags a value of a predicate on `path` asks for, if it is a find payload; text
    /// payloads count only in their expressions.
    fn payload(
        &mut self,
        path: &str,
        v: &Value,
        text: Option<QueryType>,
    ) -> Result<Option<Asked>, WriteError<R::Error>> {
        let Some((subtype, bytes)) = subtype_of(v) else { return Ok(None) };
        match subtype {
            BlobSubtype::Fle2FindEquality if text.is_none() => {
                let p = FindEqualityPayload::parse(bytes)?;
                self.check_indexed(path, QueryType::Equality)?;
                let tags =
                    tags_of(self.esc, &[p.tokens], p.max_contention).map_err(WriteError::Read)?;
                Ok(Some(Some(tags)))
            }
            BlobSubtype::Fle2FindRange if text.is_none() => {
                let p = FindRangePayload::parse(bytes)?;
                self.check_indexed(path, QueryType::Range)?;
                match p.edges {
                    None => Ok(Some(None)),
                    Some((sets, cm)) => {
                        let tags = tags_of(self.esc, &sets, cm).map_err(WriteError::Read)?;
                        Ok(Some(Some(tags)))
                    }
                }
            }
            BlobSubtype::Fle2FindText => {
                let Some(kind) = text else { return Ok(None) };
                let p = FindTextPayload::parse(bytes)?;
                self.check_text_indexed(path, kind)?;
                let sets: Vec<FindTokens> = p.tokens().cloned().into_iter().collect();
                let tags = tags_of(self.esc, &sets, p.max_contention).map_err(WriteError::Read)?;
                Ok(Some(Some(tags)))
            }
            _ => Ok(None),
        }
    }

    /// The tags of every payload in `list`, if they all are payloads.
    fn payload_list(
        &mut self,
        path: &str,
        list: &[Value],
    ) -> Result<Option<Vec<Token>>, WriteError<R::Error>> {
        let mut all = Vec::new();
        for v in list {
            match self.payload(path, v, None)? {
                Some(Some(tags)) => all.extend(tags),
                Some(None) => {}
                None => return Ok(None),
            }
        }
        Ok(Some(all))
    }

    /// Rewrites a filter (of `find`, `count`, `delete`, `update`, `$match`…).
    pub fn filter(&mut self, filter: &Document) -> Result<Document, WriteError<R::Error>> {
        let mut kept = Document::new();
        let mut clauses: Vec<Document> = Vec::new();
        for (k, v) in filter.iter() {
            if LOGICAL.contains(&k) {
                if let Value::Array(items) = v {
                    let mut out = Vec::with_capacity(items.len());
                    for item in items {
                        out.push(match item {
                            Value::Document(d) => Value::Document(self.filter(d)?),
                            other => other.clone(),
                        });
                    }
                    kept.push(k, out);
                } else {
                    kept.push(k, v.clone());
                }
                continue;
            }
            if k == "$expr" {
                match self.top_expr(v)? {
                    Some(clause) => clauses.push(clause),
                    None => kept.push(k, self.expr(v)?),
                }
                continue;
            }
            if k.starts_with('$') {
                kept.push(k, v.clone());
                continue;
            }
            match self.predicate(k, v)? {
                Some((own, rest)) => {
                    clauses.extend(own);
                    if let Some(rest) = rest {
                        kept.push(k, rest);
                    }
                }
                None => kept.push(k, v.clone()),
            }
        }
        if clauses.is_empty() {
            return Ok(kept);
        }
        if kept.is_empty() && clauses.len() == 1 {
            return Ok(clauses.remove(0));
        }
        let mut all: Vec<Value> = clauses.into_iter().map(Value::Document).collect();
        if !kept.is_empty() {
            all.push(Value::Document(kept));
        }
        Ok(Document::new().with("$and", all))
    }

    /// The clauses of a predicate on field `path`, and what is left of it, if it has payloads.
    #[allow(clippy::type_complexity)]
    fn predicate(
        &mut self,
        path: &str,
        v: &Value,
    ) -> Result<Option<(Vec<Document>, Option<Value>)>, WriteError<R::Error>> {
        let safe = |m: Document| Document::new().with(SAFE_CONTENT, m);
        if let Some(tags) = self.payload(path, v, None)? {
            return Ok(Some((tags.map(|t| safe(elem_match(&t))).into_iter().collect(), None)));
        }
        let Value::Document(ops) = v else { return Ok(None) };
        if ops.iter().next().is_none_or(|(k, _)| !k.starts_with('$')) {
            return Ok(None);
        }
        let mut clauses = Vec::new();
        let mut rest = Document::new();
        let mut found = false;
        for (op, arg) in ops.iter() {
            let tags = match op {
                EQ | NE => self.payload(path, arg, None)?,
                op if RANGE_OPS.contains(&op) => self.payload(path, arg, None)?,
                IN | NIN => match arg {
                    Value::Array(list) => self.payload_list(path, list)?.map(Some),
                    _ => None,
                },
                NOT => match arg {
                    Value::Document(inner) => match inner.get(EQ) {
                        Some(p) if inner.len() == 1 => self.payload(path, p, None)?,
                        _ => None,
                    },
                    _ => None,
                },
                _ => None,
            };
            let Some(tags) = tags else {
                rest.push(op, arg.clone());
                continue;
            };
            found = true;
            let Some(tags) = tags else { continue };
            let m = elem_match(&tags);
            let negated = matches!(op, NE | NIN | NOT);
            clauses.push(if negated { safe(Document::new().with(NOT, m)) } else { safe(m) });
        }
        if !found {
            return Ok(None);
        }
        Ok(Some((clauses, (!rest.is_empty()).then_some(Value::Document(rest)))))
    }

    /// The field path of an expression argument (`"$ssn"`).
    fn field_ref(v: &Value) -> Option<&str> {
        match v {
            Value::String(s) => s.strip_prefix('$').filter(|p| !p.starts_with('$')),
            _ => None,
        }
    }

    /// An expression argument without its `$const`: a driver with automatic encryption sends the
    /// payloads of an expression as `{$const: payload}` (measured with the reference query analysis
    /// daemon 9.0.4).
    fn constant(v: &Value) -> &Value {
        match v {
            Value::Document(d) if d.len() == 1 => d.get("$const").unwrap_or(v),
            _ => v,
        }
    }

    /// The tags an expression asks for, if it is one over a payload, and whether it is negated.
    fn expr_tags(&mut self, e: &Document) -> Result<Option<(Asked, bool)>, WriteError<R::Error>> {
        let Some((op, arg)) = e.iter().next().filter(|_| e.len() == 1) else { return Ok(None) };
        if let Some((_, key, kind)) = TEXT_EXPRESSIONS.iter().find(|(name, _, _)| *name == op) {
            let Value::Document(a) = arg else { return Ok(None) };
            let (Some(path), Some(p)) = (a.get("input").and_then(Self::field_ref), a.get(key))
            else {
                return Ok(None);
            };
            let path = path.to_string();
            return Ok(self.payload(&path, Self::constant(p), Some(*kind))?.map(|t| (t, false)));
        }
        let Value::Array(args) = arg else { return Ok(None) };
        let negated = op == NE;
        match (op, args.as_slice()) {
            (op, [a, b]) if op == EQ || op == NE || RANGE_OPS.contains(&op) => {
                let (path, p) = match (Self::field_ref(a), Self::field_ref(b)) {
                    (Some(path), _) => (path.to_string(), b),
                    (None, Some(path)) => (path.to_string(), a),
                    _ => return Ok(None),
                };
                Ok(self.payload(&path, Self::constant(p), None)?.map(|t| (t, negated)))
            }
            (IN, [a, Value::Array(list)]) => {
                let Some(path) = Self::field_ref(a) else { return Ok(None) };
                let path = path.to_string();
                let list: Vec<Value> = list.iter().map(|v| Self::constant(v).clone()).collect();
                Ok(self.payload_list(&path, &list)?.map(|t| (Some(t), false)))
            }
            _ => Ok(None),
        }
    }

    /// A whole `$expr` that is one expression over a payload, as a filter clause.
    fn top_expr(&mut self, v: &Value) -> Result<Option<Document>, WriteError<R::Error>> {
        let Value::Document(e) = v else { return Ok(None) };
        match self.expr_tags(e)? {
            Some((Some(tags), false)) => {
                Ok(Some(Document::new().with(SAFE_CONTENT, elem_match(&tags))))
            }
            Some((None, false)) => Ok(Some(Document::new())),
            _ => Ok(None),
        }
    }

    /// Rewrites an aggregation expression.
    pub fn expr(&mut self, v: &Value) -> Result<Value, WriteError<R::Error>> {
        match v {
            Value::Document(e) => {
                if let Some((tags, negated)) = self.expr_tags(e)? {
                    let any = match tags {
                        Some(t) => expr_any(&t),
                        None => Value::Boolean(true),
                    };
                    return Ok(if negated {
                        Value::Document(Document::new().with(NOT, vec![any]))
                    } else {
                        any
                    });
                }
                let mut out = Document::new();
                for (k, x) in e.iter() {
                    out.push(k, self.expr(x)?);
                }
                Ok(Value::Document(out))
            }
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for x in items {
                    out.push(self.expr(x)?);
                }
                Ok(Value::Array(out))
            }
            other => Ok(other.clone()),
        }
    }
}
