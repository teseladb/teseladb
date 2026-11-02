//! The `sort` option of `$search` (011 FR-003) as `refsearch` 1.70.6 runs it (measured 02-11-2026
//! on a 9.0.4 replica set, `scratchpad/lane3k/m14.js`): fields indexed with a type that keeps
//! values (`number`, `token`, `date`, `boolean`, `objectId`, `uuid`, and `_id`) or the score;
//! an array sorts by its least value going up and its greatest going down, a document without a
//! value (or with an empty array) is the lowest unless `noData: "highest"`, values of different
//! types follow RefDb's order, and equal documents keep the order of their keys.

use std::cmp::Ordering;

use teseladb_bson::{Document, Value};

use super::search::SearchError;
use super::search::values::{Key, indexed};
use crate::definition::Definition;

/// What a sort field compares.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SortBy {
    /// `{$meta: "searchScore"}`.
    Score,
    /// The values of a path.
    Path(String),
}

/// One field of a `sort`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SortField {
    pub(crate) by: SortBy,
    pub(crate) descending: bool,
    /// Whether a document without a value sorts above every value.
    pub(crate) no_data_highest: bool,
}

/// The value of a document for one sort field.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SortValue {
    Missing,
    Score(f32),
    Key(Key),
}

fn shape(path: &str, what: &str) -> SearchError {
    SearchError(format!("\"{path}\" {what}"))
}

/// Whether `v` is the number 1 or -1, as a direction; `None` for another number, `Err` for
/// something else.
fn order(v: &Value) -> Option<Option<bool>> {
    let n = match v {
        Value::Int32(n) => f64::from(*n),
        Value::Int64(n) => *n as f64,
        Value::Double(d) => *d,
        _ => return None,
    };
    Some(if n == 1.0 {
        Some(false)
    } else if n == -1.0 {
        Some(true)
    } else {
        None
    })
}

/// The `sort` of a `$search`, if it has one.
pub(crate) fn parse(spec: &Document) -> Result<Option<Vec<SortField>>, SearchError> {
    let Some(v) = spec.get("sort") else { return Ok(None) };
    let Value::Document(d) = v else { return Err(shape("sort", "must be a document")) };
    if d.is_empty() {
        return Err(shape("sort", "Sort spec must have at least one sort field defined"));
    }
    let mut fields = Vec::new();
    for (name, v) in d.iter() {
        let field = match v {
            Value::Document(o) => {
                let descending = match o.get("order") {
                    None => o.get("$meta").is_some(),
                    Some(v) => match order(v) {
                        Some(Some(desc)) => desc,
                        _ => return Err(shape("sort.order", "sort order must be 1 or -1")),
                    },
                };
                let no_data_highest = match o.get("noData") {
                    None => false,
                    Some(Value::String(s)) if s == "lowest" => false,
                    Some(Value::String(s)) if s == "highest" => true,
                    Some(_) => {
                        return Err(shape("sort.noData", "must be one of [highest, lowest]"));
                    }
                };
                let by = match o.get("$meta") {
                    Some(Value::String(m)) if m == "searchScore" => SortBy::Score,
                    Some(_) => return Err(shape("sort.$meta", "must be one of [searchScore]")),
                    None => SortBy::Path(name.to_owned()),
                };
                SortField { by, descending, no_data_highest }
            }
            other => match order(other) {
                Some(Some(descending)) => SortField {
                    by: SortBy::Path(name.to_owned()),
                    descending,
                    no_data_highest: false,
                },
                Some(None) => return Err(shape("sort", "sort order must be 1 or -1")),
                None => return Err(shape("sort", "must be a Expected number or document")),
            },
        };
        fields.push(field);
    }
    Ok(Some(fields))
}

/// The rank of a value's type in RefDb's order.
fn rank(k: &Key) -> u8 {
    match k {
        Key::Null => 0,
        Key::Number(_) => 1,
        Key::Token(..) | Key::Text(_) => 2,
        Key::Uuid(_) => 3,
        Key::ObjectId(_) => 4,
        Key::Bool(_) => 5,
        Key::Date(_) => 6,
    }
}

/// How two values compare, across types in RefDb's order.
pub(crate) fn compare_keys(a: &Key, b: &Key) -> Ordering {
    match (a, b) {
        (Key::Number(x), Key::Number(y)) => x.total_cmp(y),
        (Key::Date(x), Key::Date(y)) => x.cmp(y),
        (Key::Bool(x), Key::Bool(y)) => x.cmp(y),
        (Key::ObjectId(x), Key::ObjectId(y)) => x.cmp(y),
        (Key::Uuid(x), Key::Uuid(y)) => x.cmp(y),
        (Key::Token(x, _) | Key::Text(x), Key::Token(y, _) | Key::Text(y)) => {
            x.as_bytes().cmp(y.as_bytes())
        }
        _ => rank(a).cmp(&rank(b)),
    }
}

/// The value of `doc` (scored `score`) for `field`: the least of its values going up, the
/// greatest going down.
pub(crate) fn value_of(
    definition: &Definition,
    doc: &Document,
    score: f32,
    field: &SortField,
) -> SortValue {
    let SortBy::Path(path) = &field.by else { return SortValue::Score(score) };
    let mut keys: Vec<Key> = indexed(definition, doc, path)
        .into_iter()
        .map(|(k, _)| k)
        .filter(|k| *k != Key::Null)
        .collect();
    if keys.is_empty() && path == "_id" {
        keys.extend(doc.get("_id").and_then(Key::of_query));
    }
    let pick = if field.descending { Iterator::max_by } else { Iterator::min_by };
    pick(keys.into_iter(), compare_keys).map_or(SortValue::Missing, SortValue::Key)
}

/// How two values of `field` compare in the order of the results.
pub(crate) fn compare(a: &SortValue, b: &SortValue, field: &SortField) -> Ordering {
    let natural = match (a, b) {
        (SortValue::Missing, SortValue::Missing) => Ordering::Equal,
        (SortValue::Missing, _) if field.no_data_highest => Ordering::Greater,
        (SortValue::Missing, _) => Ordering::Less,
        (_, SortValue::Missing) if field.no_data_highest => Ordering::Less,
        (_, SortValue::Missing) => Ordering::Greater,
        (SortValue::Score(x), SortValue::Score(y)) => x.total_cmp(y),
        (SortValue::Key(x), SortValue::Key(y)) => compare_keys(x, y),
        (SortValue::Score(_), SortValue::Key(_)) => Ordering::Less,
        (SortValue::Key(_), SortValue::Score(_)) => Ordering::Greater,
    };
    if field.descending { natural.reverse() } else { natural }
}

/// The sort of a `$search` without `sort`: by score, best first.
pub(crate) fn by_score() -> Vec<SortField> {
    vec![SortField { by: SortBy::Score, descending: true, no_data_highest: false }]
}

/// `fields` the other way round, as `searchBefore` reads the results.
pub(crate) fn reversed(fields: &[SortField]) -> Vec<SortField> {
    fields.iter().map(|f| SortField { descending: !f.descending, ..f.clone() }).collect()
}

/// How two documents compare: their values for `fields`, then their keys.
pub(crate) fn compare_all(
    a: (&[SortValue], &[u8]),
    b: (&[SortValue], &[u8]),
    fields: &[SortField],
) -> Ordering {
    let by_values = fields.iter().enumerate().fold(Ordering::Equal, |acc, (i, f)| {
        acc.then_with(|| match (a.0.get(i), b.0.get(i)) {
            (Some(x), Some(y)) => compare(x, y, f),
            (x, y) => x.is_some().cmp(&y.is_some()),
        })
    });
    by_values.then_with(|| a.1.cmp(b.1))
}
