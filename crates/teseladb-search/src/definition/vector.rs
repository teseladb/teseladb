//! The definition of a `vectorSearch` index: its `fields`, vectors and filters (011 FR-005),
//! with the errors measured on the reference search server 1.70.6.

use teseladb_bson::{Document, Value};

use super::fields::{MAX_DIMENSIONS, SIMILARITIES, int32};
use super::{
    DefinitionError, embedding_model, int_of, join, no_embedding_service, one_of, string_of,
    unrecognized,
};

/// The field types of a `vectorSearch` index, in the reference search server's order.
pub const VECTOR_FIELD_TYPES: [&str; 5] =
    ["autoEmbed", "embeddedDocuments", "filter", "text", "vector"];
const QUANTIZATIONS: [&str; 3] = ["binary", "none", "scalar"];

/// A field of a `vectorSearch` index.
#[derive(Clone, Debug, PartialEq)]
pub enum VectorField {
    /// Vectors at `path`.
    Vector {
        /// Path.
        path: String,
        /// Number of dimensions.
        dimensions: i64,
        /// `cosine`, `dotProduct` or `euclidean`.
        similarity: String,
        /// Quantization, as given.
        quantization: Option<String>,
    },
    /// A field the pre-filter may use.
    Filter {
        /// Path.
        path: String,
    },
    /// A type the reference search server accepts and this engine keeps as given (`autoEmbed`, `text`,
    /// `embeddedDocuments`).
    Other(Document),
}

/// The model a `text` field embeds with when it names none (measured on the reference search server 1.70.6).
const DEFAULT_EMBEDDING_MODEL: &str = "voyage-3-large";

/// The definition of a `vectorSearch` index.
#[derive(Clone, Debug, PartialEq)]
pub struct VectorDefinition {
    /// Fields, in the order given.
    pub fields: Vec<VectorField>,
}

impl VectorDefinition {
    /// Reads and checks a `vectorSearch` definition.
    pub fn parse(def: &Document) -> Result<VectorDefinition, DefinitionError> {
        let items = match def.get("fields") {
            None => return Err(DefinitionError::shape("fields", "is required")),
            Some(Value::Array(items)) => items,
            Some(_) => return Err(DefinitionError::shape("fields", "must be an array")),
        };
        let fields = items
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let at = format!("fields[{i}]");
                match v {
                    Value::Document(d) => VectorField::parse(d, &at),
                    _ => Err(DefinitionError::shape(&at, "must be a document")),
                }
            })
            .collect::<Result<Vec<VectorField>, _>>()?;
        // A `text` field embeds with its `model`, `voyage-3-large` when it names none.
        let models: Vec<String> = fields
            .iter()
            .filter_map(|f| match f {
                VectorField::Other(d) if d.get("type") == Some(&Value::from("text")) => {
                    Some(match d.get("model") {
                        Some(Value::String(m)) => m.clone(),
                        _ => DEFAULT_EMBEDDING_MODEL.to_owned(),
                    })
                }
                _ => None,
            })
            .collect();
        if !models.is_empty() {
            return Err(no_embedding_service(&models));
        }
        Ok(VectorDefinition { fields })
    }

    /// The definition as the reference search server shows it.
    pub fn normalized(&self) -> Document {
        let all = self.fields.iter().map(|f| Value::Document(f.normalized())).collect();
        Document::new().with("fields", Value::Array(all))
    }
}

impl VectorField {
    fn parse(d: &Document, at: &str) -> Result<VectorField, DefinitionError> {
        let kind = string_of(d, at, "type")?
            .ok_or_else(|| DefinitionError::shape(&join(at, "type"), "is required"))?;
        if !VECTOR_FIELD_TYPES.contains(&kind) {
            let what = format!("must be one of [{}]", VECTOR_FIELD_TYPES.join(", "));
            return Err(DefinitionError::shape(&join(at, "type"), what));
        }
        let path = || -> Result<String, DefinitionError> {
            string_of(d, at, "path")?
                .map(str::to_owned)
                .ok_or_else(|| DefinitionError::shape(&join(at, "path"), "is required"))
        };
        let only = |allowed: &[&str]| match d.iter().find(|(k, _)| !allowed.contains(k)) {
            Some((k, _)) => Err(unrecognized(at, k)),
            None => Ok(()),
        };
        match kind {
            "vector" => {
                only(&["type", "path", "numDimensions", "similarity", "quantization"])?;
                let path = path()?;
                let dimensions = int_of(d, at, "numDimensions")?.ok_or_else(|| {
                    DefinitionError::shape(&join(at, "numDimensions"), "is required")
                })?;
                if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
                    let what = format!("must be within bounds [1..{MAX_DIMENSIONS}]");
                    return Err(DefinitionError::shape(&join(at, "numDimensions"), what));
                }
                let similarity = one_of(d, at, "similarity", &SIMILARITIES)?
                    .ok_or_else(|| DefinitionError::shape(&join(at, "similarity"), "is required"))?
                    .to_owned();
                let quantization =
                    one_of(d, at, "quantization", &QUANTIZATIONS)?.map(str::to_owned);
                Ok(VectorField::Vector { path, dimensions, similarity, quantization })
            }
            "filter" => {
                only(&["type", "path"])?;
                Ok(VectorField::Filter { path: path()? })
            }
            "autoEmbed" => {
                let model = embedding_model(d, at, &["model", "modality"])?;
                Err(DefinitionError::Unsupported(format!(
                    "CanonicalModel: {model} not registered yet, supported models are: []"
                )))
            }
            _ => Ok(VectorField::Other(d.clone())),
        }
    }

    fn normalized(&self) -> Document {
        match self {
            VectorField::Vector { path, dimensions, similarity, quantization } => {
                let mut d = Document::new()
                    .with("type", "vector")
                    .with("path", path.as_str())
                    .with("numDimensions", int32(*dimensions))
                    .with("similarity", similarity.as_str());
                if let Some(q) = quantization {
                    d.push("quantization", q.as_str());
                }
                d
            }
            VectorField::Filter { path } => {
                Document::new().with("type", "filter").with("path", path.as_str())
            }
            VectorField::Other(d) => d.clone(),
        }
    }
}
