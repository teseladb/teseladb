//! A schema as the filter RefDb translates it into (`parsedQuery` of `explain`, measured on a
//! `refrouter` 9.0.4 on 01-11-2026): each keyword a clause of RefDb's internal operators on the
//! path it constrains. A keyword for one kind of value (a bound for numbers, a length for
//! strings, a count for arrays, the keywords of an object) holds only for values of that kind,
//! unless the schema's own type says every value is of it; a property that is not required may
//! be missing; a subschema of the elements or of other properties names them `i`.

use teseladb_bson::{Document, Value};

use super::{Additional, Dependency, Items, Kind, Schema};
use crate::clause::{Clause, rank};
use crate::types::TypeSpec;

/// The name a subschema of array elements or of properties gives the value it checks.
const PLACEHOLDER: &str = "i";

/// The kinds of value a keyword constrains, as `$_internalSchemaType` lists them.
#[derive(Clone, Copy, PartialEq)]
enum Class {
    Number,
    String,
    Array,
    Object,
}

/// The BSON codes of the numeric types.
const NUMBER_CODES: [u8; 4] = [1, 16, 18, 19];
/// The codes of strings, documents and arrays.
const STRING_CODE: u8 = 2;
const OBJECT_CODE: u8 = 3;
const ARRAY_CODE: u8 = 4;

impl Class {
    fn holds(self, t: Option<u8>) -> bool {
        match (self, t) {
            (Class::Number, None) => true,
            (Class::Number, Some(c)) => NUMBER_CODES.contains(&c),
            (Class::String, Some(c)) => c == STRING_CODE,
            (Class::Array, Some(c)) => c == ARRAY_CODE,
            (Class::Object, Some(c)) => c == OBJECT_CODE,
            (_, None) => false,
        }
    }

    fn listed(self) -> Value {
        Value::Array(vec![match self {
            Class::Number => Value::String("number".to_owned()),
            Class::String => Value::Int32(i32::from(STRING_CODE)),
            Class::Array => Value::Int32(i32::from(ARRAY_CODE)),
            Class::Object => Value::Int32(i32::from(OBJECT_CODE)),
        }])
    }
}

impl Schema {
    /// The schema as the filter RefDb translates it into.
    pub(crate) fn translated(&self) -> Clause {
        self.at(None)
    }

    /// The types the schema's own `bsonType` or `type` allows (`None` for "number"), if it
    /// names any.
    fn own_types(&self) -> Option<Vec<Option<u8>>> {
        self.rules.iter().find_map(|r| match &r.kind {
            Kind::Types(specs) => Some(
                specs
                    .iter()
                    .map(|s| match s {
                        TypeSpec::Element(t) => Some(*t),
                        TypeSpec::Number => None,
                    })
                    .collect(),
            ),
            Kind::JsonTypes(codes) => Some(json_types(codes)),
            _ => None,
        })
    }

    /// The clause of the schema for the value at `path` (`None`: the document checked itself).
    fn at(&self, path: Option<&str>) -> Clause {
        let mut clauses = Vec::new();
        let mut object = Vec::new();
        let required: Vec<&str> = self
            .rules
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Required(names) => Some(names.iter().map(String::as_str)),
                _ => None,
            })
            .flatten()
            .collect();
        for r in &self.rules {
            match (&r.kind, path) {
                (Kind::Types(_) | Kind::JsonTypes(_), None) => {
                    let types = self.own_types().unwrap_or_default();
                    clauses.push(if types.iter().any(|t| Class::Object.holds(*t)) {
                        Clause::True
                    } else {
                        Clause::False
                    });
                }
                (Kind::Types(_) | Kind::JsonTypes(_), Some(p)) => {
                    clauses.push(type_clause(p, &self.own_types().unwrap_or_default()));
                }
                (Kind::Enum(values), None) => clauses.push(Clause::Or(
                    values
                        .iter()
                        .map(|v| {
                            other(
                                rank::SCHEMA_ROOT_DOC_EQ,
                                "$_internalSchemaRootDocEq",
                                v.to_value(),
                            )
                        })
                        .collect(),
                )),
                (Kind::Enum(values), Some(p)) => clauses.push(Clause::Or(
                    values
                        .iter()
                        .map(|v| {
                            Clause::field(rank::SCHEMA_EQ, p, "$_internalSchemaEq", v.to_value())
                        })
                        .collect(),
                )),
                (Kind::AllOf(subs), _) => {
                    clauses.push(Clause::And(subs.iter().map(|s| s.at(path)).collect()));
                }
                (Kind::AnyOf(subs), _) => {
                    clauses.push(Clause::Or(subs.iter().map(|s| s.at(path)).collect()));
                }
                (Kind::OneOf(subs), _) => clauses.push(other(
                    rank::SCHEMA_XOR,
                    "$_internalSchemaXor",
                    Value::Array(
                        subs.iter().map(|s| Value::Document(written(s.at(path)))).collect(),
                    ),
                )),
                (Kind::Not(s), _) => clauses.push(Clause::not(s.at(path).simplified())),
                (Kind::Encrypt(_), _) => {}
                (k, None) if is_object_keyword(k) => {
                    clauses.push(self.object_keyword(k, &required));
                }
                (k, Some(_)) if is_object_keyword(k) => {
                    object.push(self.object_keyword(k, &required))
                }
                (_, None) => {}
                (k, Some(p)) => clauses.extend(self.value_keyword(k, p)),
            }
        }
        if let Some(p) = path
            && !object.is_empty()
        {
            let inner = written(Clause::And(object));
            let matched =
                Clause::field(rank::SCHEMA_OBJECT_MATCH, p, "$_internalSchemaObjectMatch", inner);
            clauses.push(self.restricted(Class::Object, p, matched));
        }
        Clause::And(clauses)
    }

    /// A keyword of the document at hand's own fields.
    fn object_keyword(&self, k: &Kind, required: &[&str]) -> Clause {
        match k {
            Kind::Required(names) => Clause::And(
                names.iter().map(|n| Clause::field(rank::EXISTS, n, "$exists", true)).collect(),
            ),
            Kind::Properties(props) => {
                let mut sorted: Vec<&(String, Schema)> = props.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                Clause::And(
                    sorted
                        .into_iter()
                        .map(|(name, s)| {
                            let c = s.at(Some(name));
                            if required.contains(&name.as_str()) {
                                c
                            } else {
                                Clause::Or(vec![missing(name), c])
                            }
                        })
                        .collect(),
                )
            }
            Kind::MinProperties(n) => other(
                rank::SCHEMA_MIN_PROPERTIES,
                "$_internalSchemaMinProperties",
                Value::Int64(*n),
            ),
            Kind::MaxProperties(n) => other(
                rank::SCHEMA_MAX_PROPERTIES,
                "$_internalSchemaMaxProperties",
                Value::Int64(*n),
            ),
            Kind::AdditionalProperties { add, properties, .. } => {
                let otherwise = match add {
                    Additional::Allowed(true) => written(Clause::True),
                    Additional::Allowed(false) => written(Clause::False),
                    Additional::Schema(s) => written(s.at(Some(PLACEHOLDER))),
                };
                self.allowed(properties.clone(), otherwise)
            }
            Kind::PatternProperties(_) => {
                // With `additionalProperties` beside it, that keyword's clause has the patterns.
                if self.rules.iter().any(|r| matches!(r.kind, Kind::AdditionalProperties { .. })) {
                    return Clause::True;
                }
                let names = self.property_names();
                self.allowed(names, written(Clause::True))
            }
            Kind::Dependencies(deps) => Clause::And(
                deps.iter()
                    .map(|(name, dep)| {
                        let then = match dep {
                            Dependency::Properties(names) => written(Clause::And(
                                names
                                    .iter()
                                    .map(|n| Clause::field(rank::EXISTS, n, "$exists", true))
                                    .collect(),
                            )),
                            Dependency::Schema(s) => written(s.at(None)),
                        };
                        let present = Value::Document(
                            Document::new()
                                .with(name.as_str(), Document::new().with("$exists", true)),
                        );
                        other(
                            rank::SCHEMA_COND,
                            "$_internalSchemaCond",
                            Value::Array(vec![
                                present,
                                Value::Document(then),
                                Value::Document(written(Clause::True)),
                            ]),
                        )
                    })
                    .collect(),
            ),
            _ => Clause::True,
        }
    }

    /// The names of the schema's `properties`.
    fn property_names(&self) -> Vec<String> {
        self.rules
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::Properties(props) => Some(props.iter().map(|(n, _)| n.clone())),
                _ => None,
            })
            .flatten()
            .collect()
    }

    /// `$_internalSchemaAllowedProperties` over `properties` and the schema's patterns.
    fn allowed(&self, properties: Vec<String>, otherwise: Document) -> Clause {
        let patterns: Vec<Value> = self
            .rules
            .iter()
            .filter_map(|r| match &r.kind {
                Kind::PatternProperties(list) => Some(list.iter().map(|(re, s)| {
                    Value::Document(
                        Document::new()
                            .with(
                                "regex",
                                Value::Regex {
                                    pattern: re.pattern().to_owned(),
                                    options: re.options().to_owned(),
                                },
                            )
                            .with("expression", written(s.at(Some(PLACEHOLDER)))),
                    )
                })),
                _ => None,
            })
            .flatten()
            .collect();
        let spec = Document::new()
            .with("properties", Value::Array(properties.into_iter().map(Value::String).collect()))
            .with("namePlaceholder", PLACEHOLDER)
            .with("patternProperties", Value::Array(patterns))
            .with("otherwise", otherwise);
        other(rank::SCHEMA_ALLOWED_PROPERTIES, "$_internalSchemaAllowedProperties", spec)
    }

    /// A keyword of the value at `p` that holds for one kind of value.
    fn value_keyword(&self, k: &Kind, p: &str) -> Vec<Clause> {
        let (class, inner) = match k {
            Kind::Minimum { bound, exclusive } => {
                let (r, op) = if *exclusive { (rank::GT, "$gt") } else { (rank::GTE, "$gte") };
                (Class::Number, Clause::field(r, p, op, bound.to_value()))
            }
            Kind::Maximum { bound, exclusive } => {
                let (r, op) = if *exclusive { (rank::LT, "$lt") } else { (rank::LTE, "$lte") };
                (Class::Number, Clause::field(r, p, op, bound.to_value()))
            }
            Kind::MultipleOf(m) => {
                let d = crate::arith::to_decimal128(&m.to_value())
                    .map_or(Value::Null, Value::Decimal128);
                let zero = crate::arith::to_decimal128(&Value::Int32(0))
                    .map_or(Value::Null, Value::Decimal128);
                let args = Value::Array(vec![d, zero]);
                (Class::Number, Clause::field(rank::SCHEMA_FMOD, p, "$_internalSchemaFmod", args))
            }
            Kind::MinLength(n) => (
                Class::String,
                Clause::field(
                    rank::SCHEMA_MIN_LENGTH,
                    p,
                    "$_internalSchemaMinLength",
                    Value::Int64(*n),
                ),
            ),
            Kind::MaxLength(n) => (
                Class::String,
                Clause::field(
                    rank::SCHEMA_MAX_LENGTH,
                    p,
                    "$_internalSchemaMaxLength",
                    Value::Int64(*n),
                ),
            ),
            Kind::Pattern(re) => {
                // `pattern` takes no flags.
                let pred = Document::new().with("$regex", re.pattern());
                let c = Clause::Field { rank: rank::REGEX, path: p.to_owned(), pred, eq: None };
                (Class::String, c)
            }
            Kind::MinItems(n) => (
                Class::Array,
                Clause::field(
                    rank::SCHEMA_MIN_ITEMS,
                    p,
                    "$_internalSchemaMinItems",
                    Value::Int64(*n),
                ),
            ),
            Kind::MaxItems(n) => (
                Class::Array,
                Clause::field(
                    rank::SCHEMA_MAX_ITEMS,
                    p,
                    "$_internalSchemaMaxItems",
                    Value::Int64(*n),
                ),
            ),
            Kind::UniqueItems => (
                Class::Array,
                Clause::field(rank::SCHEMA_UNIQUE_ITEMS, p, "$_internalSchemaUniqueItems", true),
            ),
            Kind::Items(Items::All(s)) => {
                (Class::Array, from_index(p, 0, written_or_empty(s.at(Some(PLACEHOLDER)))))
            }
            Kind::Items(Items::Tuple { schemas, additional }) => {
                let mut out = Vec::new();
                match additional {
                    Some(Additional::Allowed(false)) => {
                        let c = from_index(p, schemas.len(), written(Clause::False));
                        out.push(self.restricted(Class::Array, p, c));
                    }
                    Some(Additional::Schema(s)) => {
                        let c =
                            from_index(p, schemas.len(), written_or_empty(s.at(Some(PLACEHOLDER))));
                        out.push(self.restricted(Class::Array, p, c));
                    }
                    Some(Additional::Allowed(true)) | None => {}
                }
                let each = schemas
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let spec = Document::new()
                            .with("index", Value::Int64(i64::try_from(i).unwrap_or(i64::MAX)))
                            .with("namePlaceholder", PLACEHOLDER)
                            .with("expression", written_or_empty(s.at(Some(PLACEHOLDER))));
                        Clause::field(
                            rank::SCHEMA_MATCH_ARRAY_INDEX,
                            p,
                            "$_internalSchemaMatchArrayIndex",
                            spec,
                        )
                    })
                    .collect();
                out.push(self.restricted(Class::Array, p, Clause::And(each)));
                return out;
            }
            _ => return Vec::new(),
        };
        vec![self.restricted(class, p, inner)]
    }

    /// `inner`, which holds for values of `class` at `p`: alone when the schema's type allows
    /// only them, nothing when it allows none of them, else beside the values of other kinds.
    fn restricted(&self, class: Class, p: &str, inner: Clause) -> Clause {
        match self.own_types() {
            Some(types) if types.iter().all(|t| class.holds(*t)) => inner,
            Some(types) if !types.iter().any(|t| class.holds(*t)) => Clause::True,
            _ => {
                let kind =
                    Clause::field(rank::SCHEMA_TYPE, p, "$_internalSchemaType", class.listed());
                Clause::Or(vec![Clause::not(kind), inner])
            }
        }
    }
}

/// `$_internalSchemaType` of `types` at `p`: "number" first, then the codes in order.
fn type_clause(p: &str, types: &[Option<u8>]) -> Clause {
    let mut codes: Vec<u8> = types.iter().filter_map(|t| *t).collect();
    codes.sort_unstable();
    codes.dedup();
    let mut list: Vec<Value> = Vec::new();
    if types.contains(&None) {
        list.push(Value::String("number".to_owned()));
    }
    list.extend(codes.into_iter().map(|c| Value::Int32(i32::from(c))));
    Clause::field(rank::SCHEMA_TYPE, p, "$_internalSchemaType", Value::Array(list))
}

/// The types of `type`'s codes, the four numeric ones together as "number".
fn json_types(codes: &[u8]) -> Vec<Option<u8>> {
    let mut out: Vec<Option<u8>> =
        codes.iter().filter(|c| !NUMBER_CODES.contains(c)).map(|c| Some(*c)).collect();
    if NUMBER_CODES.iter().all(|n| codes.contains(n)) {
        out.insert(0, None);
    }
    out
}

/// `$_internalSchemaAllElemMatchFromIndex` of the elements of `p` from `index` on.
fn from_index(p: &str, index: usize, expression: Document) -> Clause {
    let args = Value::Array(vec![
        Value::Int64(i64::try_from(index).unwrap_or(i64::MAX)),
        Value::Document(expression),
    ]);
    Clause::field(
        rank::SCHEMA_ALL_ELEM_MATCH_FROM_INDEX,
        p,
        "$_internalSchemaAllElemMatchFromIndex",
        args,
    )
}

/// `{p: {$not: {$exists: true}}}`.
fn missing(p: &str) -> Clause {
    Clause::not(Clause::field(rank::EXISTS, p, "$exists", true))
}

/// A clause of its own: `{op: arg}`.
fn other(r: u8, op: &str, arg: impl Into<Value>) -> Clause {
    Clause::Other { rank: r, doc: Document::new().with(op, arg) }
}

/// Whether keyword `k` constrains the fields of a document.
fn is_object_keyword(k: &Kind) -> bool {
    matches!(
        k,
        Kind::Required(_)
            | Kind::Properties(_)
            | Kind::MinProperties(_)
            | Kind::MaxProperties(_)
            | Kind::AdditionalProperties { .. }
            | Kind::PatternProperties(_)
            | Kind::Dependencies(_)
    )
}

/// Clause `c` rewritten and written.
fn written(c: Clause) -> Document {
    c.simplified().written()
}

/// Clause `c` written, `{}` when it holds for every value.
fn written_or_empty(c: Clause) -> Document {
    match c.simplified() {
        Clause::True => Document::new(),
        other => other.written(),
    }
}
