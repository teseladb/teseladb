//! The reference evaluator (010 FR-020, T003): a pipeline over a snapshot of collections held in
//! memory, each stage run on the whole output of the one before it, without moving, splitting or
//! pushing any stage. It is the oracle of the `queries` load of the simulator, which compares the
//! distributed result with it at the same mark, and it gives the same answers for the same seed.

use std::cell::Cell;
use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::{Extensions, GeoQuery, MqlError, NoExtensions, TextQuery};

use crate::expr::{Ctx, Expr, Scope, truthy};
use crate::pipeline::{Env, Pipeline};
use crate::raw::bad;

/// A snapshot of collections and the variables of a command.
#[derive(Debug, Default)]
pub struct Reference {
    collections: BTreeMap<String, Vec<Document>>,
    vars: Option<Document>,
    state: Cell<u64>,
}

impl Reference {
    /// An empty snapshot whose random numbers (`$sample`, `$rand`) follow `seed`.
    pub fn new(seed: u64) -> Reference {
        Reference { collections: BTreeMap::new(), vars: None, state: Cell::new(seed) }
    }

    /// The snapshot with collection `name` holding `docs`, in their order.
    pub fn with_collection(mut self, name: &str, docs: Vec<Document>) -> Reference {
        self.collections.insert(name.to_owned(), docs);
        self
    }

    /// The snapshot with the command's variables `vars` (`let`).
    pub fn with_vars(mut self, vars: Document) -> Reference {
        self.vars = Some(vars);
        self
    }

    /// The documents of collection `name` (none when the snapshot has no such collection).
    pub fn collection_of(&self, name: &str) -> Vec<Document> {
        self.collections.get(name).cloned().unwrap_or_default()
    }

    /// The documents `stages` give over collection `coll` of the snapshot.
    pub fn evaluate(&self, coll: &str, stages: &[Document]) -> Result<Vec<Document>, MqlError> {
        let bytes: Vec<Vec<u8>> =
            stages.iter().map(Document::to_bytes).collect::<Result<_, _>>().map_err(bad)?;
        let raws: Vec<RawDoc<'_>> =
            bytes.iter().map(|b| RawDoc::from_bytes(b)).collect::<Result<_, _>>().map_err(bad)?;
        let values: Vec<RawValue<'_>> = raws.into_iter().map(RawValue::Document).collect();
        let names: Vec<String> =
            self.vars.iter().flat_map(|v| v.iter().map(|(k, _)| k.to_owned())).collect();
        let ctx = Ctx::checked(names.iter().map(String::as_str));
        let pipeline = Pipeline::parse_command(&values, &ctx, true)?;
        pipeline.run_in(self.collection_of(coll), self, false)
    }

    /// The next uniform number in `[0, 1)` of the seed's sequence.
    fn next(&self) -> f64 {
        let s = self.state.get().wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        self.state.set(s);
        (s >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Env for Reference {
    fn ext(&self) -> &dyn Extensions {
        self
    }

    fn vars(&self) -> Option<&Document> {
        self.vars.as_ref()
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(self.collection_of(name))
    }

    fn random(&self) -> f64 {
        self.next()
    }
}

/// `$expr` and the expressions of projections from the expression engine; the operators of
/// other engines (JavaScript, text, geometry) as a node without them.
impl Extensions for Reference {
    fn expr(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<bool, MqlError> {
        let d = Document::from(doc);
        Ok(truthy(&Expr::parse(e)?.eval(&d, &Scope::new(d.clone(), vars))?))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        NoExtensions.where_js(code, doc)
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        NoExtensions.text(query, doc)
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool, MqlError> {
        NoExtensions.geo(query, value)
    }

    fn random(&self) -> f64 {
        self.next()
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>, MqlError> {
        NoExtensions.meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>, MqlError> {
        let d = Document::from(doc);
        Expr::parse(e)?.eval(&d, &Scope::new(d.clone(), vars))
    }

    fn update_pipeline(
        &self,
        p: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document, MqlError> {
        NoExtensions.update_pipeline(p, doc, vars)
    }
}
