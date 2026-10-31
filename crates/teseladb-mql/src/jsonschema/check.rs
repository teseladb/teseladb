//! Checking a value against a parsed schema, and the explanation of each rule it breaks, with
//! the fields of RefDb's `errInfo`.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawDoc, RawValue, Value, types as t};

use super::{Additional, Dependency, Items, Kind, Rule, Schema, kw};
use crate::error::Result;
use crate::regex::Regex;
use crate::types;
use crate::value::{cmp, exact_i64, is_nan, is_number, to_f64};

/// Fields of the explanation of a failed rule, each declared once.
pub(super) mod detail {
    pub const OPERATOR_NAME: &str = "operatorName";
    pub const SPECIFIED_AS: &str = "specifiedAs";
    pub const REASON: &str = "reason";
    pub const CONSIDERED_VALUE: &str = "consideredValue";
    pub const CONSIDERED_TYPE: &str = "consideredType";
    pub const SCHEMA_RULES_NOT_SATISFIED: &str = "schemaRulesNotSatisfied";
    pub const PROPERTIES_NOT_SATISFIED: &str = "propertiesNotSatisfied";
    pub const PROPERTY_NAME: &str = "propertyName";
    pub const DETAILS: &str = "details";
    pub const MISSING_PROPERTIES: &str = "missingProperties";
    pub const ADDITIONAL_PROPERTIES: &str = "additionalProperties";
    pub const ITEM_INDEX: &str = "itemIndex";
    pub const SCHEMAS_NOT_SATISFIED: &str = "schemasNotSatisfied";
    pub const MATCHING_SCHEMA_INDEXES: &str = "matchingSchemaIndexes";
    pub const INDEX: &str = "index";
    pub const DUPLICATED_VALUE: &str = "duplicatedValue";
    pub const NUMBER_OF_PROPERTIES: &str = "numberOfProperties";
    pub const FAILING_DEPENDENCIES: &str = "failingDependencies";
    pub const CONDITIONAL_PROPERTY: &str = "conditionalProperty";
    pub const REGEX_MATCHED: &str = "regexMatched";
    pub const NUMBER_OF_ITEMS: &str = "numberOfItems";
    pub const FAILING_PROPERTY: &str = "failingProperty";
    pub const ADDITIONAL_ITEMS: &str = "additionalItems";
    pub const TITLE: &str = "title";
    pub const DESCRIPTION: &str = "description";
}

/// A failed rule's explanation, with what was considered.
fn fail(rule: &Rule, reason: &str, considered: Option<RawValue<'_>>) -> Document {
    let mut d = base(rule);
    d.insert(detail::REASON, reason);
    if let Some(v) = considered {
        d.insert(detail::CONSIDERED_VALUE, Value::from(v));
    }
    d
}

fn base(rule: &Rule) -> Document {
    Document::new()
        .with(detail::OPERATOR_NAME, rule.name)
        .with(detail::SPECIFIED_AS, rule.spec.to_value())
}

/// The explanation of a sub-schema's failures, with its title and description.
pub(super) fn nested(schema: &Schema, details: Vec<Document>, mut into: Document) -> Document {
    if let Some(title) = &schema.title {
        into.insert(detail::TITLE, title.as_str());
    }
    if let Some(d) = &schema.description {
        into.insert(detail::DESCRIPTION, d.as_str());
    }
    into.insert(detail::DETAILS, details.into_iter().map(Value::Document).collect::<Vec<_>>());
    into
}

/// The reason of `items` with one schema for every item.
const ITEMS_REASON: &str = "At least one item did not match the sub-schema";

impl Rule {
    pub(super) fn check(&self, v: RawValue<'_>, explaining: bool) -> Result<Option<Document>> {
        let doc = match v {
            RawValue::Document(d) => Some(d),
            _ => None,
        };
        let arr: Option<Vec<RawValue<'_>>> = match v {
            RawValue::Array(a) => Some(a.iter().collect()),
            _ => None,
        };
        Ok(match &self.kind {
            Kind::Types(specs) => {
                (!specs.iter().any(|s| s.matches(v.element_type()))).then(|| type_fail(self, v))
            }
            Kind::JsonTypes(list) => {
                (!list.contains(&v.element_type())).then(|| type_fail(self, v))
            }
            Kind::Enum(items) => (!items.iter().any(|i| cmp(i.get(), v, None) == Ordering::Equal))
                .then(|| fail(self, "value was not found in enum", Some(v))),
            Kind::Required(names) => doc.and_then(|d| {
                let missing: Vec<Value> = names
                    .iter()
                    .filter(|n| crate::matcher::field(d, n).is_none())
                    .map(|n| Value::from(n.as_str()))
                    .collect();
                (!missing.is_empty()).then(|| base(self).with(detail::MISSING_PROPERTIES, missing))
            }),
            Kind::Properties(props) => match doc {
                None => None,
                Some(d) => {
                    let mut bad = Vec::new();
                    for (name, schema) in props {
                        let Some(pv) = crate::matcher::field(d, name) else {
                            if explaining && let Some(e) = schema.missing_one_of(name) {
                                bad.push(Value::Document(e));
                            }
                            continue;
                        };
                        if let Some((details, s)) = schema.nested_details(pv, explaining)? {
                            bad.push(Value::Document(nested(
                                s,
                                details,
                                Document::new().with(detail::PROPERTY_NAME, name.as_str()),
                            )));
                        }
                    }
                    (!bad.is_empty()).then(|| {
                        Document::new()
                            .with(detail::OPERATOR_NAME, self.name)
                            .with(detail::PROPERTIES_NOT_SATISFIED, bad)
                    })
                }
            },
            Kind::AdditionalProperties { add, properties, patterns } => match doc {
                None => None,
                Some(d) => self.additional_properties(add, properties, patterns, d, explaining)?,
            },
            Kind::PatternProperties(list) => match doc {
                None => None,
                Some(d) => {
                    let mut bad = Vec::new();
                    for (k, pv) in d.iter() {
                        for (re, schema) in list {
                            if !re.is_match(k)? {
                                continue;
                            }
                            if let Some((details, s)) = schema.nested_details(pv, explaining)? {
                                bad.push(Value::Document(nested(
                                    s,
                                    details,
                                    Document::new()
                                        .with(detail::PROPERTY_NAME, k)
                                        .with(detail::REGEX_MATCHED, re.pattern()),
                                )));
                            }
                        }
                    }
                    (!bad.is_empty()).then(|| {
                        Document::new()
                            .with(detail::OPERATOR_NAME, self.name)
                            .with(detail::DETAILS, bad)
                    })
                }
            },
            Kind::MinProperties(n) | Kind::MaxProperties(n) => doc.and_then(|d| {
                let c = i64::try_from(d.iter().count()).unwrap_or(i64::MAX);
                let ok =
                    if matches!(self.kind, Kind::MinProperties(_)) { c >= *n } else { c <= *n };
                (!ok).then(|| {
                    fail(self, "specified number of properties was not satisfied", None)
                        .with(detail::NUMBER_OF_PROPERTIES, i32::try_from(c).unwrap_or(i32::MAX))
                })
            }),
            Kind::Dependencies(deps) => match doc {
                None => None,
                Some(d) => self.dependencies(deps, d, explaining)?,
            },
            Kind::Items(items) => match &arr {
                None => None,
                Some(elems) => self.items(items, elems, explaining)?,
            },
            Kind::MinItems(n) | Kind::MaxItems(n) => arr.as_ref().and_then(|elems| {
                let c = i64::try_from(elems.len()).unwrap_or(i64::MAX);
                let ok = if matches!(self.kind, Kind::MinItems(_)) { c >= *n } else { c <= *n };
                (!ok).then(|| {
                    fail(self, "array did not match specified length", Some(v))
                        .with(detail::NUMBER_OF_ITEMS, i32::try_from(c).unwrap_or(i32::MAX))
                })
            }),
            Kind::UniqueItems => arr.as_ref().and_then(|elems| {
                let dup = elems.iter().enumerate().find(|(i, a)| {
                    elems[..*i].iter().any(|b| cmp(**a, *b, None) == Ordering::Equal)
                });
                dup.map(|(_, d)| {
                    fail(self, "found a duplicate item", Some(v))
                        .with(detail::DUPLICATED_VALUE, Value::from(*d))
                })
            }),
            Kind::Minimum { bound, exclusive } | Kind::Maximum { bound, exclusive } => {
                if !is_number(&v) {
                    return Ok(None);
                }
                let o = cmp(v, bound.get(), None);
                let min = matches!(self.kind, Kind::Minimum { .. });
                let ok = !is_nan(&v)
                    && match (min, exclusive) {
                        (true, false) => o != Ordering::Less,
                        (true, true) => o == Ordering::Greater,
                        (false, false) => o != Ordering::Greater,
                        (false, true) => o == Ordering::Less,
                    };
                (!ok).then(|| fail(self, "comparison failed", Some(v)))
            }
            Kind::MultipleOf(m) => {
                if !is_number(&v) {
                    return Ok(None);
                }
                (!multiple_of(v, m.get())).then(|| {
                    fail(self, "considered value is not a multiple of the specified value", Some(v))
                })
            }
            Kind::MinLength(n) | Kind::MaxLength(n) => match v {
                RawValue::String(s) => {
                    let c = i64::try_from(s.chars().count()).unwrap_or(i64::MAX);
                    let ok =
                        if matches!(self.kind, Kind::MinLength(_)) { c >= *n } else { c <= *n };
                    (!ok).then(|| fail(self, "specified string length was not satisfied", Some(v)))
                }
                _ => None,
            },
            Kind::Pattern(re) => match v {
                RawValue::String(s) => (!re.is_match(s)?)
                    .then(|| fail(self, "regular expression did not match", Some(v))),
                _ => None,
            },
            Kind::AllOf(list) | Kind::AnyOf(list) | Kind::OneOf(list) => {
                self.combination(list, v, explaining)?
            }
            Kind::Not(schema) => schema.failures(v, false)?.is_empty().then(|| {
                Document::new()
                    .with(detail::OPERATOR_NAME, self.name)
                    .with(detail::REASON, "child expression matched")
            }),
            Kind::Encrypt => {
                let ok = matches!(v, RawValue::Binary { subtype: t::BINARY_ENCRYPTED, .. });
                (!ok).then(|| fail(self, "value was not encrypted", None))
            }
        })
    }

    /// `additionalItems: false` after `items` with a schema per position: its own failure, with
    /// the items past them (measured on a `refrouter` 9.0.4).
    pub(super) fn additional_items(&self, v: RawValue<'_>) -> Option<Document> {
        let Kind::Items(Items::Tuple { schemas, additional: Some(Additional::Allowed(false)) }) =
            &self.kind
        else {
            return None;
        };
        let RawValue::Array(a) = v else { return None };
        let extra: Vec<Value> = a.iter().skip(schemas.len()).map(Value::from).collect();
        (!extra.is_empty()).then(|| {
            Document::new()
                .with(detail::OPERATOR_NAME, kw::ADDITIONAL_ITEMS)
                .with(detail::SPECIFIED_AS, Document::new().with(kw::ADDITIONAL_ITEMS, false))
                .with(detail::REASON, "found additional items")
                .with(detail::ADDITIONAL_ITEMS, extra)
        })
    }

    fn additional_properties(
        &self,
        add: &Additional,
        properties: &[String],
        patterns: &[Regex],
        d: RawDoc<'_>,
        explaining: bool,
    ) -> Result<Option<Document>> {
        let mut extra = Vec::new();
        for (k, pv) in d.iter() {
            if properties.iter().any(|p| p == k) {
                continue;
            }
            let mut patterned = false;
            for re in patterns {
                patterned |= re.is_match(k)?;
            }
            if patterned {
                continue;
            }
            match add {
                Additional::Allowed(true) => {}
                Additional::Allowed(false) => extra.push(Value::from(k)),
                Additional::Schema(s) => {
                    if let Some((details, s)) = s.nested_details(pv, explaining)? {
                        // The first property that fails (measured).
                        let entry = Document::new()
                            .with(detail::OPERATOR_NAME, self.name)
                            .with(
                                detail::REASON,
                                "at least one additional property did not match the subschema",
                            )
                            .with(detail::FAILING_PROPERTY, k);
                        return Ok(Some(nested(s, details, entry)));
                    }
                }
            }
        }
        Ok((!extra.is_empty()).then(|| base(self).with(detail::ADDITIONAL_PROPERTIES, extra)))
    }

    fn dependencies(
        &self,
        deps: &[(String, Dependency)],
        d: RawDoc<'_>,
        explaining: bool,
    ) -> Result<Option<Document>> {
        let mut bad = Vec::new();
        for (prop, dep) in deps {
            if crate::matcher::field(d, prop).is_none() {
                continue;
            }
            let entry = Document::new().with(detail::CONDITIONAL_PROPERTY, prop.as_str());
            match dep {
                Dependency::Properties(names) => {
                    let missing: Vec<Value> = names
                        .iter()
                        .filter(|n| crate::matcher::field(d, n).is_none())
                        .map(|n| Value::from(n.as_str()))
                        .collect();
                    if !missing.is_empty() {
                        bad.push(Value::Document(entry.with(detail::MISSING_PROPERTIES, missing)));
                    }
                }
                Dependency::Schema(s) => {
                    if let Some((details, s)) =
                        s.nested_details(RawValue::Document(d), explaining)?
                    {
                        bad.push(Value::Document(nested(s, details, entry)));
                    }
                }
            }
        }
        Ok((!bad.is_empty()).then(|| base(self).with(detail::FAILING_DEPENDENCIES, bad)))
    }

    fn items(
        &self,
        items: &Items,
        elems: &[RawValue<'_>],
        explaining: bool,
    ) -> Result<Option<Document>> {
        let mut bad = Vec::new();
        let check = |s: &Schema, i: usize, e: RawValue<'_>, bad: &mut Vec<Value>| -> Result<()> {
            if let Some((details, s)) = s.nested_details(e, explaining)? {
                let i = i32::try_from(i).unwrap_or(i32::MAX);
                bad.push(Value::Document(nested(
                    s,
                    details,
                    Document::new().with(detail::ITEM_INDEX, i),
                )));
            }
            Ok(())
        };
        match items {
            // One schema for every item: the first item that fails, with RefDb's reason
            // (measured on a `refrouter` 9.0.4).
            Items::All(s) => {
                for (i, e) in elems.iter().enumerate() {
                    if let Some((details, s)) = s.nested_details(*e, explaining)? {
                        let entry = Document::new()
                            .with(detail::OPERATOR_NAME, self.name)
                            .with(detail::REASON, ITEMS_REASON)
                            .with(detail::ITEM_INDEX, i32::try_from(i).unwrap_or(i32::MAX));
                        return Ok(Some(nested(s, details, entry)));
                    }
                }
            }
            Items::Tuple { schemas, additional } => {
                for (i, e) in elems.iter().enumerate() {
                    match (schemas.get(i), additional) {
                        (Some(s), _) => check(s, i, *e, &mut bad)?,
                        (None, Some(Additional::Schema(s))) => check(s, i, *e, &mut bad)?,
                        (None, Some(Additional::Allowed(false))) => {}
                        (None, _) => {}
                    }
                }
            }
        }
        Ok((!bad.is_empty()).then(|| {
            Document::new().with(detail::OPERATOR_NAME, self.name).with(detail::DETAILS, bad)
        }))
    }

    fn combination(
        &self,
        list: &[Schema],
        v: RawValue<'_>,
        explaining: bool,
    ) -> Result<Option<Document>> {
        let mut failing = Vec::new();
        let mut matching = Vec::new();
        for (i, s) in list.iter().enumerate() {
            let index = i32::try_from(i).unwrap_or(i32::MAX);
            match s.nested_details(v, explaining)? {
                Some((details, s)) => failing.push(Value::Document(nested(
                    s,
                    details,
                    Document::new().with(detail::INDEX, index),
                ))),
                None => matching.push(Value::Int32(index)),
            }
        }
        // No `specifiedAs` for the logical keywords (measured).
        let name = Document::new().with(detail::OPERATOR_NAME, self.name);
        Ok(match self.kind {
            Kind::AllOf(_) if !failing.is_empty() => {
                Some(name.with(detail::SCHEMAS_NOT_SATISFIED, failing))
            }
            Kind::AnyOf(_) | Kind::OneOf(_) if matching.is_empty() => {
                Some(name.with(detail::SCHEMAS_NOT_SATISFIED, failing))
            }
            Kind::OneOf(_) if matching.len() > 1 => Some(
                name.with(detail::REASON, "more than one subschema matched")
                    .with(detail::MATCHING_SCHEMA_INDEXES, matching),
            ),
            _ => None,
        })
    }
}

fn type_fail(rule: &Rule, v: RawValue<'_>) -> Document {
    fail(rule, "type did not match", Some(v))
        .with(detail::CONSIDERED_TYPE, types::name_of(v.element_type()))
}

/// Whether `v` is a whole multiple of `m`.
fn multiple_of(v: RawValue<'_>, m: RawValue<'_>) -> bool {
    if let (Some(a), Some(b)) = (exact_i64(&v), exact_i64(&m)) {
        // `multipleOf` is positive: `b` is not zero.
        return a.wrapping_rem(b) == 0;
    }
    match (to_f64(&v), to_f64(&m)) {
        (Some(a), Some(b)) if a.is_finite() => (a / b).fract() == 0.0,
        _ => false,
    }
}
