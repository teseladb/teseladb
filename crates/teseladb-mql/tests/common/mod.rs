//! Helpers shared by the tests: documents written in Extended JSON and test extensions.

use std::cell::Cell;

use teseladb_bson::{Document, RawDoc, RawValue, Value, extjson};
use teseladb_mql::{Extensions, GeoQuery, MqlError, Result, TextQuery, codes};

/// A document from Extended JSON.
pub fn doc(json: &str) -> Document {
    extjson::parse(json).unwrap_or_else(|e| panic!("bad test JSON {json}: {e}"))
}

/// The BSON of a document from Extended JSON.
pub fn bytes(json: &str) -> Vec<u8> {
    doc(json).to_bytes().unwrap()
}

/// A view over bytes.
pub fn raw(bytes: &[u8]) -> RawDoc<'_> {
    RawDoc::from_bytes(bytes).unwrap()
}

/// A document in canonical Extended JSON, for comparisons.
pub fn json(d: &Document) -> String {
    extjson::to_canonical(raw(&d.to_bytes().unwrap()))
}

/// The canonical Extended JSON of what `json` reads as.
pub fn canon(text: &str) -> String {
    json(&doc(text))
}

/// Extensions with fixed answers, counting their calls.
#[derive(Default)]
pub struct TestExt {
    /// Calls to the integration points.
    pub calls: Cell<u32>,
    /// What `random` returns.
    pub random: Cell<f64>,
}

impl Extensions for TestExt {
    fn expr(&self, expr: RawValue<'_>, _: RawDoc<'_>, vars: Option<&Document>) -> Result<bool> {
        self.calls.set(self.calls.get() + 1);
        // `"$$name"` reads a variable of the command's `let`.
        if let RawValue::String(s) = expr
            && let Some(name) = s.strip_prefix("$$")
        {
            return Ok(vars.and_then(|v| v.get(name)) == Some(&Value::Boolean(true)));
        }
        Ok(expr == RawValue::Boolean(true))
    }

    fn where_js(&self, code: RawValue<'_>, _: RawDoc<'_>) -> Result<bool> {
        self.calls.set(self.calls.get() + 1);
        Ok(matches!(code, RawValue::String("true") | RawValue::JavaScript("true")))
    }

    fn text(&self, query: &TextQuery, doc: RawDoc<'_>) -> Result<bool> {
        self.calls.set(self.calls.get() + 1);
        Ok(matches!(doc.get("t"), Some(RawValue::String(s)) if s.contains(query.search.as_str())))
    }

    fn geo(&self, query: &GeoQuery, value: RawValue<'_>) -> Result<bool> {
        self.calls.set(self.calls.get() + 1);
        if query.operator == "$geoIntersects" {
            return Err(MqlError::new(codes::BAD_VALUE, "test"));
        }
        Ok(matches!(value, RawValue::Document(_)))
    }

    fn random(&self) -> f64 {
        self.random.get()
    }

    fn meta(&self, name: &str, _: RawDoc<'_>) -> Result<Option<Value>> {
        Ok((name == "textScore").then_some(Value::Double(1.5)))
    }

    fn update_pipeline(
        &self,
        _: RawValue<'_>,
        doc: RawDoc<'_>,
        _: Option<&Document>,
    ) -> Result<Document> {
        let mut out = Document::from(doc);
        out.insert("piped", true);
        Ok(out)
    }

    fn eval(
        &self,
        expr: RawValue<'_>,
        doc: RawDoc<'_>,
        vars: Option<&Document>,
    ) -> Result<Option<Value>> {
        match expr {
            RawValue::String(s) if s.starts_with("$$") => {
                Ok(Some(vars.and_then(|v| v.get(&s[2..])).cloned().unwrap_or(Value::Null)))
            }
            RawValue::String(path) if path.starts_with('$') => {
                Ok(Some(doc.get(&path[1..]).map_or(Value::Null, Value::from)))
            }
            RawValue::Document(d) if d.get("$fail").is_some() => {
                Err(MqlError::new(codes::BAD_VALUE, "eval failed"))
            }
            v => Ok(Some(Value::from(v))),
        }
    }
}
