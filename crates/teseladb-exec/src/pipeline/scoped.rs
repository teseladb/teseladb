//! The filters of a pipeline (`$match`, `$lookup`, `restrictSearchWithMatch`) in the pipeline's
//! environment: `$expr` sees its variables and strings compare in its collation (010 FR-014).

use teseladb_mql::{Extensions, Filter, GeoQuery, MatchContext, MqlError, TextQuery};
use teseladb_bson::{Document, RawDoc, RawValue, Value};

use super::Env;
use crate::expr::{Expr, truthy};

/// The extensions of `env` with expressions evaluated in its scopes.
struct ScopedExtensions<'a> {
    env: &'a dyn Env,
}

impl Extensions for ScopedExtensions<'_> {
    fn expr(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        _: Option<&Document>,
    ) -> Result<bool, MqlError> {
        let d = Document::from(doc);
        Ok(truthy(&Expr::parse(e)?.eval(&d, &self.env.scope(&d))?))
    }

    fn where_js(&self, code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        self.env.ext().where_js(code, doc)
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool, MqlError> {
        self.env.ext().text(query, doc)
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool, MqlError> {
        self.env.ext().geo(query, value)
    }

    fn random(&self) -> f64 {
        self.env.ext().random()
    }

    fn meta(&self, name: &str, doc: RawDoc<'_>) -> Result<Option<Value>, MqlError> {
        self.env.ext().meta(name, doc)
    }

    fn eval(
        &self,
        e: RawValue<'_>,
        doc: RawDoc<'_>,
        _: Option<&Document>,
    ) -> Result<Option<Value>, MqlError> {
        let d = Document::from(doc);
        Expr::parse(e)?.eval(&d, &self.env.scope(&d))
    }

    fn update_pipeline(
        &self,
        pipeline: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Document, MqlError> {
        self.env.ext().update_pipeline(pipeline, doc, vars)
    }
}

/// Whether `filter` matches `doc` in the environment `env`.
pub(super) fn matches(filter: &Filter, doc: &Document, env: &dyn Env) -> Result<bool, MqlError> {
    crate::raw::with_raw_doc(doc, |raw| matches_raw(filter, raw, env))
}

/// The documents of `docs` (at most 64) that match `filter` in `env`, as the bits of a mask
/// (010 FR-028, FR-031): the register machine runs them as one batch. A document that keeps its
/// stored bytes is read where it is; the others are written once for the batch. When one of them
/// cannot be written as BSON, each is matched in turn so that the error is the first one in
/// order.
pub(super) fn matches_batch(
    filter: &Filter,
    docs: &[Document],
    env: &dyn Env,
) -> Result<u64, MqlError> {
    let mut written: Vec<Option<Vec<u8>>> = Vec::with_capacity(docs.len());
    for d in docs {
        match d.canonical_bytes() {
            Some(_) => written.push(None),
            None => match crate::raw::bytes_of(d) {
                Ok(b) => written.push(Some(b)),
                Err(_) => return one_by_one(filter, docs, env),
            },
        }
    }
    let mut raws = Vec::with_capacity(docs.len());
    for (d, w) in docs.iter().zip(&written) {
        let raw = match (w, d.canonical_bytes()) {
            (Some(b), _) => RawDoc::from_bytes(b).ok(),
            (None, Some(b)) => crate::raw::stored_view(b),
            (None, None) => None,
        };
        match raw {
            Some(r) => raws.push(r),
            None => return one_by_one(filter, docs, env),
        }
    }
    let collation = env.collation();
    let ext = ScopedExtensions { env };
    let ctx = MatchContext { collation: collation.get(), ext: &ext, vars: env.vars() };
    filter.matches_batch(&raws, &ctx)
}

/// [`matches_batch`] one document at a time.
fn one_by_one(filter: &Filter, docs: &[Document], env: &dyn Env) -> Result<u64, MqlError> {
    let mut mask = 0u64;
    for (i, d) in docs.iter().enumerate() {
        if matches(filter, d, env)? {
            mask |= 1 << i;
        }
    }
    Ok(mask)
}

/// [`matches`] over a document already in BSON.
pub(super) fn matches_raw(
    filter: &Filter,
    raw: RawDoc<'_>,
    env: &dyn Env,
) -> Result<bool, MqlError> {
    let collation = env.collation();
    let ext = ScopedExtensions { env };
    let ctx = MatchContext { collation: collation.get(), ext: &ext, vars: env.vars() };
    filter.matches(raw, &ctx)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_mql::{Extensions, Filter, FilterUse, GeoQuery, MqlError, TextQuery};
    use teseladb_bson::{Document, RawDoc, RawValue, Value};

    use super::{ScopedExtensions, matches};
    use crate::pipeline::Env;

    /// Extensions that answer `true` to every engine they are asked for.
    struct Yes;

    impl Extensions for Yes {
        fn expr(
            &self,
            _: RawValue<'_>,
            _: RawDoc<'_>,
            _: Option<&Document>,
        ) -> Result<bool, MqlError> {
            Ok(false)
        }
        fn where_js(&self, _: RawValue<'_>, _: RawDoc<'_>) -> Result<bool, MqlError> {
            Ok(true)
        }
        fn text(&self, _: &TextQuery, _: RawDoc<'_>) -> Result<bool, MqlError> {
            Ok(true)
        }
        fn geo(&self, _: &GeoQuery, _: RawValue<'_>) -> Result<bool, MqlError> {
            Ok(true)
        }
        fn random(&self) -> f64 {
            0.25
        }
        fn meta(&self, name: &str, _: RawDoc<'_>) -> Result<Option<Value>, MqlError> {
            Ok(Some(Value::String(name.to_owned())))
        }
        fn eval(
            &self,
            _: RawValue<'_>,
            _: RawDoc<'_>,
            _: Option<&Document>,
        ) -> Result<Option<Value>, MqlError> {
            Ok(None)
        }
        fn update_pipeline(
            &self,
            _: RawValue<'_>,
            doc: RawDoc<'_>,
            _: Option<&Document>,
        ) -> Result<Document, MqlError> {
            Ok(Document::from(doc).with("updated", true))
        }
    }

    /// An environment over [`Yes`] with variable `v`.
    struct Node {
        vars: Document,
    }

    impl Env for Node {
        fn ext(&self) -> &dyn Extensions {
            &Yes
        }
        fn vars(&self) -> Option<&Document> {
            Some(&self.vars)
        }
        fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
            Ok(Vec::new())
        }
        fn random(&self) -> f64 {
            0.5
        }
    }

    fn filter(d: &Document) -> Filter {
        let bytes = d.to_bytes().expect("small");
        Filter::parse(RawDoc::from_bytes(&bytes).expect("a document"), FilterUse::Query)
            .expect("a filter")
    }

    #[test]
    fn engines_come_from_the_environment_and_expressions_from_its_scope() {
        let node = Node { vars: Document::new().with("v", 3) };
        let doc = Document::new().with("a", 3).with("loc", vec![Value::Int32(0), Value::Int32(0)]);
        let text = Document::new().with("$text", Document::new().with("$search", "x"));
        assert!(matches(&filter(&text), &doc, &node).expect("no error"));
        let within = Document::new().with(
            "$geoWithin",
            Document::new().with("$center", vec![Value::Array(vec![0.into(), 0.into()]), 1.into()]),
        );
        assert!(matches(&filter(&Document::new().with("loc", within)), &doc, &node).expect("ok"));
        let js = Document::new().with("$where", "true");
        assert!(matches(&filter(&js), &doc, &node).expect("no error"));
        let rate = Document::new().with("$sampleRate", 0.5);
        assert!(matches(&filter(&rate), &doc, &node).expect("no error"));
        // `$expr` sees the environment's variables, not the extensions' answer.
        let expr = Document::new()
            .with("$expr", Document::new().with("$eq", vec![Value::from("$a"), "$$v".into()]));
        assert!(matches(&filter(&expr), &doc, &node).expect("no error"));

        let ext = ScopedExtensions { env: &node };
        let bytes = doc.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&bytes).expect("a document");
        assert_eq!(ext.meta("textScore", raw).expect("ok"), Some(Value::from("textScore")));
        let holder = Document::new().with("e", "$$v").with("p", Vec::<Value>::new());
        let hb = holder.to_bytes().expect("small");
        let h = RawDoc::from_bytes(&hb).expect("a document");
        let e = h.get("e").expect("e");
        assert_eq!(ext.eval(e, raw, None).expect("ok"), Some(Value::Int32(3)));
        let p = h.get("p").expect("p");
        let updated = ext.update_pipeline(p, raw, None).expect("ok");
        assert_eq!(updated.get("updated"), Some(&Value::Boolean(true)));
    }
}
