//! The environments of the pipelines of `$lookup` and `$unionWith`: the variables of a
//! `$lookup`'s `let`, and the collection the stage reads, whose text and search indexes a first
//! `$match` with `$text` or search stage reads (011 T054, FR-004).

use teseladb_bson::Document;
use teseladb_mql::{Extensions, MqlError};

use super::Env;
use crate::expr::Scope;

/// The variables of `$lookup`'s `let`: those bound to a value, and those bound to a missing one
/// (`$$kk` of a missing field is missing, not `null`).
#[derive(Clone, Debug, Default)]
pub(super) struct LetVars {
    pub(super) set: Document,
    pub(super) missing: Vec<String>,
}

impl LetVars {
    /// A scope over `doc` with these variables.
    pub(super) fn scope(&self, doc: &Document) -> Scope {
        let mut s = Scope::new(doc.clone(), Some(&self.set));
        for n in &self.missing {
            s.vars.insert(n.clone(), None);
        }
        s
    }
}

/// An environment with more variables (`$lookup`'s `let`).
pub(super) struct WithVars<'a> {
    pub(super) inner: &'a dyn Env,
    pub(super) vars: &'a LetVars,
}

impl Env for WithVars<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn vars(&self) -> Option<&Document> {
        Some(&self.vars.set)
    }

    fn scope(&self, doc: &Document) -> Scope {
        self.vars.scope(doc).with_collation(self.inner.collation()).with_js(self.inner.js())
    }

    fn collation(&self) -> crate::collation::Collation {
        self.inner.collation()
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        self.inner.collection(name)
    }

    fn random(&self) -> f64 {
        self.inner.random()
    }

    fn js(&self) -> Option<std::rc::Rc<dyn crate::expr::js::JsEngine>> {
        self.inner.js()
    }

    fn joined_text_source(
        &self,
        name: &str,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_text_source(name, filter, vars)
    }

    fn joined_search_source(
        &self,
        name: &str,
        query: (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_search_source(name, query, from_lookup)
    }
}

/// The environment of the pipeline of a `$lookup` or `$unionWith` that reads collection `coll`
/// (011 T054, FR-004): a `$match` with `$text` or a search stage first in it reads that
/// collection's text or search indexes, as `refnode` does; everything else is the environment's
/// it runs in.
pub(super) struct Joined<'a> {
    pub(super) inner: &'a dyn Env,
    pub(super) coll: &'a str,
    /// Whether it is the pipeline of a `$lookup`.
    pub(super) from_lookup: bool,
}

impl Env for Joined<'_> {
    fn ext(&self) -> &dyn Extensions {
        self.inner.ext()
    }

    fn vars(&self) -> Option<&Document> {
        self.inner.vars()
    }

    fn collection(&self, name: &str) -> Result<Vec<Document>, MqlError> {
        self.inner.collection(name)
    }

    fn collection_matching(
        &self,
        name: &str,
        filter: &Document,
    ) -> Result<Vec<Document>, MqlError> {
        self.inner.collection_matching(name, filter)
    }

    fn random(&self) -> f64 {
        self.inner.random()
    }

    fn system_stage(&self, name: &str, spec: &Document) -> Result<Vec<Document>, MqlError> {
        self.inner.system_stage(name, spec)
    }

    fn search_source(
        &self,
        stage: &str,
        spec: &Document,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_search_source(self.coll, (stage, spec), self.from_lookup)
    }

    fn joined_search_source(
        &self,
        name: &str,
        query: (&str, &Document),
        from_lookup: bool,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_search_source(name, query, from_lookup)
    }

    fn text_source(&self, filter: &Document) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_text_source(self.coll, filter, self.vars())
    }

    fn joined_text_source(
        &self,
        name: &str,
        filter: &Document,
        vars: Option<&Document>,
    ) -> Result<Vec<super::search::Scored>, MqlError> {
        self.inner.joined_text_source(name, filter, vars)
    }

    fn check_search(&self, stage: &str, spec: &Document) -> Result<(), MqlError> {
        self.inner.check_search(stage, spec)
    }

    fn collation(&self) -> crate::collation::Collation {
        self.inner.collation()
    }

    fn geo_indexes(&self) -> Result<(String, Vec<super::geo_near::GeoIndex>), MqlError> {
        self.inner.geo_indexes()
    }

    fn allow_disk_use(&self) -> bool {
        self.inner.allow_disk_use()
    }

    fn stage_memory_limit(&self) -> usize {
        self.inner.stage_memory_limit()
    }

    fn spill(&self) -> Option<&dyn super::spill::SpillStore> {
        self.inner.spill()
    }

    fn operation_memory_limit(&self) -> usize {
        self.inner.operation_memory_limit()
    }

    fn js(&self) -> Option<std::rc::Rc<dyn crate::expr::js::JsEngine>> {
        self.inner.js()
    }

    fn scope(&self, doc: &Document) -> Scope {
        self.inner.scope(doc)
    }
}
