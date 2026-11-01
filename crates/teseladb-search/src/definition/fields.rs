//! The mappings of a `search` index: which fields are indexed and how (011 FR-006), with the
//! defaults and errors measured on `refsearch` 1.70.6.

use teseladb_bson::{Document, Value};

use super::{
    DefinitionError, StoredSource, bool_of, int_of, join, one_of, string_of, unrecognized,
};

/// The field types a mapping may name, in `refsearch`'s order.
pub const FIELD_TYPES: [&str; 14] = [
    "autoEmbedVector",
    "autocomplete",
    "boolean",
    "date",
    "document",
    "embeddedDocuments",
    "geo",
    "knnVector",
    "number",
    "objectId",
    "string",
    "token",
    "uuid",
    "vector",
];

const INDEX_OPTIONS: [&str; 4] = ["docs", "freqs", "offsets", "positions"];
const NORMS: [&str; 2] = ["include", "omit"];
const NORMALIZERS: [&str; 2] = ["lowercase", "none"];
const REPRESENTATIONS: [&str; 2] = ["double", "int64"];
const TOKENIZATIONS: [&str; 3] = ["edgeGram", "nGram", "rightEdgeGram"];
/// The similarities of vector fields.
pub const SIMILARITIES: [&str; 3] = ["cosine", "dotProduct", "euclidean"];
/// The bounds of a vector's number of dimensions.
pub const MAX_DIMENSIONS: i64 = 8192;

/// Which fields of the documents are indexed without a mapping of their own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dynamic {
    /// None (`false`, the default).
    Off,
    /// Every field, with the default type of its value (`true`).
    On,
    /// Every field whose type is in the named type set.
    TypeSet(String),
}

/// Where a mapping sits: its options depend on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    /// `mappings`.
    Root,
    /// A `document` field.
    Document,
    /// An `embeddedDocuments` field, which may keep its own `storedSource`.
    Embedded,
}

/// A mapping: dynamic indexing and the fields mapped by name, in the order given.
#[derive(Clone, Debug, PartialEq)]
pub struct Mapping {
    /// Dynamic indexing.
    pub dynamic: Dynamic,
    /// Fields by name, each with one or more types.
    pub fields: Vec<(String, Vec<FieldMapping>)>,
    /// What an `embeddedDocuments` field keeps of each element.
    pub stored_source: StoredSource,
}

impl Mapping {
    pub(crate) fn parse(
        m: &Document,
        path: &str,
        owner: Owner,
    ) -> Result<Mapping, DefinitionError> {
        let dynamic = match m.get("dynamic") {
            None | Some(Value::Boolean(false)) => Dynamic::Off,
            Some(Value::Boolean(true)) => Dynamic::On,
            Some(Value::Document(d)) => {
                let at = join(path, "dynamic");
                let set = string_of(d, &at, "typeSet")?
                    .ok_or_else(|| DefinitionError::shape(&join(&at, "typeSet"), "is required"))?;
                Dynamic::TypeSet(set.to_owned())
            }
            Some(_) => {
                return Err(DefinitionError::shape(
                    &join(path, "dynamic"),
                    "must be a boolean or document",
                ));
            }
        };
        let fields_at = join(path, "fields");
        let fields = match m.get("fields") {
            None => Vec::new(),
            Some(Value::Document(f)) => f
                .iter()
                .map(|(name, v)| {
                    Ok((
                        name.to_owned(),
                        FieldMapping::parse_list(Some(v), &join(&fields_at, name))?,
                    ))
                })
                .collect::<Result<_, DefinitionError>>()?,
            Some(_) => return Err(DefinitionError::shape(&fields_at, "must be a document")),
        };
        let stored_source = match owner {
            Owner::Embedded => {
                StoredSource::parse(m.get("storedSource"), &join(path, "storedSource"))?
            }
            Owner::Root | Owner::Document => StoredSource::None,
        };
        Ok(Mapping { dynamic, fields, stored_source })
    }

    /// The analyzer names the mapping uses, added to `out`.
    pub(crate) fn analyzers<'a>(&'a self, out: &mut Vec<&'a str>) {
        for (_, types) in &self.fields {
            for t in types {
                t.analyzers(out);
            }
        }
    }

    pub(crate) fn normalized(&self) -> Document {
        let dynamic = match &self.dynamic {
            Dynamic::Off => Value::Boolean(false),
            Dynamic::On => Value::Boolean(true),
            Dynamic::TypeSet(s) => Document::new().with("typeSet", s.as_str()).into(),
        };
        let mut fields = Document::new();
        for (name, types) in &self.fields {
            let v = match types.as_slice() {
                [one] => Value::Document(one.normalized()),
                many => {
                    Value::Array(many.iter().map(|t| Value::Document(t.normalized())).collect())
                }
            };
            fields.push(name.as_str(), v);
        }
        let mut d = Document::new().with("dynamic", dynamic).with("fields", fields);
        if let Some(s) = self.stored_source.normalized() {
            d.push("storedSource", s);
        }
        d
    }
}

/// A `string` field.
#[derive(Clone, Debug, PartialEq)]
pub struct StringField {
    /// Index analyzer.
    pub analyzer: Option<String>,
    /// Query analyzer.
    pub search_analyzer: Option<String>,
    /// Strings longer than this are not indexed.
    pub ignore_above: Option<i64>,
    /// What each posting keeps (`offsets` by default).
    pub index_options: String,
    /// Whether the text is stored for highlighting (`true` by default).
    pub store: bool,
    /// Whether the field's length counts in the score (`include` by default).
    pub norms: String,
    /// Alternate analyzers of the same field, by name.
    pub multi: Vec<(String, StringField)>,
}

impl StringField {
    fn parse(d: &Document, path: &str, in_multi: bool) -> Result<StringField, DefinitionError> {
        let mut allowed = vec![
            "type",
            "analyzer",
            "searchAnalyzer",
            "ignoreAbove",
            "indexOptions",
            "store",
            "norms",
        ];
        if !in_multi {
            allowed.push("multi");
        }
        only(d, path, &allowed)?;
        let multi = match d.get("multi") {
            None => Vec::new(),
            Some(Value::Document(m)) => m
                .iter()
                .map(|(name, v)| {
                    let at = join(&join(path, "multi"), name);
                    match v {
                        Value::Document(sub) => {
                            Ok((name.to_owned(), StringField::parse(sub, &at, true)?))
                        }
                        _ => Err(DefinitionError::shape(&at, "must be a document")),
                    }
                })
                .collect::<Result<_, DefinitionError>>()?,
            Some(_) => {
                return Err(DefinitionError::shape(&join(path, "multi"), "must be a document"));
            }
        };
        Ok(StringField {
            analyzer: string_of(d, path, "analyzer")?.map(str::to_owned),
            search_analyzer: string_of(d, path, "searchAnalyzer")?.map(str::to_owned),
            ignore_above: int_of(d, path, "ignoreAbove")?,
            index_options: one_of(d, path, "indexOptions", &INDEX_OPTIONS)?
                .unwrap_or("offsets")
                .to_owned(),
            store: bool_of(d, path, "store")?.unwrap_or(true),
            norms: one_of(d, path, "norms", &NORMS)?.unwrap_or("include").to_owned(),
            multi,
        })
    }

    fn normalized(&self) -> Document {
        let mut d = Document::new().with("type", "string");
        if let Some(a) = &self.analyzer {
            d.push("analyzer", a.as_str());
        }
        if let Some(a) = &self.search_analyzer {
            d.push("searchAnalyzer", a.as_str());
        }
        if let Some(n) = self.ignore_above {
            d.push("ignoreAbove", int32(n));
        }
        d.push("indexOptions", self.index_options.as_str());
        d.push("store", self.store);
        d.push("norms", self.norms.as_str());
        if !self.multi.is_empty() {
            let mut m = Document::new();
            for (name, f) in &self.multi {
                m.push(name.as_str(), f.normalized());
            }
            d.push("multi", m);
        }
        d
    }
}

/// How one field is indexed.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldMapping {
    /// Analyzed text.
    String(StringField),
    /// A whole string for `equals`, `in`, `range`, sort and facets.
    Token {
        /// `lowercase` or `none`, as given.
        normalizer: Option<String>,
    },
    /// Numbers.
    Number {
        /// `double` (default) or `int64`.
        representation: String,
        /// Whether integers are indexed.
        index_integers: bool,
        /// Whether doubles are indexed.
        index_doubles: bool,
    },
    /// Dates.
    Date,
    /// Booleans.
    Boolean,
    /// `ObjectId`s.
    ObjectId,
    /// UUIDs.
    Uuid,
    /// Grams of the text for `autocomplete`.
    Autocomplete {
        /// Shortest gram.
        min_grams: i64,
        /// Longest gram.
        max_grams: i64,
        /// Whether diacritics are folded.
        fold_diacritics: bool,
        /// `edgeGram` (default), `rightEdgeGram` or `nGram`.
        tokenization: String,
        /// Analyzer of the text before the grams.
        analyzer: Option<String>,
    },
    /// Points and shapes.
    Geo {
        /// Whether shapes are indexed, as given.
        index_shapes: Option<bool>,
    },
    /// An embedded document with its own mapping.
    Document(Mapping),
    /// An array of documents searched element by element.
    EmbeddedDocuments(Mapping),
    /// A vector of the deprecated `knnBeta` operator.
    KnnVector {
        /// Number of dimensions.
        dimensions: i64,
        /// Similarity.
        similarity: String,
    },
    /// A type `refsearch` accepts and this engine keeps as given (`vector`, `autoEmbedVector`).
    Other(Document),
}

/// Refuses the options of `d` outside `allowed`.
fn only(d: &Document, path: &str, allowed: &[&str]) -> Result<(), DefinitionError> {
    match d.iter().find(|(k, _)| !allowed.contains(k)) {
        Some((k, _)) => Err(unrecognized(path, k)),
        None => Ok(()),
    }
}

impl FieldMapping {
    /// The types of a field: one document or an array of them.
    pub(crate) fn parse_list(
        v: Option<&Value>,
        path: &str,
    ) -> Result<Vec<FieldMapping>, DefinitionError> {
        match v {
            Some(Value::Document(d)) => Ok(vec![FieldMapping::parse(d, path)?]),
            Some(Value::Array(items)) => items
                .iter()
                .enumerate()
                .map(|(i, item)| {
                    let at = format!("{path}[{i}]");
                    match item {
                        Value::Document(d) => FieldMapping::parse(d, &at),
                        _ => Err(DefinitionError::shape(&at, "must be a document")),
                    }
                })
                .collect(),
            None => Err(DefinitionError::shape(path, "is required")),
            Some(_) => Err(DefinitionError::shape(path, "must be a document or array")),
        }
    }

    fn parse(d: &Document, path: &str) -> Result<FieldMapping, DefinitionError> {
        let kind = string_of(d, path, "type")?
            .ok_or_else(|| DefinitionError::shape(&join(path, "type"), "is required"))?;
        if !FIELD_TYPES.contains(&kind) {
            let what = format!("must be one of [{}]", FIELD_TYPES.join(", "));
            return Err(DefinitionError::shape(&join(path, "type"), what));
        }
        Ok(match kind {
            "string" => FieldMapping::String(StringField::parse(d, path, false)?),
            "token" => {
                only(d, path, &["type", "normalizer"])?;
                FieldMapping::Token {
                    normalizer: one_of(d, path, "normalizer", &NORMALIZERS)?.map(str::to_owned),
                }
            }
            "number" => {
                only(d, path, &["type", "representation", "indexIntegers", "indexDoubles"])?;
                FieldMapping::Number {
                    representation: one_of(d, path, "representation", &REPRESENTATIONS)?
                        .unwrap_or("double")
                        .to_owned(),
                    index_integers: bool_of(d, path, "indexIntegers")?.unwrap_or(true),
                    index_doubles: bool_of(d, path, "indexDoubles")?.unwrap_or(true),
                }
            }
            "date" | "boolean" | "objectId" | "uuid" => {
                only(d, path, &["type"])?;
                match kind {
                    "date" => FieldMapping::Date,
                    "boolean" => FieldMapping::Boolean,
                    "objectId" => FieldMapping::ObjectId,
                    _ => FieldMapping::Uuid,
                }
            }
            "autocomplete" => autocomplete(d, path)?,
            "geo" => {
                only(d, path, &["type", "indexShapes"])?;
                FieldMapping::Geo { index_shapes: bool_of(d, path, "indexShapes")? }
            }
            "document" => {
                only(d, path, &["type", "dynamic", "fields"])?;
                FieldMapping::Document(Mapping::parse(d, path, Owner::Document)?)
            }
            "embeddedDocuments" => {
                only(d, path, &["type", "dynamic", "fields", "storedSource"])?;
                FieldMapping::EmbeddedDocuments(Mapping::parse(d, path, Owner::Embedded)?)
            }
            "knnVector" => knn_vector(d, path)?,
            _ => FieldMapping::Other(d.clone()),
        })
    }

    fn analyzers<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            FieldMapping::String(s) => string_analyzers(s, out),
            FieldMapping::Autocomplete { analyzer, .. } => out.extend(analyzer.as_deref()),
            FieldMapping::Document(m) | FieldMapping::EmbeddedDocuments(m) => m.analyzers(out),
            FieldMapping::Token { .. }
            | FieldMapping::Number { .. }
            | FieldMapping::Date
            | FieldMapping::Boolean
            | FieldMapping::ObjectId
            | FieldMapping::Uuid
            | FieldMapping::Geo { .. }
            | FieldMapping::KnnVector { .. }
            | FieldMapping::Other(_) => {}
        }
    }

    /// The field mapping as `refsearch` shows it.
    pub fn normalized(&self) -> Document {
        match self {
            FieldMapping::String(s) => s.normalized(),
            FieldMapping::Token { normalizer } => {
                let mut d = Document::new().with("type", "token");
                if let Some(n) = normalizer {
                    d.push("normalizer", n.as_str());
                }
                d
            }
            FieldMapping::Number { representation, index_integers, index_doubles } => {
                Document::new()
                    .with("type", "number")
                    .with("representation", representation.as_str())
                    .with("indexDoubles", *index_doubles)
                    .with("indexIntegers", *index_integers)
            }
            FieldMapping::Date => Document::new().with("type", "date"),
            FieldMapping::Boolean => Document::new().with("type", "boolean"),
            FieldMapping::ObjectId => Document::new().with("type", "objectId"),
            FieldMapping::Uuid => Document::new().with("type", "uuid"),
            FieldMapping::Autocomplete {
                min_grams,
                max_grams,
                fold_diacritics,
                tokenization,
                analyzer,
            } => {
                let mut d = Document::new()
                    .with("type", "autocomplete")
                    .with("minGrams", int32(*min_grams))
                    .with("maxGrams", int32(*max_grams))
                    .with("foldDiacritics", *fold_diacritics)
                    .with("tokenization", tokenization.as_str());
                if let Some(a) = analyzer {
                    d.push("analyzer", a.as_str());
                }
                d
            }
            FieldMapping::Geo { index_shapes } => {
                let mut d = Document::new().with("type", "geo");
                if let Some(s) = index_shapes {
                    d.push("indexShapes", *s);
                }
                d
            }
            FieldMapping::Document(m) => typed("document", m.normalized()),
            FieldMapping::EmbeddedDocuments(m) => typed("embeddedDocuments", m.normalized()),
            FieldMapping::KnnVector { dimensions, similarity } => Document::new()
                .with("type", "knnVector")
                .with("dimensions", int32(*dimensions))
                .with("similarity", similarity.as_str()),
            FieldMapping::Other(d) => d.clone(),
        }
    }
}

fn string_analyzers<'a>(s: &'a StringField, out: &mut Vec<&'a str>) {
    out.extend(s.analyzer.as_deref());
    out.extend(s.search_analyzer.as_deref());
    for (_, m) in &s.multi {
        string_analyzers(m, out);
    }
}

/// A number of the definition as `refsearch` shows it: a 32-bit integer (measured).
pub(crate) fn int32(n: i64) -> Value {
    Value::Int32(i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX }))
}

/// `type` followed by the fields of `rest`.
fn typed(kind: &str, rest: Document) -> Document {
    let mut d = Document::new().with("type", kind);
    for (k, v) in rest.iter() {
        d.push(k, v.clone());
    }
    d
}

fn autocomplete(d: &Document, path: &str) -> Result<FieldMapping, DefinitionError> {
    only(d, path, &["type", "minGrams", "maxGrams", "foldDiacritics", "tokenization", "analyzer"])?;
    let positive = |field: &str, default: i64| -> Result<i64, DefinitionError> {
        match int_of(d, path, field)? {
            None => Ok(default),
            Some(n) if n > 0 => Ok(n),
            Some(_) => Err(DefinitionError::shape(&join(path, field), "must be positive")),
        }
    };
    Ok(FieldMapping::Autocomplete {
        min_grams: positive("minGrams", 2)?,
        max_grams: positive("maxGrams", 15)?,
        fold_diacritics: bool_of(d, path, "foldDiacritics")?.unwrap_or(true),
        tokenization: one_of(d, path, "tokenization", &TOKENIZATIONS)?
            .unwrap_or("edgeGram")
            .to_owned(),
        analyzer: string_of(d, path, "analyzer")?.map(str::to_owned),
    })
}

fn knn_vector(d: &Document, path: &str) -> Result<FieldMapping, DefinitionError> {
    only(d, path, &["type", "dimensions", "similarity"])?;
    let dimensions = int_of(d, path, "dimensions")?
        .ok_or_else(|| DefinitionError::shape(&join(path, "dimensions"), "is required"))?;
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        let what = format!("must be within bounds [1..{MAX_DIMENSIONS}]");
        return Err(DefinitionError::shape(&join(path, "dimensions"), what));
    }
    let similarity = one_of(d, path, "similarity", &SIMILARITIES)?
        .ok_or_else(|| DefinitionError::shape(&join(path, "similarity"), "is required"))?;
    Ok(FieldMapping::KnnVector { dimensions, similarity: similarity.to_owned() })
}
