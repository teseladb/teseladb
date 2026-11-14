//! The distributed run of a pipeline (010 FR-008, T021, T022): the stages each range can run
//! alone go to every range; a `$sort` (with the `$limit` after it) sorts in each range and the
//! coordinator merges the sorted parts; a `$group` whose accumulators combine groups in each
//! range, its partial groups are spread by the hash of their key over the nodes that combine
//! them ([`shuffle`]), and the coordinator joins what they give; the rest runs at the coordinator.
//! The result is the one the [`crate::reference::Reference`] evaluator gives over all the
//! documents at the same mark, which the `queries` load of the simulator checks.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;

use crate::expr::Ctx;
use crate::pipeline::{Env, Pipeline};
use crate::raw::bad;

/// The stages each range runs whole: they look at one document at a time.
const STREAMED: [&str; 9] = [
    "$match",
    "$project",
    "$addFields",
    "$set",
    "$unset",
    "$replaceRoot",
    "$replaceWith",
    "$redact",
    "$unwind",
];

/// How the coordinator joins what the ranges give.
#[derive(Clone, Debug, PartialEq)]
pub enum Merge {
    /// The parts one after another.
    Concat,
    /// The sorted parts merged by `sort`, the first `limit` kept.
    Sorted {
        /// The sort.
        sort: Document,
        /// The documents kept (skip and limit).
        limit: Option<i64>,
    },
    /// The partial groups combined by `group` (spread over nodes by the hash of their key),
    /// then `finish` (`$avg` from its sum and count).
    Group {
        /// The `$group` that combines partial groups.
        group: Document,
        /// The stages after it that finish the values.
        finish: Vec<Document>,
    },
}

/// A pipeline split for a distributed run.
#[derive(Clone, Debug, PartialEq)]
pub struct Split {
    /// What each range runs.
    pub shard: Vec<Document>,
    /// How the coordinator joins the parts.
    pub merge: Merge,
    /// What the coordinator runs after joining them.
    pub rest: Vec<Document>,
}

/// The name of a stage.
fn name_of(st: &Document) -> &str {
    st.iter().next().map_or("", |(k, _)| k)
}

/// The field of a partial `$avg` that counts its numbers.
fn count_field(f: &str) -> String {
    format!("\u{1}n:{f}")
}

/// Fields of a `$group` and their accumulators.
type Fields = Vec<(String, Value)>;

/// How one field of a `$group` runs split: its partial fields in each range, the fields that
/// combine them, and the value that finishes it.
type Parts = (Fields, Fields, Option<Value>);

/// The partial and combining accumulators of `$group` field `f` with accumulator `acc`, and the
/// value that finishes it; `None` for one that does not combine.
fn partial(f: &str, acc: &Value) -> Option<Parts> {
    let Value::Document(d) = acc else { return None };
    let (op, arg) = d.iter().next()?;
    if d.len() != 1 {
        return None;
    }
    let on = |op: &str, arg: Value| Value::Document(Document::new().with(op, arg));
    let field = Value::String(format!("${f}"));
    Some(match op {
        "$sum" | "$min" | "$max" | "$first" | "$last" | "$mergeObjects" => {
            let combine = if op == "$sum" { "$sum" } else { op };
            (vec![(f.to_owned(), acc.clone())], vec![(f.to_owned(), on(combine, field))], None)
        }
        "$count" => (
            vec![(f.to_owned(), on("$sum", Value::Int32(1)))],
            vec![(f.to_owned(), on("$sum", field))],
            None,
        ),
        "$push" => (
            vec![(f.to_owned(), acc.clone())],
            vec![(f.to_owned(), on("$concatArrays", field))],
            None,
        ),
        "$addToSet" => {
            (vec![(f.to_owned(), acc.clone())], vec![(f.to_owned(), on("$setUnion", field))], None)
        }
        "$avg" => {
            let n = count_field(f);
            let numbers = on(
                "$cond",
                Value::Array(vec![on("$isNumber", arg.clone()), Value::Int32(1), Value::Int32(0)]),
            );
            let finish = on(
                "$cond",
                Value::Array(vec![
                    on("$eq", Value::Array(vec![Value::String(format!("${n}")), Value::Int32(0)])),
                    Value::Null,
                    on(
                        "$divide",
                        Value::Array(vec![field.clone(), Value::String(format!("${n}"))]),
                    ),
                ]),
            );
            (
                vec![(f.to_owned(), on("$sum", arg.clone())), (n.clone(), on("$sum", numbers))],
                vec![
                    (f.to_owned(), on("$sum", field)),
                    (n.clone(), on("$sum", Value::String(format!("${n}")))),
                ],
                Some(finish),
            )
        }
        _ => return None,
    })
}

/// The partial `$group` of each range, the one that combines them and the stages that finish
/// the values; `None` when an accumulator does not combine.
fn split_group(spec: &Document) -> Option<(Document, Document, Vec<Document>)> {
    let mut shard = Document::new().with("_id", spec.get("_id")?.clone());
    let mut merge = Document::new().with("_id", "$_id");
    let mut finish = Document::new();
    let mut helpers = Vec::new();
    for (f, acc) in spec.iter().filter(|(k, _)| *k != "_id") {
        let (parts, combine, done) = partial(f, acc)?;
        for (k, v) in parts {
            shard.push(k, v);
        }
        for (k, v) in combine {
            merge.push(k.clone(), v);
            if k != f {
                helpers.push(Value::String(k));
            }
        }
        if let Some(v) = done {
            finish.push(f, v);
        }
    }
    let mut stages = Vec::new();
    if !finish.is_empty() {
        stages.push(Document::new().with("$addFields", finish));
    }
    if !helpers.is_empty() {
        stages.push(Document::new().with("$unset", Value::Array(helpers)));
    }
    Some((Document::new().with("$group", shard), Document::new().with("$group", merge), stages))
}

/// Splits `stages` for a run over ranges.
pub fn split(stages: &[Document]) -> Split {
    let streamed = stages.iter().take_while(|s| STREAMED.contains(&name_of(s))).count();
    let mut shard: Vec<Document> = stages[..streamed].to_vec();
    let rest = &stages[streamed..];
    let Some(next) = rest.first() else {
        return Split { shard, merge: Merge::Concat, rest: Vec::new() };
    };
    match (name_of(next), next.iter().next().map(|(_, v)| v)) {
        ("$sort", Some(Value::Document(sort))) => {
            let limit = match rest.get(1).and_then(|s| s.get("$limit")) {
                Some(v) => crate::expr::values::int64_of(v),
                None => None,
            };
            shard.push(next.clone());
            if let Some(l) = limit {
                shard.push(Document::new().with("$limit", l));
            }
            Split {
                shard,
                merge: Merge::Sorted { sort: sort.clone(), limit },
                rest: rest[1..].to_vec(),
            }
        }
        ("$limit", Some(v)) => {
            shard.push(Document::new().with("$limit", v.clone()));
            Split { shard, merge: Merge::Concat, rest: rest.to_vec() }
        }
        ("$group", Some(Value::Document(g))) => match split_group(g) {
            Some((partial, group, finish)) => {
                shard.push(partial);
                Split { shard, merge: Merge::Group { group, finish }, rest: rest[1..].to_vec() }
            }
            None => Split { shard, merge: Merge::Concat, rest: rest.to_vec() },
        },
        _ => Split { shard, merge: Merge::Concat, rest: rest.to_vec() },
    }
}

/// `stages` parsed as a pipeline over a collection.
fn parse(stages: &[Document]) -> Result<Pipeline, MqlError> {
    let bytes: Vec<Vec<u8>> =
        stages.iter().map(Document::to_bytes).collect::<Result<_, _>>().map_err(bad)?;
    let raws: Vec<RawDoc<'_>> =
        bytes.iter().map(|b| RawDoc::from_bytes(b)).collect::<Result<_, _>>().map_err(bad)?;
    let values: Vec<RawValue<'_>> = raws.into_iter().map(RawValue::Document).collect();
    let names: Vec<String> = Vec::new();
    Pipeline::parse(&values, &Ctx::checked(names.iter().map(String::as_str)))
}

/// The partial groups of `docs` spread over `nodes` by the hash of their `_id` (its group key in
/// the command's collation), each node's in their order (FR-008, T022).
pub fn shuffle(docs: Vec<Document>, nodes: usize, env: &dyn Env) -> Vec<Vec<Document>> {
    let n = nodes.max(1);
    let collation = env.collation();
    let mut out = vec![Vec::new(); n];
    for d in docs {
        let key = crate::pipeline::group_key(d.get("_id").unwrap_or(&Value::Null), collation.get());
        let node = usize::try_from(teseladb_hash::crc32c(&key)).unwrap_or(0) % n;
        out[node].push(d);
    }
    out
}

/// Runs `stages` over the documents of each range in `parts` (in range order) as the ranges and
/// the coordinator run them.
pub fn run_distributed(
    stages: &[Document],
    parts: Vec<Vec<Document>>,
    env: &dyn Env,
) -> Result<Vec<Document>, MqlError> {
    let s = split(stages);
    let shard = parse(&s.shard)?;
    let nodes = parts.len();
    let mut outs = Vec::with_capacity(nodes);
    for p in parts {
        outs.push(shard.run_in(p, env, false)?);
    }
    let joined = match &s.merge {
        Merge::Concat => outs.into_iter().flatten().collect(),
        Merge::Sorted { sort, limit } => {
            let mut merge = vec![Document::new().with("$sort", sort.clone())];
            if let Some(l) = limit {
                merge.push(Document::new().with("$limit", *l));
            }
            parse(&merge)?.run_in(outs.into_iter().flatten().collect(), env, false)?
        }
        Merge::Group { group, finish } => {
            let mut stages = vec![group.clone()];
            stages.extend(finish.iter().cloned());
            let combine = parse(&stages)?;
            let mut partials: Vec<Document> = outs.into_iter().flatten().collect();
            // A combination that takes a partial group twice (`queries` must catch it).
            if cfg!(teseladb_bug = "partial_double_count")
                && let Some(first) = partials.first().cloned()
            {
                partials.push(first);
            }
            let mut all = Vec::new();
            for part in shuffle(partials, nodes, env) {
                all.extend(combine.run_in(part, env, false)?);
            }
            all
        }
    };
    parse(&s.rest)?.run_in(joined, env, false)
}
