//! `$text` over the collection's text index (011 FR-001, T029): the documents the filter of a
//! pipeline's first `$match` selects, each with its `textScore`, as a RefDb 9.0.4 replica set
//! gives them (measured 02-11-2026, `scratchpad/lane3m/m33.js` and `m37.js`). The text index
//! keeps no entries of its own: each query reads the collection and scores what it reads with
//! [`teseladb_search::text_index`].

use std::collections::BTreeMap;

use teseladb_exec::pipeline::search::Scored;
use teseladb_meta::catalog::{CollectionEntry, Namespace};
use teseladb_mql::{
    Extensions, Filter, FilterUse, GeoQuery, MatchContext, MqlError, Result as MqlResult,
};
use teseladb_range::read::ReadConcern;
use teseladb_search::text_index::{Language, Sensitivity, TextIndex, TextQuery};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::Backend;
use super::engines::NodeExtensions;
use super::mutation::from_mql;
use super::query::{documents_of, for_each_match_with};
use crate::error::{BAD_VALUE, CommandError, INDEX_NOT_FOUND};

/// The metadata name of the score.
pub const TEXT_SCORE: &str = "textScore";

/// The error of a `$text` on a collection without a text index (measured: 27).
fn no_index(ns: &Namespace, missing: bool) -> CommandError {
    let tail = if missing {
        format!(" (no such collection '{}.{}')", ns.db, ns.coll)
    } else {
        String::new()
    };
    CommandError::new(INDEX_NOT_FOUND, format!("text index required for $text query{tail}"))
}

/// The error of a `$text` in a pipeline that reads no collection of its own (a view's).
pub fn no_index_here() -> CommandError {
    CommandError::new(INDEX_NOT_FOUND, "text index required for $text query")
}

fn text_error(e: teseladb_search::text_index::TextError) -> MqlError {
    MqlError::new(teseladb_mql::ErrorCode { code: e.code, name: "BadValue" }, e.message)
}

/// The text index of the collection, from the catalog's ready indexes.
fn text_index<B: Backend + ?Sized>(
    backend: &mut B,
    coll: u32,
) -> Result<Option<TextIndex>, CommandError> {
    for entry in backend.indexes(coll)? {
        if entry.state != teseladb_meta::indexes::IndexState::Ready {
            continue;
        }
        let spec = Document::from(RawDoc::from_bytes(&entry.spec)?);
        if teseladb_index::spec::is_text(RawDoc::from_bytes(&entry.spec)?) {
            let index =
                TextIndex::from_spec(&spec).map_err(|e| CommandError::new(BAD_VALUE, e.message))?;
            return Ok(Some(index));
        }
    }
    Ok(None)
}

/// The extensions of a `$text` filter: the text operator scores the document with the index,
/// and everything else is the node's.
struct TextExtensions<'a> {
    index: &'a TextIndex,
}

impl TextExtensions<'_> {
    fn query(&self, q: &teseladb_mql::TextQuery) -> MqlResult<TextQuery> {
        let language = match &q.language {
            Some(l) => Language::named(l).map_err(text_error)?,
            None => self.index.language(),
        };
        let s = Sensitivity { case: q.case_sensitive, diacritic: q.diacritic_sensitive };
        Ok(TextQuery::parse(&q.search, language, s))
    }

    /// The score of `doc` for `q`, `None` when it does not match.
    fn score(&self, q: &teseladb_mql::TextQuery, doc: RawDoc<'_>) -> MqlResult<Option<f64>> {
        let query = self.query(q)?;
        Ok(self.index.score(&query, &Document::from(doc)))
    }
}

impl Extensions for TextExtensions<'_> {
    fn expr(&self, e: RawValue<'_>, doc: RawDoc<'_>, vars: Option<&Document>) -> MqlResult<bool> {
        NodeExtensions.expr(e, doc, vars)
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> MqlResult<bool> {
        NodeExtensions.where_js(code, doc)
    }

    fn text(&self, q: &teseladb_mql::TextQuery, doc: RawDoc<'_>) -> MqlResult<bool> {
        Ok(self.score(q, doc)?.is_some())
    }

    fn geo(&self, q: &GeoQuery, value: RawValue<'_>) -> MqlResult<bool> {
        NodeExtensions.geo(q, value)
    }

    fn random(&self) -> f64 {
        NodeExtensions.random()
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> MqlResult<Option<Value>> {
        NodeExtensions.meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> MqlResult<Option<Value>> {
        NodeExtensions.eval(e, doc, vars)
    }

    fn update_pipeline(
        &self,
        p: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> MqlResult<Document> {
        NodeExtensions.update_pipeline(p, doc, vars)
    }
}

/// How many `$text` clauses `filter` holds, refusing one under `$nor` (measured: code 2).
fn count_text(filter: &Document, under_nor: bool) -> Result<usize, CommandError> {
    let mut n = 0;
    for (k, v) in filter.iter() {
        match (k, v) {
            ("$text", _) if under_nor => {
                return Err(CommandError::new(BAD_VALUE, "text expression not allowed in nor"));
            }
            ("$text", _) => n += 1,
            ("$and" | "$or" | "$nor", Value::Array(items)) => {
                // A parsed filter: its `$and`, `$or` and `$nor` hold documents.
                for d in items.iter().filter_map(Value::as_document) {
                    n += count_text(d, under_nor || k == "$nor")?;
                }
            }
            _ => {}
        }
    }
    Ok(n)
}

/// The collection a `$text` on `ns` reads: the one a view without stages stands on, through the
/// views it stands on (measured on 9.0.4: an `aggregate` on such a view, or a `$lookup` or
/// `$unionWith` of one, reads its collection's text index; a view with stages is refused
/// before, `scratchpad/lane3t/m97.js` and `m100.js`).
pub fn read_namespace<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
) -> Result<Namespace, CommandError> {
    Ok(match super::views::resolve(backend, ns)? {
        Some(v) => super::names::ns(&ns.db, &v.collection),
        None => ns.clone(),
    })
}

/// Plans the `$text` of `filter` on `ns`: one text expression, the collection's text index and
/// the query's `$language`, in `refnode`'s order (measured: the missing index comes first).
pub fn check<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    filter: &Document,
) -> Result<(), CommandError> {
    let parsed = parse_filter(filter)?;
    planned(backend, ns, filter, &parsed).map(|_| ())
}

/// `filter`, parsed as a query's.
fn parse_filter(filter: &Document) -> Result<Filter, CommandError> {
    let bytes = filter.to_bytes()?;
    Filter::parse(RawDoc::from_bytes(&bytes)?, FilterUse::Query).map_err(from_mql)
}

/// The collection of `ns` and its text index, once the `$text` of `filter` is planned.
fn planned<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    filter: &Document,
    parsed: &Filter,
) -> Result<(CollectionEntry, TextIndex), CommandError> {
    if count_text(filter, false)? > 1 {
        return Err(CommandError::new(BAD_VALUE, "Too many text expressions"));
    }
    let Some(coll) = backend.collection(ns)? else { return Err(no_index(ns, true)) };
    let Some(index) = text_index(backend, coll.id)? else { return Err(no_index(ns, false)) };
    let ext = TextExtensions { index: &index };
    parsed.text_query().map(|q| ext.query(q)).transpose().map_err(from_mql)?;
    Ok((coll, index))
}

/// The documents of `ns` that `filter` (with a `$text`) selects, each with its `textScore`
/// (measured: the score of the text clause, also when an `$or` let the document in); its `$expr`
/// sees the variables `vars`.
pub fn source<B: Backend + ?Sized>(
    backend: &mut B,
    ns: &Namespace,
    filter: &Document,
    (vars, concern): (Option<&Document>, ReadConcern),
) -> Result<Vec<Scored>, CommandError> {
    let parsed = parse_filter(filter)?;
    let (coll, index) = planned(backend, ns, filter, &parsed)?;
    let ext = TextExtensions { index: &index };
    let query = parsed.text_query();
    let ctx = MatchContext { collation: None, ext: &ext, vars };
    let mut out = Vec::new();
    for_each_match_with(
        backend,
        documents_of(&coll),
        &Document::new(),
        None,
        None,
        concern,
        |_, raw| {
            let doc = RawDoc::from_bytes(raw)?;
            let found = parsed.matches(doc, &ctx).map_err(from_mql)?;
            if !found {
                return Ok(true);
            }
            let score = match query {
                Some(q) => ext.score(q, doc).map_err(from_mql)?.unwrap_or(0.0),
                None => 0.0,
            };
            let meta: BTreeMap<String, Value> =
                BTreeMap::from([(TEXT_SCORE.to_owned(), Value::Double(score))]);
            out.push(Scored { doc: Document::from(doc), meta });
            Ok(true)
        },
    )?;
    Ok(out)
}
