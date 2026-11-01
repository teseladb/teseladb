//! Search index definitions (011 FR-006, T039): the definition of a `search` index (mappings,
//! analyzers, `storedSource`, synonyms, type sets) and of a `vectorSearch` index (its `fields`),
//! checked and filled with their defaults as `refsearch` 1.70.6 does (measured on a 9.0.4 replica
//! set, 01-11-2026). [`Definition::normalized`] gives the definition as `refsearch` shows it in
//! `statusDetail.mainIndex.definition`.

mod fields;
mod vector;

use teseladb_bson::{Document, Value};

pub use fields::{Dynamic, FieldMapping, Mapping, StringField};
pub use vector::{VectorDefinition, VectorField};

use crate::analysis::analyzer::Analyzer;

/// A problem of a definition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefinitionError {
    /// The definition's shape: the path of the field inside the definition and what is wrong;
    /// `refsearch` words it `"<path>" <what>`.
    Shape {
        /// Path inside the definition (`mappings.fields.t.type`); empty for the root.
        path: String,
        /// What is wrong (`is required`, `must be a boolean`…).
        what: String,
    },
    /// Names that are no analyzer, built-in or defined (`references non-existent analyzers`).
    MissingAnalyzers(Vec<String>),
}

impl DefinitionError {
    pub(crate) fn shape(path: &str, what: impl Into<String>) -> DefinitionError {
        DefinitionError::Shape { path: path.to_owned(), what: what.into() }
    }

    /// The message `refsearch` gives, with the shape errors' paths behind `prefix`
    /// (`userCommand.indexes[0].` when creating, nothing when updating).
    pub fn message(&self, prefix: &str) -> String {
        match self {
            DefinitionError::Shape { path, what } => {
                let at = format!("{prefix}{path}");
                format!("\"{}\" {what}", at.trim_end_matches('.'))
            }
            DefinitionError::MissingAnalyzers(names) => {
                format!("references non-existent analyzers: {}", names.join(", "))
            }
        }
    }
}

/// The path of `field` under `parent` (`parent.field`, or `field` at the root).
pub(crate) fn join(parent: &str, field: &str) -> String {
    if parent.is_empty() { field.to_owned() } else { format!("{parent}.{field}") }
}

/// `"<path>" unrecognized field "<field>"`.
pub(crate) fn unrecognized(path: &str, field: &str) -> DefinitionError {
    DefinitionError::shape(path, format!("unrecognized field \"{field}\""))
}

/// A string option of `doc` at `path`.
pub(crate) fn string_of<'a>(
    doc: &'a Document,
    path: &str,
    field: &str,
) -> Result<Option<&'a str>, DefinitionError> {
    match doc.get(field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(DefinitionError::shape(&join(path, field), "must be a string")),
    }
}

/// A boolean option of `doc` at `path`.
pub(crate) fn bool_of(
    doc: &Document,
    path: &str,
    field: &str,
) -> Result<Option<bool>, DefinitionError> {
    match doc.get(field) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => Err(DefinitionError::shape(&join(path, field), "must be a boolean")),
    }
}

/// A whole number option of `doc` at `path`.
pub(crate) fn int_of(
    doc: &Document,
    path: &str,
    field: &str,
) -> Result<Option<i64>, DefinitionError> {
    match doc.get(field) {
        None => Ok(None),
        Some(Value::Int32(n)) => Ok(Some(i64::from(*n))),
        Some(Value::Int64(n)) => Ok(Some(*n)),
        Some(Value::Double(f)) if f.fract() == 0.0 && f.is_finite() => Ok(Some(*f as i64)),
        Some(_) => Err(DefinitionError::shape(&join(path, field), "must be an integer")),
    }
}

/// One of `allowed` for option `field` of `doc` at `path`.
pub(crate) fn one_of<'a>(
    doc: &'a Document,
    path: &str,
    field: &str,
    allowed: &[&str],
) -> Result<Option<&'a str>, DefinitionError> {
    let Some(v) = string_of(doc, path, field)? else { return Ok(None) };
    if allowed.contains(&v) {
        return Ok(Some(v));
    }
    let what = format!("must be one of [{}]", allowed.join(", "));
    Err(DefinitionError::shape(&join(path, field), what))
}

/// What the index keeps of each document besides its fields (`storedSource`).
#[derive(Clone, Debug, PartialEq)]
pub enum StoredSource {
    /// Nothing (the default, or `false`).
    None,
    /// The whole document (`true`).
    All,
    /// The fields listed (`include`).
    Include(Vec<String>),
    /// All but the fields listed (`exclude`).
    Exclude(Vec<String>),
}

impl StoredSource {
    fn parse(v: Option<&Value>, path: &str) -> Result<StoredSource, DefinitionError> {
        let list = |items: &Value, at: &str| -> Result<Vec<String>, DefinitionError> {
            let Value::Array(items) = items else {
                return Err(DefinitionError::shape(at, "must be an array"));
            };
            items
                .iter()
                .map(|i| match i {
                    Value::String(s) => Ok(s.clone()),
                    _ => Err(DefinitionError::shape(at, "must be an array of strings")),
                })
                .collect()
        };
        match v {
            None | Some(Value::Boolean(false)) => Ok(StoredSource::None),
            Some(Value::Boolean(true)) => Ok(StoredSource::All),
            Some(Value::Document(d)) => match (d.get("include"), d.get("exclude")) {
                (Some(i), None) => Ok(StoredSource::Include(list(i, &join(path, "include"))?)),
                (None, Some(e)) => Ok(StoredSource::Exclude(list(e, &join(path, "exclude"))?)),
                _ => Err(DefinitionError::shape(path, "must have one of [include, exclude]")),
            },
            Some(_) => Err(DefinitionError::shape(path, "must be a boolean or document")),
        }
    }

    fn normalized(&self) -> Option<Value> {
        let names =
            |l: &[String]| Value::Array(l.iter().map(|s| Value::from(s.as_str())).collect());
        match self {
            StoredSource::None => None,
            StoredSource::All => Some(Value::Boolean(true)),
            StoredSource::Include(l) => Some(Document::new().with("include", names(l)).into()),
            StoredSource::Exclude(l) => Some(Document::new().with("exclude", names(l)).into()),
        }
    }
}

/// A synonym mapping: its name, the collection with the synonyms and the analyzer they go through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Synonyms {
    /// Name.
    pub name: String,
    /// Collection of the synonym documents.
    pub collection: String,
    /// Analyzer.
    pub analyzer: String,
}

/// The definition of a `search` index.
#[derive(Clone, Debug, PartialEq)]
pub struct Definition {
    /// Index analyzer of the fields without their own (`reflib.standard` when absent).
    pub analyzer: Option<String>,
    /// Query analyzer of the fields without their own (the index analyzer when absent).
    pub search_analyzer: Option<String>,
    /// The document's mapping.
    pub mappings: Mapping,
    /// What is kept besides the fields.
    pub stored_source: StoredSource,
    /// Custom analyzers, as given, by name.
    pub analyzers: Vec<(String, Document)>,
    /// Synonym mappings.
    pub synonyms: Vec<Synonyms>,
    /// Type sets of `dynamic: {typeSet}`, by name, each with its field types.
    pub type_sets: Vec<(String, Vec<FieldMapping>)>,
}

/// The analyzer used when an index names none.
pub const DEFAULT_ANALYZER: &str = "reflib.standard";

impl Definition {
    /// Reads and checks a `search` definition. Fields `refsearch` does not know at the top are
    /// left out, as it does (measured: `numPartitions` and unknown names are accepted).
    pub fn parse(def: &Document) -> Result<Definition, DefinitionError> {
        let mappings = match def.get("mappings") {
            None => return Err(DefinitionError::shape("mappings", "is required")),
            Some(Value::Document(m)) => Mapping::parse(m, "mappings", fields::Owner::Root)?,
            Some(_) => return Err(DefinitionError::shape("mappings", "must be a document")),
        };
        let analyzers = custom_analyzers(def)?;
        let synonyms = synonyms(def)?;
        let type_sets = type_sets(def)?;
        let d = Definition {
            analyzer: string_of(def, "", "analyzer")?.map(str::to_owned),
            search_analyzer: string_of(def, "", "searchAnalyzer")?.map(str::to_owned),
            mappings,
            stored_source: StoredSource::parse(def.get("storedSource"), "storedSource")?,
            analyzers,
            synonyms,
            type_sets,
        };
        d.check_analyzers()?;
        Ok(d)
    }

    /// Whether `name` is an analyzer of this index: a built-in one or one it defines.
    pub fn has_analyzer(&self, name: &str) -> bool {
        Analyzer::builtin(name).is_some() || self.analyzers.iter().any(|(n, _)| n == name)
    }

    /// Every analyzer name the definition uses must exist (code 125 when one does not).
    fn check_analyzers(&self) -> Result<(), DefinitionError> {
        let mut used: Vec<&str> = Vec::new();
        used.extend(self.analyzer.as_deref());
        used.extend(self.search_analyzer.as_deref());
        self.mappings.analyzers(&mut used);
        used.extend(self.synonyms.iter().map(|s| s.analyzer.as_str()));
        let mut missing: Vec<String> = Vec::new();
        for name in used {
            if !self.has_analyzer(name) && !missing.iter().any(|m| m == name) {
                missing.push(name.to_owned());
            }
        }
        if missing.is_empty() { Ok(()) } else { Err(DefinitionError::MissingAnalyzers(missing)) }
    }

    /// The definition as `refsearch` shows it, with its defaults filled in.
    pub fn normalized(&self) -> Document {
        let mut d = Document::new();
        if let Some(a) = &self.analyzer {
            d.push("analyzer", a.as_str());
        }
        if let Some(a) = &self.search_analyzer {
            d.push("searchAnalyzer", a.as_str());
        }
        d.push("mappings", self.mappings.normalized());
        if let Some(s) = self.stored_source.normalized() {
            d.push("storedSource", s);
        }
        if !self.analyzers.is_empty() {
            let all = self.analyzers.iter().map(|(_, a)| Value::Document(a.clone())).collect();
            d.push("analyzers", Value::Array(all));
        }
        if !self.synonyms.is_empty() {
            let all = self
                .synonyms
                .iter()
                .map(|s| {
                    let source = Document::new().with("collection", s.collection.as_str());
                    Value::Document(
                        Document::new()
                            .with("name", s.name.as_str())
                            .with("source", source)
                            .with("analyzer", s.analyzer.as_str()),
                    )
                })
                .collect();
            d.push("synonyms", Value::Array(all));
        }
        if !self.type_sets.is_empty() {
            let all = self
                .type_sets
                .iter()
                .map(|(name, types)| {
                    let types = types.iter().map(|t| Value::Document(t.normalized())).collect();
                    Value::Document(
                        Document::new()
                            .with("name", name.as_str())
                            .with("types", Value::Array(types)),
                    )
                })
                .collect();
            d.push("typeSets", Value::Array(all));
        }
        d
    }
}

/// The documents of array field `field` of the definition, with their paths.
fn documents<'a>(
    def: &'a Document,
    field: &str,
) -> Result<Vec<(String, &'a Document)>, DefinitionError> {
    match def.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let at = format!("{field}[{i}]");
                match v {
                    Value::Document(d) => Ok((at, d)),
                    _ => Err(DefinitionError::shape(&at, "must be a document")),
                }
            })
            .collect(),
        Some(_) => Err(DefinitionError::shape(field, "must be an array")),
    }
}

/// The custom analyzers, each with a name.
fn custom_analyzers(def: &Document) -> Result<Vec<(String, Document)>, DefinitionError> {
    documents(def, "analyzers")?
        .into_iter()
        .map(|(at, a)| {
            let name = string_of(a, &at, "name")?
                .ok_or_else(|| DefinitionError::shape(&join(&at, "name"), "is required"))?;
            Ok((name.to_owned(), a.clone()))
        })
        .collect()
}

/// The synonym mappings.
fn synonyms(def: &Document) -> Result<Vec<Synonyms>, DefinitionError> {
    documents(def, "synonyms")?
        .into_iter()
        .map(|(at, s)| {
            let required = |f: &str| DefinitionError::shape(&join(&at, f), "is required");
            let name = string_of(s, &at, "name")?.ok_or_else(|| required("name"))?;
            let analyzer = string_of(s, &at, "analyzer")?.ok_or_else(|| required("analyzer"))?;
            let source_at = join(&at, "source");
            let collection = match s.get("source") {
                Some(Value::Document(src)) => string_of(src, &source_at, "collection")?
                    .ok_or_else(|| {
                        DefinitionError::shape(&join(&source_at, "collection"), "is required")
                    })?,
                Some(_) => return Err(DefinitionError::shape(&source_at, "must be a document")),
                None => return Err(required("source")),
            };
            Ok(Synonyms {
                name: name.to_owned(),
                collection: collection.to_owned(),
                analyzer: analyzer.to_owned(),
            })
        })
        .collect()
}

/// The type sets.
fn type_sets(def: &Document) -> Result<Vec<(String, Vec<FieldMapping>)>, DefinitionError> {
    documents(def, "typeSets")?
        .into_iter()
        .map(|(at, t)| {
            let name = string_of(t, &at, "name")?
                .ok_or_else(|| DefinitionError::shape(&join(&at, "name"), "is required"))?;
            let types = FieldMapping::parse_list(t.get("types"), &join(&at, "types"))?;
            Ok((name.to_owned(), types))
        })
        .collect()
}

#[cfg(test)]
mod tests;
