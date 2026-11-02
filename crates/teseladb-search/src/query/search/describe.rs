//! The queries `refsearch` 1.70.6 builds for `equals`, `in`, `range` and `near`, as `scoreDetails`
//! writes them (measured 02-11-2026, `scratchpad/lane3l/m18.js` to `m20.js`). A number is asked
//! as a 64-bit integer (when it has one) and as a double, each also on its `Multiple` field; a
//! double is written as its sortable 64-bit encoding (`-bits(|x|)` below zero), a string token
//! as its bytes in hexadecimal, a date as its milliseconds.

use super::Score;
use super::explain::{Ctx, Desc, Leaf, sum};
use super::values::{Key, ValueOp, indexed, types_at};
use crate::definition::FieldMapping;
use crate::index::Explanation;

/// The lowest double bound `refsearch` writes for an open range.
const OPEN_LOW: i64 = -9_218_868_437_227_405_311;
/// The highest double bound `refsearch` writes for an open range (`Double.MAX_VALUE`).
const OPEN_HIGH: i64 = 9_218_868_437_227_405_311;

/// The 64-bit encoding `refsearch` gives a double.
pub(super) fn encoded(x: f64) -> i64 {
    let bits = |v: f64| v.to_bits() as i64;
    if x < 0.0 { -bits(-x) } else { bits(x) }
}

/// `x` cut to a 64-bit integer, as Java's `(long)` cast.
fn long(x: f64) -> i64 {
    x as i64
}

/// Bytes as RefLib's `BytesRef` writes them: `[61 62]`.
fn hex(bytes: &[u8]) -> String {
    let all: Vec<String> = bytes.iter().map(|b| format!("{b:x}")).collect();
    format!("[{}]", all.join(" "))
}

/// A term's bytes as RefLib's `Term` writes them: the text when it is UTF-8, else in hex.
fn term_text(bytes: &[u8]) -> String {
    std::str::from_utf8(bytes).map_or_else(|_| hex(bytes), str::to_owned)
}

/// The token a query string makes on the field at `path`, as its normalizer leaves it.
fn token(ctx: &Ctx<'_>, path: &str, text: &str) -> String {
    let lower = types_at(ctx.index.definition(), path)
        .into_iter()
        .any(|t| matches!(t, FieldMapping::Token { normalizer: Some(n) } if n == "lowercase"));
    if lower {
        text.chars().map(crate::analysis::unicode::lowercase).collect()
    } else {
        text.to_owned()
    }
}

/// A range of the 64-bit points `name`: `IndexOrDocValuesQuery(…) name-Multiple:[a TO b]`.
fn points(prefix: &str, kind: &str, path: &str, low: i64, high: i64) -> String {
    let one = format!("{prefix}$type:{kind}/{path}:[{low} TO {high}]");
    format!(
        "IndexOrDocValuesQuery(indexQuery={one}, dvQuery={one}) {prefix}$type:{kind}Multiple/{path}:[{low} TO {high}]"
    )
}

/// The number query of `range` or `equals` between the bounds, each with whether it holds.
fn number_range(
    ctx: &Ctx<'_>,
    path: &str,
    lower: Option<(f64, bool)>,
    upper: Option<(f64, bool)>,
    integers: bool,
) -> String {
    let p = &ctx.prefix;
    let int_low = lower.map_or(i64::MIN, |(a, incl)| {
        if incl { long(a.ceil()) } else { long(a.floor()).saturating_add(1) }
    });
    let int_high = upper.map_or(i64::MAX, |(b, incl)| {
        if incl { long(b.floor()) } else { long(b.ceil()).saturating_sub(1) }
    });
    let dbl_low = lower.map_or(OPEN_LOW, |(a, incl)| encoded(a) + i64::from(!incl));
    let dbl_high = upper.map_or(OPEN_HIGH, |(b, incl)| encoded(b) - i64::from(!incl));
    let doubles = points(p, "double", path, dbl_low, dbl_high);
    if integers && int_low <= int_high {
        format!("{} {doubles}", points(p, "int64", path, int_low, int_high))
    } else {
        doubles
    }
}

/// The constant-score form of `equals`, `in` and a `range` of one path; `None` for the others.
pub(super) fn values_leaf(ctx: &Ctx<'_>, op: &ValueOp) -> Option<Leaf> {
    match op {
        ValueOp::AnyOf { paths, values, set: false, .. } => {
            let (path, value) = (paths.first()?, values.first()?);
            Some(equals(ctx, path, value))
        }
        ValueOp::AnyOf { paths, values, .. } => Some(in_set(ctx, paths, values)),
        ValueOp::Range { paths, lower, upper, .. } if paths.len() == 1 => {
            Some(range(ctx, &paths[0], lower.as_ref(), upper.as_ref()))
        }
        _ => None,
    }
}

fn equals(ctx: &Ctx<'_>, path: &str, value: &Key) -> Leaf {
    let p = &ctx.prefix;
    match value {
        Key::Bool(b) => {
            Leaf::Term(format!("{p}$type:boolean/{path}:{}", if *b { 'T' } else { 'F' }))
        }
        Key::ObjectId(o) => Leaf::Term(format!("{p}$type:objectId/{path}:{}", term_text(o))),
        Key::Uuid(u) => Leaf::Term(format!("{p}$type:uuid/{path}:{}", term_text(u))),
        Key::Null => Leaf::Term(format!("{p}$type:null/{path}:N")),
        Key::Number(x) => {
            let integral = x.fract() == 0.0 && x.abs() < 9.2e18;
            Leaf::Wrapped(number_range(ctx, path, Some((*x, true)), Some((*x, true)), integral))
        }
        Key::Date(ms) => Leaf::Wrapped(points(p, "date", path, *ms, *ms)),
        Key::Text(s) | Key::Token(s, _) => {
            let h = hex(token(ctx, path, s).as_bytes());
            Leaf::Bare(format!("{p}$type:token/{path}:[{h} TO {h}]"))
        }
    }
}

fn in_set(ctx: &Ctx<'_>, paths: &[String], values: &[Key]) -> Leaf {
    let p = &ctx.prefix;
    let mut parts: Vec<String> = Vec::new();
    let mut single_term = true;
    for path in paths {
        let set = |mut v: Vec<i64>| {
            v.sort_unstable();
            v.dedup();
            v.iter().map(i64::to_string).collect::<Vec<_>>().join(" ")
        };
        let numbers: Vec<f64> = values
            .iter()
            .filter_map(|v| if let Key::Number(x) = v { Some(*x) } else { None })
            .collect();
        if !numbers.is_empty() {
            single_term = false;
            let ints = set(numbers.iter().map(|&x| long(x)).collect());
            let dbls = set(numbers.iter().map(|&x| encoded(x)).collect());
            parts.push(format!("{p}$type:int64/{path}:{{{ints}}}"));
            parts.push(format!("{p}$type:int64Multiple/{path}:{{{ints}}}"));
            parts.push(format!("{p}$type:double/{path}:{{{dbls}}}"));
            parts.push(format!("{p}$type:doubleMultiple/{path}:{{{dbls}}}"));
        }
        let dates: Vec<i64> = values
            .iter()
            .filter_map(|v| if let Key::Date(d) = v { Some(*d) } else { None })
            .collect();
        if !dates.is_empty() {
            single_term = false;
            let all = set(dates);
            parts.push(format!("{p}$type:date/{path}:{{{all}}}"));
            parts.push(format!("{p}$type:dateMultiple/{path}:{{{all}}}"));
        }
        let mut tokens: Vec<String> = values
            .iter()
            .filter_map(|v| match v {
                Key::Text(s) | Key::Token(s, _) => Some(token(ctx, path, s)),
                _ => None,
            })
            .collect();
        if !tokens.is_empty() {
            single_term = false;
            tokens.sort();
            tokens.dedup();
            parts.push(format!("{p}$type:token/{path}:({})", tokens.join(" ")));
        }
        for v in values {
            let term = match v {
                Key::Bool(b) => format!("{p}$type:boolean/{path}:{}", if *b { 'T' } else { 'F' }),
                Key::ObjectId(o) => format!("{p}$type:objectId/{path}:{}", term_text(o)),
                Key::Uuid(u) => format!("{p}$type:uuid/{path}:{}", term_text(u)),
                _ => continue,
            };
            if !parts.contains(&term) {
                parts.push(term);
            }
        }
    }
    match parts.as_slice() {
        [one] if single_term => Leaf::Term(one.clone()),
        _ => Leaf::Wrapped(parts.join(" ")),
    }
}

fn range(
    ctx: &Ctx<'_>,
    path: &str,
    lower: Option<&(Key, bool)>,
    upper: Option<&(Key, bool)>,
) -> Leaf {
    let p = &ctx.prefix;
    let kind = lower.or(upper).map(|(k, _)| k);
    match kind {
        Some(Key::Date(_)) => {
            let ms = |b: Option<&(Key, bool)>, open: i64, step: i64| match b {
                Some((Key::Date(d), incl)) => {
                    if *incl {
                        *d
                    } else {
                        d.saturating_add(step)
                    }
                }
                _ => open,
            };
            Leaf::Wrapped(points(p, "date", path, ms(lower, i64::MIN, 1), ms(upper, i64::MAX, -1)))
        }
        Some(Key::Text(_) | Key::Token(..)) => {
            let bound = |b: Option<&(Key, bool)>, open: char, closed: char| match b {
                Some((Key::Text(s) | Key::Token(s, _), incl)) => {
                    (hex(token(ctx, path, s).as_bytes()), if *incl { closed } else { open })
                }
                _ => ("*".to_owned(), open),
            };
            let (low, l) = bound(lower, '{', '[');
            let (high, h) = bound(upper, '}', ']');
            Leaf::Bare(format!("{p}$type:token/{path}:{l}{low} TO {high}{h}"))
        }
        _ => {
            let num = |b: Option<&(Key, bool)>| match b {
                Some((Key::Number(x), incl)) => Some((*x, *incl)),
                _ => None,
            };
            Leaf::Wrapped(number_range(ctx, path, num(lower), num(upper), true))
        }
    }
}

/// The description of `near` over numbers, as a constant score writes it.
pub(super) fn near_description(ctx: &Ctx<'_>, op: &ValueOp) -> Option<Desc> {
    let ValueOp::Near { paths, origin: Key::Number(o), pivot, .. } = op else { return None };
    let mut parts = Vec::new();
    for path in paths {
        let name = format!("{}$type:double/{path}", ctx.prefix);
        parts.push(format!(
            "LongDistanceFeatureQuery(field=,origin={},pivotDistance={}) {name}:[{} TO {}] {name}:[{} TO {}]",
            long(*o),
            long(*pivot),
            i64::MIN,
            encoded(*o),
            encoded(*o) + 1,
            i64::MAX
        ));
    }
    Some(Desc::Bool(parts.join(" ")))
}

/// The explanation of a `range` on several paths or of `near` on numbers or dates for `doc`.
pub(super) fn near(
    ctx: &Ctx<'_>,
    op: &ValueOp,
    boost: f32,
    doc: u32,
    score: f32,
) -> Option<Explanation> {
    let def = ctx.index.definition();
    let d = ctx.index.document(doc)?;
    match op {
        ValueOp::Range { paths, lower, upper, score: how } => {
            let parts: Vec<Explanation> = paths
                .iter()
                .filter(|p| {
                    let one = ValueOp::Range {
                        paths: vec![(*p).clone()],
                        lower: lower.clone(),
                        upper: upper.clone(),
                        score: Score::Plain,
                    };
                    one.doc_score(def, d, 1.0).is_some()
                })
                .map(|p| range(ctx, p, lower.as_ref(), upper.as_ref()).explain(how, boost))
                .collect();
            let mut e = sum(parts);
            e.value = score;
            Some(e)
        }
        ValueOp::Near { paths, origin, pivot, .. } => {
            let mut parts = Vec::new();
            for path in paths {
                let value = indexed(def, d, path)
                    .into_iter()
                    .find_map(|(k, in_array)| (!in_array).then_some(k));
                let part = match (value, origin) {
                    (Some(Key::Number(v)), Key::Number(o)) => {
                        number_distance(ctx, path, v, *o, *pivot, boost)
                    }
                    (Some(Key::Date(v)), Key::Date(o)) => {
                        date_distance(v as f64, *o as f64, *pivot, boost)
                    }
                    _ => continue,
                };
                parts.push(part);
            }
            match parts.len() {
                0 => None,
                1 if paths.len() == 1 => parts.pop(),
                _ => {
                    let mut e = sum(parts);
                    e.value = score;
                    Some(e)
                }
            }
        }
        ValueOp::AnyOf { .. } => None,
    }
}

/// `near` on a number (measured: three nested sums, two with a boost, around the half of the
/// line the value is on and the distance feature).
fn number_distance(
    ctx: &Ctx<'_>,
    path: &str,
    v: f64,
    o: f64,
    pivot: f64,
    boost: f32,
) -> Explanation {
    let name = format!("{}$type:double/{path}", ctx.prefix);
    let half = if v <= o {
        format!("{name}:[{} TO {}]", i64::MIN, encoded(o))
    } else {
        format!("{name}:[{} TO {}]", encoded(o) + 1, i64::MAX)
    };
    let feature = (pivot / (pivot + (o - v).abs())) as f32;
    let value = (f64::from(boost) * (pivot / (pivot + (o - v).abs()))) as f32;
    let expr = Explanation::node(
        feature,
        "pivot / (pivot + abs(origin - value)), computed from:",
        vec![
            Explanation::leaf(
                pivot as f32,
                format!("constant({})", crate::index::java_double(pivot)),
            ),
            Explanation::leaf(o as f32, format!("constant({})", crate::index::java_double(o))),
            Explanation::leaf(v as f32, format!("double({name})")),
        ],
    );
    let query = format!(
        "weight(FunctionScoreQuery(LongDistanceFeatureQuery(field=,origin={},pivotDistance={}), scored by expr(pivot / (pivot + abs(origin - value))))),",
        encoded(o),
        encoded(pivot)
    );
    let function = if boost == 1.0 {
        Explanation::node(value, format!("{query} result of:"), vec![expr])
    } else {
        Explanation::node(
            value,
            format!("{query} product of:"),
            vec![Explanation::leaf(boost, "boost"), expr],
        )
    };
    let required = Explanation::node(
        0.0,
        "match on required clause, product of:",
        vec![Explanation::leaf(0.0, "# clause"), Explanation::leaf(1.0, half)],
    );
    let mut e = Explanation::node(value, "sum of:", vec![required, function]);
    let levels = if boost == 1.0 { 2 } else { 1 };
    for _ in 0..levels {
        e = Explanation::node(value, "sum of:", vec![e]);
    }
    e
}

/// `near` on a date: RefLib's distance feature of 64-bit points.
fn date_distance(v: f64, o: f64, pivot: f64, boost: f32) -> Explanation {
    let value = (f64::from(boost) * (pivot / (pivot + (v - o).abs()))) as f32;
    Explanation::node(
        value,
        "Distance score, computed as weight * pivotDistance / (pivotDistance + abs(value - origin)) from:",
        vec![
            Explanation::leaf(boost, "weight"),
            Explanation::leaf(pivot as f32, "pivotDistance"),
            Explanation::leaf(o as f32, "origin"),
            Explanation::leaf(v as f32, "current value"),
        ],
    )
}
