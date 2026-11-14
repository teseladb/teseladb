//! The fields of the documents a pipeline reads that its stages use (the reference server's dependency
//! analysis, which decides the projection `explain` shows under the stages a query plan runs,
//! measured on a reference router 9.0.4): the paths each stage reads, up to the first stage that makes
//! new documents (`$group`, an inclusion `$project`, `$replaceRoot`…); a stage that passes the
//! whole document on, or a pipeline that ends without such a stage, needs every field.

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};

/// The paths of the input documents `stages` use, in order and without a path under another;
/// `None` when they need the whole document.
pub fn needed(stages: &[Document]) -> Option<Vec<String>> {
    let mut paths = BTreeSet::new();
    let mut produced: BTreeSet<String> = BTreeSet::new();
    for st in stages {
        let (name, arg) = st.iter().next()?;
        let mut used = BTreeSet::new();
        let before = produced.clone();
        let ends = match (name, arg) {
            ("$group", Value::Document(g)) => {
                for (_, v) in g.iter() {
                    expression(v, &mut used)?;
                }
                true
            }
            ("$bucket" | "$bucketAuto", Value::Document(b)) => {
                for (k, v) in b.iter() {
                    if k == "groupBy" || k == "output" {
                        expression(v, &mut used)?;
                    }
                }
                true
            }
            ("$sortByCount" | "$replaceWith", v) => {
                expression(v, &mut used)?;
                true
            }
            ("$replaceRoot", Value::Document(r)) => {
                expression(r.get("newRoot").unwrap_or(&Value::Null), &mut used)?;
                true
            }
            ("$count", _) => true,
            // Each pipeline of a `$facet` reads what it needs; together, what any reads.
            ("$facet", Value::Document(f)) => {
                for (_, sub) in f.iter() {
                    let Value::Array(items) = sub else { return None };
                    let stages: Vec<Document> = items
                        .iter()
                        .map(|i| match i {
                            Value::Document(d) => Some(d.clone()),
                            _ => None,
                        })
                        .collect::<Option<_>>()?;
                    used.extend(needed(&stages)?);
                }
                true
            }
            ("$project", Value::Document(p)) if inclusion(p) => {
                projection(p, "", &mut used)?;
                if !excludes_id(p) {
                    used.insert("_id".to_owned());
                }
                true
            }
            ("$addFields" | "$set", Value::Document(p)) => {
                set_fields(p, "", &mut used, &mut produced)?;
                false
            }
            ("$match", Value::Document(f)) => {
                filter(f, &mut used)?;
                false
            }
            ("$sort", Value::Document(s)) => {
                used.extend(s.iter().map(|(k, _)| k.to_owned()));
                false
            }
            ("$limit" | "$skip", _) => false,
            ("$fill", Value::Document(f)) => {
                if let Some(Value::Document(out)) = f.get("output") {
                    for (k, _) in out.iter() {
                        used.insert(k.to_owned());
                        produced.insert(k.to_owned());
                    }
                }
                for k in ["partitionBy", "partitionByFields", "sortBy"] {
                    match f.get(k) {
                        Some(Value::Document(d)) if k == "sortBy" => {
                            used.extend(d.iter().map(|(x, _)| x.to_owned()));
                        }
                        Some(v) => expression(v, &mut used)?,
                        None => {}
                    }
                }
                false
            }
            ("$setWindowFields", Value::Document(w)) => {
                window_fields(w, &mut used, &mut produced)?;
                false
            }
            ("$unwind", u) => {
                let path = match u {
                    Value::String(p) => Some(p.as_str()),
                    Value::Document(d) => match d.get("path") {
                        Some(Value::String(p)) => Some(p.as_str()),
                        _ => None,
                    },
                    _ => None,
                };
                used.insert(path?.strip_prefix('$')?.to_owned());
                false
            }
            ("$lookup", Value::Document(l)) if l.get("pipeline").is_none() => {
                let (Some(Value::String(local)), Some(Value::String(as_field))) =
                    (l.get("localField"), l.get("as"))
                else {
                    return None;
                };
                used.insert(local.clone());
                produced.insert(as_field.clone());
                false
            }
            // A projection that drops fields reads none.
            ("$project", Value::Document(_)) | ("$unset", _) => false,
            _ => return None,
        };
        paths.extend(used.into_iter().filter(|p| !before.iter().any(|q| covers(q, p))));
        if ends {
            return Some(minimal(paths));
        }
    }
    None
}

/// Whether path `a` is `b` or a prefix of it.
fn covers(a: &str, b: &str) -> bool {
    b == a || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('.'))
}

/// `paths` without those under another.
fn minimal(paths: BTreeSet<String>) -> Vec<String> {
    let all: Vec<String> = paths.into_iter().collect();
    all.iter().filter(|p| !all.iter().any(|q| q != *p && covers(q, p))).cloned().collect()
}

/// Whether a `$project` keeps fields (rather than dropping them).
fn inclusion(p: &Document) -> bool {
    let kept = |v: &Value| {
        !matches!(v, Value::Int32(0) | Value::Int64(0) | Value::Boolean(false))
            && !matches!(v, Value::Double(x) if *x == 0.0)
    };
    // `{_id: 1}` alone keeps `_id`; `_id` beside other fields decides nothing.
    p.iter().filter(|(k, _)| *k != "_id").any(|(_, v)| kept(v))
        || (p.len() == 1 && p.get("_id").is_some_and(kept))
}

/// Whether a `$project` drops `_id`.
fn excludes_id(p: &Document) -> bool {
    matches!(p.get("_id"), Some(Value::Int32(0) | Value::Int64(0) | Value::Boolean(false)))
        || matches!(p.get("_id"), Some(Value::Double(x)) if *x == 0.0)
}

/// The paths an inclusion `$project` reads (under `prefix`).
fn projection(p: &Document, prefix: &str, used: &mut BTreeSet<String>) -> Option<()> {
    for (k, v) in p.iter() {
        let path = if prefix.is_empty() { k.to_owned() } else { format!("{prefix}.{k}") };
        match v {
            Value::Boolean(true) | Value::Int32(_) | Value::Int64(_) | Value::Double(_) => {
                if k != "_id" {
                    used.insert(path);
                }
            }
            Value::Boolean(false) => {}
            Value::Document(d) if d.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) => {
                projection(d, &path, used)?;
            }
            // A field computed inside a document needs the whole top-level field (measured).
            other => {
                expression(other, used)?;
                if let Some((top, _)) = path.split_once('.') {
                    used.insert(top.to_owned());
                }
            }
        }
    }
    Some(())
}

/// The paths `$addFields` reads, and the fields it sets.
fn set_fields(
    p: &Document,
    prefix: &str,
    used: &mut BTreeSet<String>,
    produced: &mut BTreeSet<String>,
) -> Option<()> {
    for (k, v) in p.iter() {
        let path = if prefix.is_empty() { k.to_owned() } else { format!("{prefix}.{k}") };
        match v {
            Value::Document(d) if d.iter().next().is_some_and(|(x, _)| !x.starts_with('$')) => {
                set_fields(d, &path, used, produced)?;
            }
            other => {
                expression(other, used)?;
                produced.insert(path);
            }
        }
    }
    Some(())
}

/// The paths a `$setWindowFields` reads (its partition, order and window functions), and the
/// fields it sets.
fn window_fields(
    w: &Document,
    used: &mut BTreeSet<String>,
    produced: &mut BTreeSet<String>,
) -> Option<()> {
    if let Some(p) = w.get("partitionBy") {
        expression(p, used)?;
    }
    if let Some(Value::Document(by)) = w.get("sortBy") {
        used.extend(by.iter().map(|(k, _)| k.to_owned()));
    }
    if let Some(Value::Document(out)) = w.get("output") {
        for (name, f) in out.iter() {
            if let Value::Document(spec) = f {
                for (k, arg) in spec.iter().filter(|(k, _)| *k != "window") {
                    if k.starts_with('$') {
                        expression(arg, used)?;
                    }
                }
            }
            produced.insert(name.to_owned());
        }
    }
    Some(())
}

/// The paths a filter reads; `None` for one that reads the whole document.
fn filter(f: &Document, used: &mut BTreeSet<String>) -> Option<()> {
    for (k, v) in f.iter() {
        match (k, v) {
            ("$and" | "$or" | "$nor", Value::Array(items)) => {
                for i in items {
                    let Value::Document(d) = i else { return None };
                    filter(d, used)?;
                }
            }
            ("$expr", e) => expression(e, used)?,
            ("$comment", _) => {}
            (k, _) if k.starts_with('$') => return None,
            (k, _) => {
                used.insert(k.to_owned());
            }
        }
    }
    Some(())
}

/// The paths expression `v` reads; `None` for one that reads the whole document.
fn expression(v: &Value, used: &mut BTreeSet<String>) -> Option<()> {
    match v {
        Value::String(s) if s == "$$ROOT" || s == "$$CURRENT" => None,
        Value::String(s) if s.starts_with("$$ROOT.") || s.starts_with("$$CURRENT.") => {
            let rest = s.split_once('.').map_or("", |(_, r)| r);
            used.insert(rest.to_owned());
            Some(())
        }
        Value::String(s) if s.starts_with("$$") => Some(()),
        Value::String(s) if s.starts_with('$') => {
            used.insert(s[1..].to_owned());
            Some(())
        }
        Value::Array(items) => items.iter().try_for_each(|i| expression(i, used)),
        Value::Document(d) => match d.iter().next() {
            Some(("$literal" | "$const", _)) => Some(()),
            Some(("$rand", _)) => Some(()),
            // The path a `distinct` on a view reads, given as a constant (measured on a reference router
            // 9.0.4).
            Some(("$_internalFindAllValuesAtPath", Value::Array(args))) => {
                let path = match args.first() {
                    Some(Value::Document(c)) => c.get("$const").or_else(|| c.get("$literal")),
                    other => other,
                };
                match path {
                    Some(Value::String(p)) => {
                        used.insert(p.clone());
                        Some(())
                    }
                    _ => None,
                }
            }
            Some(("$meta" | "$function" | "$accumulator", _)) => None,
            // The named arguments of `$top` and the like sort by fields.
            _ => d.iter().try_for_each(|(k, x)| match (k, x) {
                ("sortBy", Value::Document(by)) => {
                    used.extend(by.iter().map(|(f, _)| f.to_owned()));
                    Some(())
                }
                _ => expression(x, used),
            }),
        },
        _ => Some(()),
    }
}
