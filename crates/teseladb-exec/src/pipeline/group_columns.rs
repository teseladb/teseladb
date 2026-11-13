//! `$group` on register columns (010 FR-031, FR-028): when the key is a field path or a constant
//! and every output is `$count`, or `$sum`, `$avg`, `$min` or `$max` of a field path or a
//! constant, each batch of 64 stored documents loads its fields with the register machine's
//! `LoadField` into columns, and the accumulators take the numbers of the registers without
//! evaluating an expression in a scope per document. A path that crosses an array (which gives
//! an array of what each element gives) is evaluated as an expression for that document.

use teseladb_mql::vm::{Cell, load_column};
use teseladb_bson::{Document, RawDoc, Value};

use crate::accum::{Accumulator, Unary};
use crate::expr::{Eval, Expr};

/// The input of a key or an accumulator.
#[derive(Clone, Debug)]
pub(super) enum Input {
    Field(String),
    Const(Value),
}

impl Input {
    fn of(e: &Expr) -> Option<Input> {
        match e {
            Expr::Field(p) => Some(Input::Field(p.clone())),
            Expr::Literal(v) => Some(Input::Const(v.clone())),
            _ => None,
        }
    }
}

/// A `$group` that runs on columns: its key's input and each output's (`None` for `$count`).
#[derive(Clone, Debug)]
pub(super) struct Columnar {
    key: Input,
    inputs: Vec<Option<Input>>,
}

/// The plan of a `$group` by `id` with `outputs` on columns, when it has one.
pub(super) fn columnar(id: &Expr, outputs: &[(String, Accumulator)]) -> Option<Columnar> {
    let key = Input::of(id)?;
    let inputs = outputs
        .iter()
        .map(|(_, acc)| match acc {
            Accumulator::Count => Some(None),
            Accumulator::Unary(Unary::Sum | Unary::Avg | Unary::Min | Unary::Max, e) => {
                Input::of(e).map(Some)
            }
            _ => None,
        })
        .collect::<Option<Vec<Option<Input>>>>()?;
    Some(Columnar { key, inputs })
}

/// The stored bytes of each of `docs`, when all have them.
pub(super) fn stored<'d>(docs: &'d [Document]) -> Option<Vec<RawDoc<'d>>> {
    docs.iter().map(|d| d.canonical_bytes().and_then(|b| RawDoc::from_stored(b).ok())).collect()
}

/// The columns of a batch.
pub(super) struct Columns<'d> {
    key: Vec<Cell<'d>>,
    inputs: Vec<Vec<Cell<'d>>>,
}

impl Columnar {
    /// Loads the columns of `docs`.
    pub(super) fn load<'d>(&self, docs: &[RawDoc<'d>]) -> Columns<'d> {
        let column = |i: &Input| {
            let mut out = Vec::with_capacity(docs.len());
            if let Input::Field(p) = i {
                load_column(docs, p, &mut out);
            }
            out
        };
        Columns {
            key: column(&self.key),
            inputs: self
                .inputs
                .iter()
                .map(|i| i.as_ref().map(column).unwrap_or_default())
                .collect(),
        }
    }

    /// The key of document `lane`; `None` when the expression must give it.
    pub(super) fn key(&self, cols: &Columns<'_>, lane: usize) -> Option<Value> {
        value_of(&self.key, &cols.key, lane).map(|v| v.unwrap_or(Value::Null))
    }

    /// The input of output `k` for document `lane` (`Some(None)` for `$count`, which takes
    /// none); `None` when the expression must give it.
    pub(super) fn input(&self, cols: &Columns<'_>, k: usize, lane: usize) -> Option<Eval> {
        match &self.inputs[k] {
            None => Some(None),
            Some(i) => value_of(i, &cols.inputs[k], lane),
        }
    }
}

/// What `input` gives for document `lane` of `column`.
fn value_of(input: &Input, column: &[Cell<'_>], lane: usize) -> Option<Eval> {
    match input {
        Input::Const(v) => Some(Some(v.clone())),
        Input::Field(_) => match column[lane] {
            Cell::Missing => Some(None),
            Cell::Int32(n) => Some(Some(Value::Int32(n))),
            Cell::Int64(n) => Some(Some(Value::Int64(n))),
            Cell::Double(x) => Some(Some(Value::Double(x))),
            Cell::Value(raw) => Some(Some(Value::from(raw))),
            Cell::ThroughArray => None,
        },
    }
}
