//! The plan cache hashes of a read as a `refnode` 9.0.4 shows them (010 FR-002, H-028):
//! `planCacheShapeHash` (and `queryHash`, its old name) and `planCacheKey`.
//!
//! The shape is the text a `refnode` builds for its classic plan cache: each node of the parsed
//! filter as a two-letter code and its path, its children between brackets, then the sections of
//! the sort, the projection's required fields, the collation and the key of a `distinct`, each
//! after a `|`, and three flags (`apiStrict`, the deferred engine choice, the regular path). The
//! key adds, for each predicate on a path some index holds, one digit per index saying whether
//! the index can answer it. Both are hashed with MurmurHash3 x64-128 and seed 0, keeping the low
//! 32 bits of the first word; the key also mixes the hash of the query settings (none) as
//! `boost::hash_combine` does. Measured against 290 hashes of a `refnode` 9.0.4
//! (`tests/explain/query-hash-vectors.jsonl`).

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};
use teseladb_collation::{Alternate, CaseFirst, CollationSpec, MaxVariable};

/// What a read's shape is made of.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ShapeInput<'a> {
    /// The filter as `explain` shows it (`parsedQuery`).
    pub parsed: Option<&'a Document>,
    /// The filter as the command gave it: a `$expr` takes its path from it.
    pub original: Option<&'a Document>,
    /// The sort.
    pub sort: Option<&'a Document>,
    /// The projection.
    pub projection: Option<&'a Document>,
    /// The read's collation, unless it is the simple one.
    pub collation: Option<&'a CollationSpec>,
    /// The key of a `distinct` whose plan may scan an index for it.
    pub distinct: Option<&'a str>,
    /// `apiStrict`.
    pub api_strict: bool,
}

/// The characters a field name escapes with `\` in a shape.
const SPECIAL: &[u8] = b"[],|-/?:\\";
/// The regular expression flags a shape keeps.
const REGEX_FLAGS: &[u8] = b"imsux";

/// The shape text of a read.
pub(crate) fn shape_key(input: &ShapeInput<'_>) -> String {
    let empty = Document::new();
    let mut exprs = expr_paths(input.original.or(input.parsed).unwrap_or(&empty));
    exprs.reverse();
    let mut out = String::new();
    filter(input.parsed.unwrap_or(&empty), &mut exprs, &mut out);
    out.push('|');
    sort(input.sort, &mut out);
    out.push('|');
    projection(input.projection, &mut out);
    out.push('|');
    if let Some(c) = input.collation {
        collation(c, &mut out);
    }
    out.push('|');
    if let Some(k) = input.distinct {
        user(k, &mut out);
        out.push('0');
    }
    out.push(if input.api_strict { 't' } else { 'f' });
    out.push_str("tr");
    out
}

/// `planCacheShapeHash` of a shape text.
pub(crate) fn shape_hash(key: &str) -> String {
    format!("{:08X}", hash32(key))
}

/// `planCacheKey` of a shape text and the indexability digits of its predicates.
pub(crate) fn cache_key_hash(key: &str, indexability: &str) -> String {
    let full = format!("{key}{indexability}");
    let mixed = hash_combine(u64::from(hash32(&full)), settings_hash());
    format!("{:08X}", mixed & u64::from(u32::MAX))
}

/// An index as the plan cache key sees it.
#[derive(Clone, Debug)]
pub(crate) struct KeyIndex {
    /// The fields of its key pattern.
    pub fields: Vec<String>,
    /// `sparse`.
    pub sparse: bool,
    /// Its collation, unless simple.
    pub collation: Option<CollationSpec>,
    /// Hidden: it counts for the plan cache key and answers no `distinct`.
    pub hidden: bool,
}

/// The indexability digits of filter `parsed` over `indexes` for a read with collation
/// `collation`: for each predicate on a path some index holds, `<` one digit per such index `>`,
/// and for each `$not`, whether an index could answer it.
pub(crate) fn indexability(
    parsed: &Document,
    indexes: &[KeyIndex],
    collation: Option<&CollationSpec>,
) -> String {
    let mut out = String::new();
    Indexability { indexes, collation }.doc(parsed, "", &mut out);
    out
}

/// The key of a `distinct` as its shape keeps it: none when the filter is a `$or`, or empty and
/// no index holds the key (measured on a `refnode` 9.0.4: those reads plan without the key).
pub(crate) fn distinct_key<'a>(
    parsed: &Document,
    key: Option<&'a str>,
    indexes: &[KeyIndex],
) -> Option<&'a str> {
    let key = key?;
    if parsed.get("$or").is_some() {
        return None;
    }
    // A hidden index answers no `distinct` (measured: with only a hidden index on the key, an
    // empty filter keeps no key).
    let indexed = indexes.iter().any(|i| !i.hidden && i.fields.iter().any(|f| f == key));
    (indexed || !parsed.is_empty()).then_some(key)
}

/// The key of a `$group` stage by one field that an index starting with that field may answer
/// as a `distinct` (measured on a `refrouter` 9.0.4: with an index on `g`, `$group: {_id: "$g"}`
/// over a `$match` adds the key `g` to the shape; without it, nothing).
pub(crate) fn group_key<'a>(stage: Option<&'a Document>, indexes: &[KeyIndex]) -> Option<&'a str> {
    let Some(Value::Document(group)) = stage?.get("$group") else { return None };
    let Some(Value::String(path)) = group.get("_id") else { return None };
    let field = path.strip_prefix('$').filter(|f| !f.starts_with('$'))?;
    indexes
        .iter()
        .any(|i| !i.hidden && i.fields.first().is_some_and(|f| f == field))
        .then_some(field)
}

/// The fields a pipeline's query reads as its shape counts them, from the paths its stages use
/// (`fields`) and its first stage: a `$project` that computes `_id` does not read `_id`, and one
/// that computes a field inside a subdocument reads that subdocument too (measured on a `refrouter`
/// 9.0.4: `{o: {k: 1, z: "$n"}}` reads `_id`, `n`, `o` and `o.k`).
pub(crate) fn shaped_fields(mut fields: Vec<String>, first: Option<&Document>) -> Vec<String> {
    let Some(Value::Document(spec)) = first.and_then(|st| st.get("$project")) else {
        return fields;
    };
    let computed = |v: &Value| !matches!(v, Value::Boolean(_)) && number(v).is_none();
    if spec.get("_id").is_some_and(computed)
        && !dependencies(spec.get("_id").unwrap_or(&Value::Null)).is_some_and(|d| d.contains("_id"))
    {
        fields.retain(|f| f != "_id");
    }
    for (k, v) in spec.iter() {
        let Value::Document(sub) = v else { continue };
        if sub.iter().any(|(sk, _)| sk.starts_with('$')) || !sub.iter().any(|(_, x)| computed(x)) {
            continue;
        }
        if !fields.iter().any(|f| f == k) {
            fields.push(k.to_owned());
        }
        for (sk, x) in sub.iter().filter(|(_, x)| !computed(x)) {
            let path = format!("{k}.{sk}");
            let included = number(x).is_some_and(|n| n != 0.0) || matches!(x, Value::Boolean(true));
            if included && !fields.contains(&path) {
                fields.push(path);
            }
        }
    }
    fields
}

/// The three hashes of a read (`queryHash`, `planCacheShapeHash`, `planCacheKey`) over the
/// indexes of its collection.
pub(crate) fn hashes(input: &ShapeInput<'_>, indexes: &[KeyIndex]) -> [(&'static str, String); 3] {
    let key = shape_key(input);
    let empty = Document::new();
    let digits = indexability(input.parsed.unwrap_or(&empty), indexes, input.collation);
    let shape = shape_hash(&key);
    [
        ("queryHash", shape.clone()),
        ("planCacheShapeHash", shape),
        ("planCacheKey", cache_key_hash(&key, &digits)),
    ]
}

/// MurmurHash3 x64-128 of `key` with seed 0: the low 32 bits of its first word.
fn hash32(key: &str) -> u32 {
    let h = murmur3_x64_128(key.as_bytes(), 0) & u64::from(u32::MAX);
    u32::try_from(h).unwrap_or_default()
}

/// The hash of empty query settings: `boost::hash_combine` of an empty framework, empty index
/// hints, `reject: false` and empty knobs (an empty optional hashes to `0x12345678`).
fn settings_hash() -> u64 {
    const EMPTY_OPTIONAL: u64 = 0x1234_5678;
    [EMPTY_OPTIONAL, EMPTY_OPTIONAL, 0, EMPTY_OPTIONAL].iter().fold(0, |h, v| hash_combine(h, *v))
}

/// `boost::hash_combine` of Boost 1.88 on 64 bits.
fn hash_combine(seed: u64, value: u64) -> u64 {
    const M: u64 = 0x0e98_46af_9b1a_615d;
    let mut x = seed.wrapping_add(0x9e37_79b9).wrapping_add(value);
    x ^= x >> 32;
    x = x.wrapping_mul(M);
    x ^= x >> 32;
    x = x.wrapping_mul(M);
    x ^ (x >> 28)
}

/// The first 64-bit word of MurmurHash3 x64-128 (Austin Appleby's public-domain algorithm).
fn murmur3_x64_128(data: &[u8], seed: u64) -> u64 {
    const C1: u64 = 0x87c3_7b91_1142_53d5;
    const C2: u64 = 0x4cf5_ad43_2745_937f;
    let mix1 = |k: u64| k.wrapping_mul(C1).rotate_left(31).wrapping_mul(C2);
    let mix2 = |k: u64| k.wrapping_mul(C2).rotate_left(33).wrapping_mul(C1);
    let word = |b: &[u8]| {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        u64::from_le_bytes(w)
    };
    let (mut h1, mut h2) = (seed, seed);
    let (blocks, tail) = data.as_chunks::<16>();
    for b in blocks {
        h1 ^= mix1(word(&b[..8]));
        h1 = h1.rotate_left(27).wrapping_add(h2).wrapping_mul(5).wrapping_add(0x52dc_e729);
        h2 ^= mix2(word(&b[8..]));
        h2 = h2.rotate_left(31).wrapping_add(h1).wrapping_mul(5).wrapping_add(0x3849_5ab5);
    }
    if tail.len() > 8 {
        h2 ^= mix2(word(&tail[8..]));
    }
    if !tail.is_empty() {
        h1 ^= mix1(word(&tail[..tail.len().min(8)]));
    }
    let len = data.len() as u64;
    h1 ^= len;
    h2 ^= len;
    h1 = h1.wrapping_add(h2);
    h2 = h2.wrapping_add(h1);
    fmix(h1).wrapping_add(fmix(h2))
}

/// MurmurHash3's final mix.
fn fmix(mut k: u64) -> u64 {
    k ^= k >> 33;
    k = k.wrapping_mul(0xff51_afd7_ed55_8ccd);
    k ^= k >> 33;
    k = k.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    k ^ (k >> 33)
}

/// A field name or flag, its special characters escaped.
fn user(s: &str, out: &mut String) {
    for c in s.chars() {
        if u8::try_from(c).is_ok_and(|b| SPECIAL.contains(&b)) {
            out.push('\\');
        }
        out.push(c);
    }
}

/// The code of an operator of a field.
fn op_code(op: &str) -> &str {
    match op {
        "$lt" => "lt",
        "$lte" => "le",
        "$gt" => "gt",
        "$gte" => "ge",
        "$in" => "in",
        "$regex" => "re",
        "$exists" => "ex",
        "$type" => "ty",
        "$size" => "sz",
        "$mod" => "mo",
        "$bitsAllSet" => "ls",
        "$bitsAllClear" => "lc",
        "$bitsAnySet" => "ys",
        "$bitsAnyClear" => "yc",
        "$geoWithin" | "$geoIntersects" => "go",
        "$near" | "$nearSphere" => "gn",
        "$_internalExprEq" => "eeq",
        "$_internalExprGt" => "egt",
        "$_internalExprGte" => "ege",
        "$_internalExprLt" => "elt",
        "$_internalExprLte" => "ele",
        "$_internalEqHash" => "ieh",
        // The internal operators of `$jsonSchema` keep their name without `$_`.
        _ => op.strip_prefix("$_").filter(|o| o.starts_with("internalSchema")).unwrap_or("eq"),
    }
}

/// Nodes as one node, or as the `$and` of them.
fn all_of(nodes: &[String], out: &mut String) {
    match nodes {
        [] => out.push_str("an"),
        [one] => out.push_str(one),
        many => {
            out.push_str("an[");
            out.push_str(&many.join(","));
            out.push(']');
        }
    }
}

/// A filter document: one node, or an implicit `$and` of several.
fn filter(d: &Document, exprs: &mut Vec<String>, out: &mut String) {
    let nodes: Vec<String> = d
        .iter()
        .map(|(k, v)| {
            let mut s = String::new();
            top(k, v, exprs, &mut s);
            s
        })
        .collect();
    all_of(&nodes, out);
}

/// The clauses of a logical operator, between brackets.
fn clauses(items: &[Value], exprs: &mut Vec<String>, out: &mut String) {
    let parts: Vec<String> = items
        .iter()
        .map(|c| {
            let mut s = String::new();
            if let Value::Document(d) = c {
                filter(d, exprs, &mut s);
            }
            s
        })
        .collect();
    out.push('[');
    out.push_str(&parts.join(","));
    out.push(']');
}

/// One top-level entry of a filter.
fn top(k: &str, v: &Value, exprs: &mut Vec<String>, out: &mut String) {
    match (k, v) {
        // A `$nor` of one clause is the `$not` of it.
        ("$nor", Value::Array(items)) if items.len() == 1 => {
            out.push_str("nt");
            if let Some(Value::Document(d)) = items.first()
                && negates_null(d)
            {
                out.push_str("not_eq_null");
            }
            clauses(items, exprs, out);
        }
        ("$and" | "$or" | "$nor", Value::Array(items)) => {
            out.push_str(match k {
                "$and" => "an",
                "$or" => "or",
                _ => "nr",
            });
            clauses(items, exprs, out);
        }
        ("$alwaysTrue", _) => out.push_str("at"),
        ("$alwaysFalse", _) => out.push_str("af"),
        ("$where", _) => out.push_str("wh"),
        ("$text", _) => out.push_str("te"),
        ("$expr", _) => {
            out.push_str("xp");
            user(&exprs.pop().unwrap_or_default(), out);
        }
        _ => field(k, v, out),
    }
}

/// Whether operators `ops` are one equality (or `$gte`, `$lte`) to null.
fn equals_null(ops: &Document) -> bool {
    let mut o = ops.iter();
    matches!((o.next(), o.next()), (Some(("$eq" | "$gte" | "$lte", Value::Null)), None))
}

/// Whether filter `d` is an equality (or `$gte`, `$lte`) to null of one field.
fn negates_null(d: &Document) -> bool {
    let mut it = d.iter();
    match (it.next(), it.next()) {
        (Some((k, Value::Document(ops))), None) => !k.starts_with('$') && equals_null(ops),
        _ => false,
    }
}

/// The operators of a field's condition, if it is written with operators.
fn operators(v: &Value) -> Option<&Document> {
    match v {
        Value::Document(d) if !d.is_empty() && d.iter().all(|(k, _)| k.starts_with('$')) => Some(d),
        _ => None,
    }
}

/// The predicates of field `path`.
fn field(path: &str, v: &Value, out: &mut String) {
    let Some(ops) = operators(v) else {
        let op = if matches!(v, Value::Regex { .. }) { "$regex" } else { "$eq" };
        return leaf(op, path, v, None, out);
    };
    let options = match ops.get("$options") {
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    };
    let nodes: Vec<String> = ops
        .iter()
        .filter(|(k, _)| *k != "$options")
        .map(|(k, x)| {
            let mut s = String::new();
            operator(path, k, x, options, &mut s);
            s
        })
        .collect();
    all_of(&nodes, out);
}

/// One operator `op` of field `path`.
fn operator(path: &str, op: &str, x: &Value, options: Option<&str>, out: &mut String) {
    match (op, x) {
        ("$not", _) => {
            out.push_str("nt");
            if matches!(x, Value::Document(inner) if equals_null(inner)) {
                out.push_str("not_eq_null");
            }
            out.push('[');
            field(path, x, out);
            out.push(']');
        }
        ("$elemMatch", Value::Document(inner)) => elem_match(path, inner, options, out),
        _ => leaf(op, path, x, options, out),
    }
}

/// `$elemMatch` of field `path`: of values when `inner` has only operators, else of objects.
fn elem_match(path: &str, inner: &Document, options: Option<&str>, out: &mut String) {
    let of_values = inner.iter().all(|(k, _)| {
        k.starts_with('$') && !matches!(k, "$and" | "$or" | "$nor" | "$expr" | "$where")
    });
    if !of_values {
        out.push_str("eo");
        user(path, out);
        out.push('[');
        filter(inner, &mut Vec::new(), out);
        out.push(']');
        return;
    }
    out.push_str("ev");
    user(path, out);
    let parts: Vec<String> = inner
        .iter()
        .filter(|(k, _)| *k != "$options")
        .map(|(k, y)| {
            let mut s = String::new();
            operator("", k, y, options, &mut s);
            s
        })
        .collect();
    out.push('[');
    out.push_str(&parts.join(","));
    out.push(']');
}

/// A predicate `op` of field `path` on `x`.
fn leaf(op: &str, path: &str, x: &Value, options: Option<&str>, out: &mut String) {
    out.push_str(op_code(op));
    user(path, out);
    match (op, x) {
        ("$regex", Value::Regex { options: o, .. }) => flags(o, out),
        ("$regex", _) => flags(options.unwrap_or_default(), out),
        ("$in", Value::Array(items)) => {
            let regexes: Vec<&str> = items
                .iter()
                .filter_map(|i| match i {
                    Value::Regex { options, .. } => Some(options.as_str()),
                    _ => None,
                })
                .collect();
            if !regexes.is_empty() {
                user("_re", out);
                flags(&regexes.concat(), out);
            }
        }
        ("$gt" | "$gte", Value::MinKey) => out.push_str("min"),
        ("$lt" | "$lte", Value::MaxKey) => out.push_str("max"),
        _ => {}
    }
}

/// The valid flags of regular expressions, sorted and once each, between `/`.
fn flags(options: &str, out: &mut String) {
    let set: BTreeSet<char> = options
        .chars()
        .filter(|c| u8::try_from(*c).is_ok_and(|b| REGEX_FLAGS.contains(&b)))
        .collect();
    if set.is_empty() {
        return;
    }
    out.push('/');
    for f in set {
        user(&f.to_string(), out);
    }
    out.push('/');
}

/// The sort section: `a`, `d` or `t` (text score) and the field, for each field but `$natural`.
fn sort(s: Option<&Document>, out: &mut String) {
    let Some(s) = s else { return };
    let parts: Vec<String> = s
        .iter()
        .filter(|(k, _)| *k != "$natural")
        .map(|(k, v)| {
            let dir = match v {
                Value::Document(d) if d.get("$meta").is_some() => 't',
                _ if number(v) == Some(1.0) => 'a',
                _ => 'd',
            };
            let mut p = dir.to_string();
            user(k, &mut p);
            p
        })
        .collect();
    out.push_str(&parts.join(","));
}

/// A number's value.
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int32(n) => Some(f64::from(*n)),
        Value::Int64(n) => Some(*n as f64),
        Value::Double(n) => Some(*n),
        Value::Decimal128(d) => d.to_string().parse().ok(),
        _ => None,
    }
}

/// The projection section: the fields an inclusion needs, sorted by their bytes and joined with
/// `-`; nothing when the projection needs the whole document.
fn projection(p: Option<&Document>, out: &mut String) {
    let Some(p) = p.filter(|p| !p.is_empty()) else { return };
    let mut needed: BTreeSet<String> = BTreeSet::new();
    let mut with_id = true;
    for (k, v) in p.iter() {
        let flag = match v {
            Value::Boolean(b) => Some(*b),
            _ => number(v).map(|n| n != 0.0),
        };
        match (k, flag) {
            ("_id", Some(f)) => with_id = f,
            (_, Some(false)) => return,
            (_, Some(true)) => {
                needed.insert(k.to_owned());
            }
            (_, None) => match dependencies(v) {
                Some(d) => needed.extend(d),
                None => return,
            },
        }
    }
    if with_id {
        needed.insert("_id".to_owned());
    }
    let parts: Vec<String> = needed
        .iter()
        .map(|f| {
            let mut s = String::new();
            user(f, &mut s);
            s
        })
        .collect();
    out.push_str(&parts.join("-"));
}

/// The fields an expression of a projection reads; `None` when it needs the whole document
/// (`$$ROOT`, `$slice`, `$elemMatch`).
fn dependencies(v: &Value) -> Option<BTreeSet<String>> {
    fn walk(v: &Value, out: &mut BTreeSet<String>) -> Option<()> {
        match v {
            Value::String(s) if s.starts_with("$$") => {
                let var = s.split('.').next().unwrap_or_default();
                (!matches!(var, "$$ROOT" | "$$CURRENT")).then_some(())
            }
            Value::String(s) => {
                if let Some(f) = s.strip_prefix('$') {
                    out.insert(f.to_owned());
                }
                Some(())
            }
            Value::Document(d) if d.get("$literal").is_some() || d.get("$const").is_some() => {
                Some(())
            }
            Value::Document(d) => d.iter().try_for_each(|(k, x)| {
                if matches!(k, "$slice" | "$elemMatch") {
                    return None;
                }
                walk(x, out)
            }),
            Value::Array(items) => items.iter().try_for_each(|x| walk(x, out)),
            _ => Some(()),
        }
    }
    let mut out = BTreeSet::new();
    walk(v, &mut out)?;
    Some(out)
}

/// The collation section: the locale and one digit for each option.
fn collation(c: &CollationSpec, out: &mut String) {
    let bit = |b: bool| if b { '1' } else { '0' };
    out.push_str(&c.locale);
    out.push(bit(c.case_level));
    out.push(match c.case_first {
        CaseFirst::Upper => '0',
        CaseFirst::Lower => '1',
        CaseFirst::Off => '2',
    });
    out.push_str(&(c.strength as u8).to_string());
    out.push(bit(c.numeric_ordering));
    out.push(bit(c.alternate == Alternate::Shifted));
    out.push(match c.max_variable {
        MaxVariable::Punct => '0',
        MaxVariable::Space => '1',
        MaxVariable::Symbol => '2',
        MaxVariable::Currency => '3',
    });
    out.push(bit(c.normalization));
    out.push(bit(c.backwards));
}

/// The path of each `$expr` of filter `f`, in order: the field an `$eq` of one field and one
/// constant compares, as the `refnode` takes it when it parses the expression; empty otherwise.
fn expr_paths(f: &Document) -> Vec<String> {
    fn walk(d: &Document, out: &mut Vec<String>) {
        for (k, v) in d.iter() {
            match (k, v) {
                ("$expr", e) => out.push(eq_path(e).unwrap_or_default()),
                ("$and" | "$or" | "$nor", Value::Array(items)) => {
                    for i in items {
                        if let Value::Document(c) = i {
                            walk(c, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(f, &mut out);
    out
}

/// The field `{$eq: [field, constant]}` compares, either way round.
fn eq_path(e: &Value) -> Option<String> {
    let Value::Document(d) = e else { return None };
    let mut it = d.iter();
    let (Some(("$eq", Value::Array(args))), None) = (it.next(), it.next()) else { return None };
    let [a, b] = args.as_slice() else { return None };
    let (path, other) = match (field_path(a), field_path(b)) {
        (Some(p), _) => (p, b),
        (None, Some(p)) if !is_path_like(a) => (p, a),
        _ => return None,
    };
    let value = match other {
        Value::Document(c) => {
            let mut ci = c.iter();
            match (ci.next(), ci.next()) {
                (Some(("$literal" | "$const", v)), None) => v,
                _ => return None,
            }
        }
        Value::String(s) if s.starts_with('$') => return None,
        v => v,
    };
    (!matches!(value, Value::Null | Value::Undefined | Value::Array(_))).then_some(path)
}

/// Whether `v` is a field path or a variable.
fn is_path_like(v: &Value) -> bool {
    matches!(v, Value::String(s) if s.starts_with('$'))
}

/// The field a field path reads, from the current document.
fn field_path(v: &Value) -> Option<String> {
    let Value::String(s) = v else { return None };
    if let Some(rest) = s.strip_prefix("$$") {
        let (var, tail) = rest.split_once('.')?;
        return matches!(var, "ROOT" | "CURRENT").then(|| tail.to_owned());
    }
    s.strip_prefix('$').map(str::to_owned)
}

/// The indexes and collation the indexability digits are taken over.
struct Indexability<'a> {
    indexes: &'a [KeyIndex],
    collation: Option<&'a CollationSpec>,
}

impl Indexability<'_> {
    /// The digits of a filter document under `parent` (the path of an enclosing `$elemMatch`).
    fn doc(&self, d: &Document, parent: &str, out: &mut String) {
        for (k, v) in d.iter() {
            match (k, v) {
                ("$and" | "$or" | "$nor", Value::Array(items)) => {
                    // A `$nor` of one clause is a `$not`, which an index may answer.
                    if k == "$nor" && items.len() == 1 {
                        out.push_str("<1>");
                    }
                    for i in items {
                        if let Value::Document(c) = i {
                            self.doc(c, parent, out);
                        }
                    }
                }
                (k, _) if k.starts_with('$') => {}
                _ => self.field(&join(parent, k), v, out),
            }
        }
    }

    /// The digits of the predicates of field `path`.
    fn field(&self, path: &str, v: &Value, out: &mut String) {
        let Some(ops) = operators(v) else { return self.leaf(path, "$eq", v, out) };
        for (op, x) in ops.iter().filter(|(k, _)| *k != "$options") {
            match (op, x) {
                ("$not", _) => {
                    // A `$not` of a comparison to an array no index can answer.
                    let with_array = matches!(x, Value::Document(n) if n.iter().any(|(o, y)| {
                        matches!(o, "$eq" | "$lt" | "$lte" | "$gt" | "$gte")
                            && matches!(y, Value::Array(_))
                    }));
                    out.push_str(if with_array { "<0>" } else { "<1>" });
                    self.field(path, x, out);
                }
                ("$elemMatch", Value::Document(inner)) => {
                    self.leaf(path, op, x, out);
                    if inner.iter().all(|(k, _)| k.starts_with('$')) {
                        for (o, y) in inner.iter() {
                            self.leaf(path, o, y, out);
                        }
                    } else {
                        self.doc(inner, path, out);
                    }
                }
                _ => self.leaf(path, op, x, out),
            }
        }
    }

    /// The digits of one predicate: for each index holding `path`, whether it can answer it.
    fn leaf(&self, path: &str, op: &str, x: &Value, out: &mut String) {
        let holding: Vec<&KeyIndex> =
            self.indexes.iter().filter(|i| i.fields.iter().any(|f| f == path)).collect();
        if holding.is_empty() {
            return;
        }
        out.push('<');
        for i in holding {
            let sparse_ok = !i.sparse || !null_equality(op, x);
            let collation_ok = i.collation.as_ref() == self.collation || !collatable(op, x);
            out.push(if sparse_ok && collation_ok { '1' } else { '0' });
        }
        out.push('>');
    }
}

/// `parent.path`, or `path` alone.
fn join(parent: &str, path: &str) -> String {
    if parent.is_empty() { path.to_owned() } else { format!("{parent}.{path}") }
}

/// Whether a predicate compares to null as an equality, which a sparse index cannot answer.
fn null_equality(op: &str, x: &Value) -> bool {
    match (op, x) {
        ("$eq" | "$gte" | "$lte", Value::Null) => true,
        ("$in", Value::Array(items)) => items.iter().any(|i| matches!(i, Value::Null)),
        _ => false,
    }
}

/// Whether a predicate compares strings, which an index of another collation cannot answer.
fn collatable(op: &str, x: &Value) -> bool {
    let of = |v: &Value| {
        matches!(v, Value::String(_) | Value::Symbol(_) | Value::Document(_) | Value::Array(_))
    };
    match (op, x) {
        ("$in", Value::Array(items)) => items.iter().any(of),
        ("$eq" | "$lt" | "$lte" | "$gt" | "$gte", v) => of(v),
        _ => false,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Hashes a `refnode` 9.0.4 gave for these reads (`$planCacheStats` and `explain`).
    const VECTORS: &str = include_str!("../../tests/explain/query-hash-vectors.jsonl");

    fn doc_of(v: Option<&Value>) -> Option<&Document> {
        match v {
            Some(Value::Document(d)) => Some(d),
            _ => None,
        }
    }

    #[test]
    fn shapes_hash_as_a_refnode_hashes_them() {
        let mut checked = 0;
        for line in VECTORS.lines().filter(|l| !l.trim().is_empty()) {
            let v = teseladb_bson::extjson::parse(line).expect("vector");
            let spec = doc_of(v.get("collation"))
                .and_then(|c| crate::commands::views::spec_of(Some(c)).expect("collation"));
            let distinct = match v.get("distinct") {
                Some(Value::String(s)) => Some(s.as_str()),
                _ => None,
            };
            let input = ShapeInput {
                parsed: doc_of(v.get("parsed")),
                original: doc_of(v.get("original")),
                sort: doc_of(v.get("sort")),
                projection: doc_of(v.get("projection")),
                collation: spec.as_ref(),
                distinct,
                api_strict: v.get("apiStrict") == Some(&Value::Boolean(true)),
            };
            let key = shape_key(&input);
            assert_eq!(Some(&Value::String(key.clone())), v.get("key"), "{line}");
            assert_eq!(Some(&Value::String(shape_hash(&key))), v.get("queryHash"), "{line}");
            checked += 1;
        }
        assert_eq!(checked, 290);
    }

    #[test]
    fn plan_cache_keys_mix_the_indexability_and_empty_settings() {
        // Measured on a collection with no index on `a` or `b`.
        assert_eq!(cache_key_hash("eqa||||ftr", ""), "A7BBE03A");
        assert_eq!(cache_key_hash("gta||||ftr", ""), "893CCC1D");
        assert_eq!(cache_key_hash("an||||ftr", ""), "8CEEBFD3");
        assert_eq!(cache_key_hash("ina||||ftr", ""), "4BD04234");
        assert_eq!(cache_key_hash("an[eqa,eqb]||||ftr", ""), "D2430CAC");
        // `x` with one index on it, hidden (battery `plan-plan-more`).
        assert_eq!(cache_key_hash("gtx||||ftr", "<1>"), "8B1738D8");
    }

    #[test]
    fn murmur3_hashes_the_empty_input_with_seed_zero_to_zero() {
        assert_eq!(murmur3_x64_128(b"", 0), 0);
        // A tail longer than one word, and whole blocks.
        assert_eq!(shape_hash("an[eqa,eqb]||||ftr"), "D1F2A12D");
    }

    fn index(fields: &[&str], sparse: bool) -> KeyIndex {
        KeyIndex {
            fields: fields.iter().map(|f| (*f).to_owned()).collect(),
            sparse,
            collation: None,
            hidden: false,
        }
    }

    #[test]
    fn each_predicate_on_an_indexed_path_says_which_indexes_answer_it() {
        let parsed = teseladb_bson::extjson::parse(
            r#"{"$and": [{"a": {"$eq": null}}, {"b": {"$gt": 1}}, {"c": {"$not": {"$eq": 1}}}]}"#,
        )
        .expect("filter");
        let indexes = [index(&["a"], false), index(&["a", "b"], true), index(&["c"], false)];
        // `a` = null: the sparse index cannot answer it.
        assert_eq!(indexability(&parsed, &indexes, None), "<10><1><1><1>");
        // Without indexes only the `$not` says it may use one.
        assert_eq!(indexability(&parsed, &[], None), "<1>");
    }
}
