//! `$jsonSchema` (FR-009, FR-022): the draft 4 keywords the reference server accepts, plus
//! `bsonType`, `encrypt` and `encryptMetadata`, without `$ref`, `$schema`, `default`,
//! `definitions`, `format` or `id`. A failed validation explains each rule not satisfied as the
//! reference server's `errInfo` does.

use teseladb_bson::{Document, RawDoc, RawValue, Value, types as t};

use crate::error::{MqlError, Result, bad_value, codes, type_mismatch};
use crate::regex::Regex;
use crate::types::{self, TypeSpec};
use crate::value::{OwnedValue, exact_i64, is_number, to_f64};

mod check;
mod translate;

use check::detail;

/// Keywords, each declared once.
mod kw {
    pub const BSON_TYPE: &str = "bsonType";
    pub const TYPE: &str = "type";
    pub const ENUM: &str = "enum";
    pub const REQUIRED: &str = "required";
    pub const PROPERTIES: &str = "properties";
    pub const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
    pub const PATTERN_PROPERTIES: &str = "patternProperties";
    pub const MIN_PROPERTIES: &str = "minProperties";
    pub const MAX_PROPERTIES: &str = "maxProperties";
    pub const DEPENDENCIES: &str = "dependencies";
    pub const ITEMS: &str = "items";
    pub const ADDITIONAL_ITEMS: &str = "additionalItems";
    pub const MIN_ITEMS: &str = "minItems";
    pub const MAX_ITEMS: &str = "maxItems";
    pub const UNIQUE_ITEMS: &str = "uniqueItems";
    pub const MINIMUM: &str = "minimum";
    pub const MAXIMUM: &str = "maximum";
    pub const EXCLUSIVE_MINIMUM: &str = "exclusiveMinimum";
    pub const EXCLUSIVE_MAXIMUM: &str = "exclusiveMaximum";
    pub const MULTIPLE_OF: &str = "multipleOf";
    pub const MIN_LENGTH: &str = "minLength";
    pub const MAX_LENGTH: &str = "maxLength";
    pub const PATTERN: &str = "pattern";
    pub const ALL_OF: &str = "allOf";
    pub const ANY_OF: &str = "anyOf";
    pub const ONE_OF: &str = "oneOf";
    pub const NOT: &str = "not";
    pub const TITLE: &str = "title";
    pub const DESCRIPTION: &str = "description";
    pub const ENCRYPT: &str = "encrypt";
    pub const ENCRYPT_METADATA: &str = "encryptMetadata";
    /// Keywords of JSON Schema that the reference server does not support.
    pub const UNSUPPORTED: [&str; 6] =
        ["$ref", "$schema", "default", "definitions", "format", "id"];
}

/// JSON Schema's `type` values that the reference server accepts.
const JSON_TYPES: [(&str, &[u8]); 6] = [
    ("object", &[t::DOCUMENT]),
    ("array", &[t::ARRAY]),
    ("number", &[t::INT32, t::INT64, t::DOUBLE, t::DECIMAL128]),
    ("boolean", &[t::BOOLEAN]),
    ("string", &[t::STRING]),
    ("null", &[t::NULL]),
];

/// A parsed schema.
#[derive(Debug)]
pub struct Schema {
    title: Option<String>,
    description: Option<String>,
    rules: Vec<Rule>,
}

#[derive(Debug)]
struct Rule {
    name: &'static str,
    /// `{keyword: argument}` as the user wrote it.
    spec: OwnedValue,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Types(Vec<TypeSpec>),
    JsonTypes(Vec<u8>),
    Enum(Vec<OwnedValue>),
    Required(Vec<String>),
    Properties(Vec<(String, Schema)>),
    AdditionalProperties {
        add: Additional,
        /// Names of `properties` beside it.
        properties: Vec<String>,
        /// Patterns of `patternProperties` beside it.
        patterns: Vec<Regex>,
    },
    PatternProperties(Vec<(Regex, Schema)>),
    MinProperties(i64),
    MaxProperties(i64),
    Dependencies(Vec<(String, Dependency)>),
    Items(Items),
    MinItems(i64),
    MaxItems(i64),
    UniqueItems,
    Minimum {
        bound: OwnedValue,
        exclusive: bool,
    },
    Maximum {
        bound: OwnedValue,
        exclusive: bool,
    },
    MultipleOf(OwnedValue),
    MinLength(i64),
    MaxLength(i64),
    Pattern(Box<Regex>),
    AllOf(Vec<Schema>),
    AnyOf(Vec<Schema>),
    OneOf(Vec<Schema>),
    Not(Box<Schema>),
    /// `encrypt`, with the original types its `bsonType` allows.
    Encrypt(Option<Vec<u8>>),
}

#[derive(Debug)]
enum Additional {
    Allowed(bool),
    Schema(Box<Schema>),
}

#[derive(Debug)]
enum Dependency {
    Properties(Vec<String>),
    Schema(Box<Schema>),
}

#[derive(Debug)]
enum Items {
    All(Box<Schema>),
    Tuple { schemas: Vec<Schema>, additional: Option<Additional> },
}

fn failed(message: impl Into<String>) -> MqlError {
    MqlError::new(codes::FAILED_TO_PARSE, message)
}

impl Schema {
    /// Reads a schema.
    pub fn parse(doc: RawDoc<'_>) -> Result<Schema> {
        let mut schema = Schema { title: None, description: None, rules: Vec::new() };
        let keys: Vec<&str> = doc.iter().map(|(k, _)| k).collect();
        if keys.contains(&kw::TYPE) && keys.contains(&kw::BSON_TYPE) {
            return Err(failed("Cannot specify both $jsonSchema keywords 'type' and 'bsonType'"));
        }
        let get = |k: &str| crate::matcher::field(doc, k);
        let exclusive = |k: &str| -> Result<bool> {
            match get(k) {
                None => Ok(false),
                Some(RawValue::Boolean(b)) => Ok(b),
                Some(_) => {
                    Err(type_mismatch(format!("$jsonSchema keyword '{k}' must be a boolean")))
                }
            }
        };
        for (key, value) in doc.iter() {
            if kw::UNSUPPORTED.contains(&key) {
                return Err(failed(format!(
                    "$jsonSchema keyword '{key}' is not currently supported"
                )));
            }
            let kind = match key {
                kw::TITLE | kw::DESCRIPTION => {
                    let RawValue::String(s) = value else {
                        return Err(type_mismatch(format!(
                            "$jsonSchema keyword '{key}' must be a string"
                        )));
                    };
                    if key == kw::TITLE {
                        schema.title = Some(s.into())
                    } else {
                        schema.description = Some(s.into())
                    }
                    continue;
                }
                kw::EXCLUSIVE_MINIMUM | kw::EXCLUSIVE_MAXIMUM => {
                    exclusive(key)?;
                    let partner =
                        if key == kw::EXCLUSIVE_MINIMUM { kw::MINIMUM } else { kw::MAXIMUM };
                    if get(partner).is_none() {
                        return Err(failed(format!(
                            "$jsonSchema keyword '{partner}' must be a present if {key} is present"
                        )));
                    }
                    continue;
                }
                kw::ENCRYPT_METADATA => {
                    object(key, value)?;
                    continue;
                }
                kw::BSON_TYPE => Kind::Types(bson_types(value)?),
                kw::TYPE => Kind::JsonTypes(json_types(value)?),
                kw::ENUM => {
                    let RawValue::Array(arr) = value else {
                        return Err(type_mismatch("$jsonSchema keyword 'enum' must be an array"));
                    };
                    let items = arr.iter().map(OwnedValue::from_raw).collect::<Result<Vec<_>>>()?;
                    if items.is_empty() {
                        return Err(failed("$jsonSchema keyword 'enum' cannot be an empty array"));
                    }
                    Kind::Enum(items)
                }
                kw::REQUIRED => Kind::Required(names(key, value)?),
                kw::PROPERTIES => Kind::Properties(
                    object(key, value)?
                        .iter()
                        .map(|(k, v)| Ok((k.to_owned(), sub(key, v)?)))
                        .collect::<Result<_>>()?,
                ),
                kw::ADDITIONAL_PROPERTIES => {
                    let names_of = |k: &str| -> Vec<&str> {
                        match get(k) {
                            Some(RawValue::Document(d)) => d.iter().map(|(n, _)| n).collect(),
                            _ => Vec::new(),
                        }
                    };
                    Kind::AdditionalProperties {
                        add: additional(key, value)?,
                        properties: names_of(kw::PROPERTIES)
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        patterns: names_of(kw::PATTERN_PROPERTIES)
                            .into_iter()
                            .map(|p| Regex::new(p, ""))
                            .collect::<Result<_>>()?,
                    }
                }
                kw::PATTERN_PROPERTIES => Kind::PatternProperties(
                    object(key, value)?
                        .iter()
                        .map(|(k, v)| Ok((Regex::new(k, "")?, sub(key, v)?)))
                        .collect::<Result<_>>()?,
                ),
                kw::MIN_PROPERTIES => Kind::MinProperties(count(key, value)?),
                kw::MAX_PROPERTIES => Kind::MaxProperties(count(key, value)?),
                kw::DEPENDENCIES => Kind::Dependencies(
                    object(key, value)?
                        .iter()
                        .map(|(k, v)| {
                            let dep = match v {
                                RawValue::Array(_) => Dependency::Properties(names(key, v)?),
                                RawValue::Document(d) => {
                                    Dependency::Schema(Box::new(Schema::parse(d)?))
                                }
                                _ => {
                                    return Err(type_mismatch(
                                        "property dependencies must be an object or an array",
                                    ));
                                }
                            };
                            Ok((k.to_owned(), dep))
                        })
                        .collect::<Result<_>>()?,
                ),
                kw::ITEMS => Kind::Items(match value {
                    RawValue::Document(d) => Items::All(Box::new(Schema::parse(d)?)),
                    RawValue::Array(arr) => Items::Tuple {
                        schemas: arr.iter().map(|v| sub(key, v)).collect::<Result<_>>()?,
                        additional: get(kw::ADDITIONAL_ITEMS)
                            .map(|v| additional(kw::ADDITIONAL_ITEMS, v))
                            .transpose()?,
                    },
                    _ => {
                        return Err(type_mismatch(
                            "$jsonSchema keyword 'items' must be an array or an object",
                        ));
                    }
                }),
                kw::ADDITIONAL_ITEMS => {
                    additional(key, value)?;
                    continue;
                }
                kw::MIN_ITEMS => Kind::MinItems(count(key, value)?),
                kw::MAX_ITEMS => Kind::MaxItems(count(key, value)?),
                kw::UNIQUE_ITEMS => match value {
                    RawValue::Boolean(true) => Kind::UniqueItems,
                    RawValue::Boolean(false) => continue,
                    _ => {
                        return Err(type_mismatch(
                            "$jsonSchema keyword 'uniqueItems' must be a boolean",
                        ));
                    }
                },
                kw::MINIMUM => Kind::Minimum {
                    bound: number(key, value)?,
                    exclusive: exclusive(kw::EXCLUSIVE_MINIMUM)?,
                },
                kw::MAXIMUM => Kind::Maximum {
                    bound: number(key, value)?,
                    exclusive: exclusive(kw::EXCLUSIVE_MAXIMUM)?,
                },
                kw::MULTIPLE_OF => {
                    let m = number(key, value)?;
                    if to_f64(&m.get()).is_none_or(|x| x <= 0.0 || x.is_nan()) {
                        return Err(failed(
                            "$jsonSchema keyword 'multipleOf' must have a positive value",
                        ));
                    }
                    Kind::MultipleOf(m)
                }
                kw::MIN_LENGTH => Kind::MinLength(count(key, value)?),
                kw::MAX_LENGTH => Kind::MaxLength(count(key, value)?),
                kw::PATTERN => match value {
                    RawValue::String(p) => Kind::Pattern(Box::new(Regex::new(p, "")?)),
                    _ => {
                        return Err(type_mismatch(
                            "$jsonSchema keyword 'pattern' must be a string",
                        ));
                    }
                },
                kw::ALL_OF | kw::ANY_OF | kw::ONE_OF => {
                    let RawValue::Array(arr) = value else {
                        return Err(type_mismatch(format!(
                            "$jsonSchema keyword '{key}' must be an array"
                        )));
                    };
                    let list = arr.iter().map(|v| sub(key, v)).collect::<Result<Vec<_>>>()?;
                    if list.is_empty() {
                        return Err(bad_value(format!(
                            "$jsonSchema keyword '{key}' must be a non-empty array"
                        )));
                    }
                    match key {
                        kw::ALL_OF => Kind::AllOf(list),
                        kw::ANY_OF => Kind::AnyOf(list),
                        _ => Kind::OneOf(list),
                    }
                }
                kw::NOT => Kind::Not(Box::new(sub(key, value)?)),
                kw::ENCRYPT => {
                    object(key, value)?;
                    Kind::Encrypt(encrypt_types(value)?)
                }
                _ => return Err(failed(format!("Unknown $jsonSchema keyword: {key}"))),
            };
            let name = KEYWORDS.iter().copied().find(|k| *k == key).unwrap_or(kw::NOT);
            let mut spec = Document::new().with(key, Value::from(value));
            // An exclusive bound shows beside its bound (measured).
            if let Kind::Minimum { exclusive: true, .. } | Kind::Maximum { exclusive: true, .. } =
                kind
            {
                let flag =
                    if key == kw::MINIMUM { kw::EXCLUSIVE_MINIMUM } else { kw::EXCLUSIVE_MAXIMUM };
                spec.push(flag, true);
            }
            schema.rules.push(Rule { name, spec: OwnedValue::new(Value::Document(spec))?, kind });
        }
        // The order the reference server checks the keywords in (measured on a reference router
        // 9.0.4).
        schema
            .rules
            .sort_by_key(|r| ORDER.iter().position(|k| *k == r.name).unwrap_or(ORDER.len()));
        Ok(schema)
    }

    /// Whether the document satisfies the schema.
    pub fn matches(&self, doc: RawDoc<'_>) -> Result<bool> {
        Ok(self.failures(RawValue::Document(doc), false)?.is_empty())
    }

    /// The explanation of why `doc` does not satisfy the schema, `None` if it does: the
    /// `details` document of `errInfo`.
    pub fn explain(&self, doc: RawDoc<'_>) -> Result<Option<Document>> {
        let rules = self.failures(RawValue::Document(doc), true)?;
        if rules.is_empty() {
            return Ok(None);
        }
        let mut d = Document::new().with(detail::OPERATOR_NAME, crate::ops::JSON_SCHEMA);
        if let Some(title) = &self.title {
            d.insert(detail::TITLE, title.as_str());
        }
        d.insert(
            detail::SCHEMA_RULES_NOT_SATISFIED,
            rules.into_iter().map(Value::Document).collect::<Vec<_>>(),
        );
        Ok(Some(d))
    }

    /// The rules `v` does not satisfy, explained.
    /// What the reference server's explanation of a failed document says of a missing property
    /// whose schema has `oneOf` with several sub-schemas: every one of them unsatisfied, with no
    /// details, although the missing property does not make the document fail (measured on a
    /// reference router 9.0.4).
    fn missing_one_of(&self, name: &str) -> Option<Document> {
        let rule = self.rules.iter().find(|r| matches!(&r.kind, Kind::OneOf(l) if l.len() > 1))?;
        let Kind::OneOf(list) = &rule.kind else { return None };
        let entries: Vec<Value> = (0..list.len())
            .map(|i| {
                Value::Document(
                    Document::new()
                        .with(check::detail::INDEX, i32::try_from(i).unwrap_or(i32::MAX))
                        .with(check::detail::DETAILS, Vec::<Value>::new()),
                )
            })
            .collect();
        let one_of = Document::new()
            .with(check::detail::OPERATOR_NAME, kw::ONE_OF)
            .with(check::detail::SCHEMAS_NOT_SATISFIED, entries);
        Some(
            Document::new()
                .with(check::detail::PROPERTY_NAME, name)
                .with(check::detail::DETAILS, vec![Value::Document(one_of)]),
        )
    }

    /// The rules `v` breaks; `explaining` adds what the reference server's explanation adds (see
    /// [`Rule::check`]).
    fn failures(&self, v: RawValue<'_>, explaining: bool) -> Result<Vec<Document>> {
        let mut out = Vec::new();
        for rule in &self.rules {
            if let Some(d) = rule.check(v, explaining)? {
                out.push(d);
            }
            out.extend(rule.additional_items(v));
        }
        Ok(out)
    }

    /// The explanation of a sub-schema: its rules, with its title and description.
    fn nested_details(
        &self,
        v: RawValue<'_>,
        explaining: bool,
    ) -> Result<Option<(Vec<Document>, &Schema)>> {
        let f = self.failures(v, explaining)?;
        Ok((!f.is_empty()).then_some((f, self)))
    }
}

/// The order of the keywords' failures in `errInfo`: those of a value's type, then the logical
/// ones, then the type itself.
const ORDER: [&str; 26] = [
    kw::MULTIPLE_OF,
    kw::MAXIMUM,
    kw::MINIMUM,
    kw::PATTERN,
    kw::MAX_LENGTH,
    kw::MIN_LENGTH,
    kw::MIN_ITEMS,
    kw::MAX_ITEMS,
    kw::UNIQUE_ITEMS,
    kw::ITEMS,
    kw::ADDITIONAL_ITEMS,
    kw::PROPERTIES,
    kw::ADDITIONAL_PROPERTIES,
    kw::PATTERN_PROPERTIES,
    kw::REQUIRED,
    kw::MIN_PROPERTIES,
    kw::MAX_PROPERTIES,
    kw::DEPENDENCIES,
    kw::ENCRYPT,
    kw::ALL_OF,
    kw::ANY_OF,
    kw::ONE_OF,
    kw::NOT,
    kw::ENUM,
    kw::BSON_TYPE,
    kw::TYPE,
];

/// Every keyword that becomes a rule, for its static name.
const KEYWORDS: [&str; 26] = [
    kw::BSON_TYPE,
    kw::TYPE,
    kw::ENUM,
    kw::REQUIRED,
    kw::PROPERTIES,
    kw::ADDITIONAL_PROPERTIES,
    kw::PATTERN_PROPERTIES,
    kw::MIN_PROPERTIES,
    kw::MAX_PROPERTIES,
    kw::DEPENDENCIES,
    kw::ITEMS,
    kw::MIN_ITEMS,
    kw::MAX_ITEMS,
    kw::UNIQUE_ITEMS,
    kw::MINIMUM,
    kw::MAXIMUM,
    kw::MULTIPLE_OF,
    kw::MIN_LENGTH,
    kw::MAX_LENGTH,
    kw::PATTERN,
    kw::ALL_OF,
    kw::ANY_OF,
    kw::ONE_OF,
    kw::NOT,
    kw::ENCRYPT,
    kw::ADDITIONAL_ITEMS,
];

fn sub(key: &str, v: RawValue<'_>) -> Result<Schema> {
    match v {
        RawValue::Document(d) => Schema::parse(d),
        _ => Err(type_mismatch(format!("$jsonSchema keyword '{key}' must be an object"))),
    }
}

fn object<'a>(key: &str, v: RawValue<'a>) -> Result<RawDoc<'a>> {
    match v {
        RawValue::Document(d) => Ok(d),
        _ => Err(type_mismatch(format!("$jsonSchema keyword '{key}' must be an object"))),
    }
}

fn additional(key: &str, v: RawValue<'_>) -> Result<Additional> {
    match v {
        RawValue::Boolean(b) => Ok(Additional::Allowed(b)),
        RawValue::Document(d) => Ok(Additional::Schema(Box::new(Schema::parse(d)?))),
        _ => Err(type_mismatch(format!(
            "$jsonSchema keyword '{key}' must be either an object or a boolean"
        ))),
    }
}

fn names(key: &str, v: RawValue<'_>) -> Result<Vec<String>> {
    let RawValue::Array(arr) = v else {
        return Err(type_mismatch(format!("$jsonSchema keyword '{key}' must be an array")));
    };
    let mut out: Vec<String> = Vec::new();
    for item in arr.iter() {
        let RawValue::String(s) = item else {
            return Err(type_mismatch(format!(
                "$jsonSchema keyword '{key}' must contain only strings"
            )));
        };
        if out.iter().any(|o| o == s) {
            return Err(failed(format!(
                "$jsonSchema keyword '{key}' array cannot contain duplicate values"
            )));
        }
        out.push(s.to_owned());
    }
    if out.is_empty() {
        return Err(bad_value(format!("$jsonSchema keyword '{key}' cannot be an empty array")));
    }
    Ok(out)
}

fn count(key: &str, v: RawValue<'_>) -> Result<i64> {
    if !is_number(&v) {
        return Err(type_mismatch(format!("$jsonSchema keyword '{key}' must be a number")));
    }
    match exact_i64(&v) {
        Some(n) if n >= 0 => Ok(n),
        _ => Err(failed(format!(
            "$jsonSchema keyword '{key}' must be a representable non-negative integer"
        ))),
    }
}

fn number(key: &str, v: RawValue<'_>) -> Result<OwnedValue> {
    if !is_number(&v) {
        return Err(type_mismatch(format!("$jsonSchema keyword '{key}' must be a number")));
    }
    OwnedValue::from_raw(v)
}

fn bson_types(v: RawValue<'_>) -> Result<Vec<TypeSpec>> {
    let one = |v: RawValue<'_>| match v {
        RawValue::String(s) => {
            types::by_alias(s).ok_or_else(|| bad_value(format!("Unknown type name alias: {s}")))
        }
        _ => Err(type_mismatch(
            "$jsonSchema keyword 'bsonType' must be a string or an array of strings",
        )),
    };
    match v {
        RawValue::Array(arr) => {
            let list = arr.iter().map(one).collect::<Result<Vec<_>>>()?;
            if list.is_empty() {
                return Err(failed("$jsonSchema keyword 'bsonType' must match at least one type"));
            }
            Ok(list)
        }
        v => Ok(vec![one(v)?]),
    }
}

fn json_types(v: RawValue<'_>) -> Result<Vec<u8>> {
    let one = |v: RawValue<'_>| -> Result<&'static [u8]> {
        let RawValue::String(s) = v else {
            return Err(type_mismatch(
                "$jsonSchema keyword 'type' must be a string or an array of strings",
            ));
        };
        if s == "integer" {
            return Err(failed("$jsonSchema type 'integer' is not currently supported."));
        }
        JSON_TYPES
            .iter()
            .find(|(n, _)| *n == s)
            .map(|(_, e)| *e)
            .ok_or_else(|| bad_value(format!("Unknown type name alias: {s}")))
    };
    let lists = match v {
        RawValue::Array(arr) => arr.iter().map(one).collect::<Result<Vec<_>>>()?,
        v => vec![one(v)?],
    };
    if lists.is_empty() {
        return Err(failed("$jsonSchema keyword 'type' must match at least one type"));
    }
    Ok(lists.concat())
}

/// The original types `encrypt.bsonType` allows: an alias or an array of aliases.
fn encrypt_types(value: RawValue<'_>) -> Result<Option<Vec<u8>>> {
    let RawValue::Document(d) = value else { return Ok(None) };
    let alias = |v: RawValue<'_>| match v {
        RawValue::String(a) => match crate::types::by_alias(a) {
            Some(crate::types::TypeSpec::Element(e)) => Ok(e),
            _ => Err(bad_value(format!("Unknown type name alias: {a}"))),
        },
        _ => Err(type_mismatch(
            "$jsonSchema keyword 'bsonType' must be a string or an array of strings",
        )),
    };
    match d.get(kw::BSON_TYPE) {
        None => Ok(None),
        Some(RawValue::Array(a)) => a.iter().map(alias).collect::<Result<Vec<u8>>>().map(Some),
        Some(v) => alias(v).map(|e| Some(vec![e])),
    }
}
