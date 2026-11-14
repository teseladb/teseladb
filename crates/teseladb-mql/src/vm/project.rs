//! Projections of `find` on the register machine (010 FR-031).
//!
//! A projection's tree is compiled once per projection into a [`Projector`]: a contiguous array
//! of 64-bit steps ([`Step`]: an 8-bit code, 8 bits of flags, a 16-bit argument and a 32-bit
//! immediate) in blocks, one per level of the tree. A block starts with a `Level` step that gives
//! how many fields it names, followed by one step per field whose code is what happens to it and
//! whose immediate is the index of its name. The projector walks the document's bytes once per
//! level, looks each field up in its block and writes the output's BSON as it goes: a field kept
//! whole is copied byte for byte, an array that is cut (`$slice`, `$`, `$elemMatch`) is written
//! with its elements renumbered, and nothing is decoded into values. The result is the one of
//! the tree evaluator ([`crate::projection::Projection::apply_by_tree`]).
//!
//! Computed fields, `$meta` and an `$elemMatch` of an inclusion are evaluated on the whole
//! document after the rest, in the projection's order, by the command's extensions: the
//! program ends with one `Ext` step that hands them to the tree's pass for those fields.
//! A projection runs on one document at a time: its output is a document per input, with
//! nothing a mask of live documents would save.

use std::fmt::Write as _;
use std::sync::Arc;

use teseladb_keys::stored::{Element, element};
use teseladb_bson::{RawArray, RawDoc, RawValue};

use crate::error::Result;
use crate::parse::Pred;
use crate::projection::{
    Action, Node, ProjectionContext, Tree, first_match_at, has_positional, mismatch, no_position,
};

/// The step codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Code {
    /// A block's first step: `imm` fields follow.
    Level,
    /// The field is kept whole.
    Keep,
    /// The field is left out.
    Drop,
    /// `$slice` number `arg` of the field's array.
    Slice,
    /// The element of the field's array at the filter's position (`a.$`).
    Positional,
    /// The first element of the field's array that predicate `arg` accepts.
    ElemMatch,
    /// A field the extensions compute after the rest (computed fields and `$meta`).
    Late,
    /// The field's documents go through block `arg`; flag [`POSITIONAL_BELOW`] when a `$`
    /// is below.
    Descend,
    /// The program's last step when computed fields follow.
    Ext,
}

impl Code {
    const ALL: [Code; 9] = [
        Code::Level,
        Code::Keep,
        Code::Drop,
        Code::Slice,
        Code::Positional,
        Code::ElemMatch,
        Code::Late,
        Code::Descend,
        Code::Ext,
    ];
}

/// The flag of a `Descend` with a `$` below it.
const POSITIONAL_BELOW: u8 = 1;

/// One step: code, flags, argument and immediate, in 64 bits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Step(u64);

impl Step {
    fn new(code: Code, flags: u8, arg: u16, imm: u32) -> Step {
        Step(
            (code as u64)
                | (u64::from(flags) << 8)
                | (u64::from(arg) << 16)
                | (u64::from(imm) << 32),
        )
    }

    #[inline(always)]
    fn code(self) -> Code {
        Code::ALL[usize::from(self.0 as u8)]
    }

    #[inline(always)]
    fn flags(self) -> u8 {
        (self.0 >> 8) as u8
    }

    #[inline(always)]
    fn arg(self) -> usize {
        usize::from((self.0 >> 16) as u16)
    }

    #[inline(always)]
    fn imm(self) -> usize {
        (self.0 >> 32) as u32 as usize
    }
}

impl std::fmt::Debug for Step {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} {} {} #{}", self.code(), self.flags(), self.arg(), self.imm())
    }
}

/// A compiled projection.
#[derive(Debug)]
pub struct Projector {
    code: Vec<Step>,
    /// The field names of the steps.
    names: Vec<Box<[u8]>>,
    /// Where each block starts in [`Projector::code`]; the first is the document's.
    blocks: Vec<u32>,
    /// The `$slice`s: elements to skip and to take.
    slices: Vec<(Option<i64>, i64)>,
    /// The predicates of the `$elemMatch`es of an exclusion.
    preds: Vec<Arc<Pred>>,
    inclusion: bool,
}

/// What does not fit a step's fields: more than 65,536 blocks, slices or predicates.
#[derive(Debug)]
pub struct TooLarge;

impl Projector {
    /// The projector of `tree`, an inclusion or an exclusion.
    pub(crate) fn compile(
        tree: &Tree,
        inclusion: bool,
    ) -> std::result::Result<Projector, TooLarge> {
        let mut p = Projector {
            code: Vec::new(),
            names: Vec::new(),
            blocks: Vec::new(),
            slices: Vec::new(),
            preds: Vec::new(),
            inclusion,
        };
        p.block(tree)?;
        if crate::projection::has_computed(tree) {
            p.code.push(Step::new(Code::Ext, 0, 0, 0));
        }
        Ok(p)
    }

    /// Emits the block of `tree` and those below it; gives its index.
    fn block(&mut self, tree: &Tree) -> std::result::Result<u16, TooLarge> {
        let index = u16::try_from(self.blocks.len()).map_err(|_| TooLarge)?;
        self.blocks.push(u32::try_from(self.code.len()).map_err(|_| TooLarge)?);
        let count = u32::try_from(tree.fields.len()).map_err(|_| TooLarge)?;
        self.code.push(Step::new(Code::Level, 0, 0, count));
        // The fields' steps come right after the header; the blocks below go after them.
        let at = self.code.len();
        self.code.extend((0..tree.fields.len()).map(|_| Step::new(Code::Drop, 0, 0, 0)));
        for (i, (name, node)) in tree.fields.iter().enumerate() {
            let imm = u32::try_from(self.names.len()).map_err(|_| TooLarge)?;
            self.names.push(name.as_bytes().into());
            let (code, flags, arg) = match node {
                Node::Leaf(Action::Include) => (Code::Keep, 0, 0),
                Node::Leaf(Action::Exclude) => (Code::Drop, 0, 0),
                Node::Leaf(Action::Slice { skip, limit }) => {
                    self.slices.push((*skip, *limit));
                    (Code::Slice, 0, u16::try_from(self.slices.len() - 1).map_err(|_| TooLarge)?)
                }
                Node::Leaf(Action::Positional) => (Code::Positional, 0, 0),
                Node::Leaf(Action::ElemMatch(pred)) => {
                    self.preds.push(Arc::clone(pred));
                    let arg = u16::try_from(self.preds.len() - 1).map_err(|_| TooLarge)?;
                    (Code::ElemMatch, 0, arg)
                }
                Node::Leaf(Action::Meta(_) | Action::Computed(_)) => (Code::Late, 0, 0),
                Node::Sub(sub) => {
                    let flags = if has_positional(sub) { POSITIONAL_BELOW } else { 0 };
                    (Code::Descend, flags, self.block(sub)?)
                }
            };
            self.code[at + i] = Step::new(code, flags, arg, imm);
        }
        Ok(index)
    }

    /// The steps as text, one per line.
    pub fn listing(&self) -> Vec<String> {
        self.code.iter().map(|s| format!("{s:?}")).collect()
    }

    /// The BSON of `doc` projected, and whether the computed fields are still to be evaluated
    /// on it (the program's `Ext` step).
    pub(crate) fn run(
        &self,
        doc: RawDoc<'_>,
        ctx: &ProjectionContext<'_>,
    ) -> Result<(Vec<u8>, bool)> {
        let mut out = Vec::with_capacity(doc.as_bytes().len());
        let run = Run { p: self, ctx };
        if self.inclusion {
            run.include(0, doc, false, &mut out)?;
        } else {
            run.exclude(0, doc, &mut out)?;
        }
        let ext = self.code.last().is_some_and(|s| s.code() == Code::Ext);
        Ok((out, ext))
    }

    /// The step of field `name` in block `block`.
    #[inline(always)]
    fn lookup(&self, block: usize, name: &[u8]) -> Option<Step> {
        let at = self.blocks[block] as usize;
        let count = self.code[at].imm();
        self.code[at + 1..=at + count].iter().copied().find(|s| *self.names[s.imm()] == *name)
    }
}

/// An element of a document's bytes: where it starts, its parts and its value as the reader
/// gives it.
struct Field<'a> {
    at: usize,
    e: Element,
    v: RawValue<'a>,
}

impl Field<'_> {
    /// Its name.
    fn key<'b>(&self, bytes: &'b [u8]) -> &'b [u8] {
        &bytes[self.at + 1..self.e.name_end]
    }
}

/// The fields of BSON `bytes` with the values the reader gives for them.
fn fields<'a>(
    bytes: &'a [u8],
    values: impl Iterator<Item = RawValue<'a>>,
) -> impl Iterator<Item = Result<Field<'a>>> {
    let mut pos = 4;
    values.map(move |v| {
        let at = pos;
        let e = element(bytes, at).ok_or_else(broken)?;
        pos = e.value + e.len;
        Ok(Field { at, e, v })
    })
}

/// A document whose bytes the reader read and the walker does not.
fn broken() -> crate::error::MqlError {
    crate::error::bad_value("a document whose elements do not read")
}

/// Starts a document or array in `out`; gives where its length goes.
fn begin(out: &mut Vec<u8>) -> usize {
    let at = out.len();
    out.extend_from_slice(&[0; 4]);
    at
}

/// Ends the document or array that starts at `at`.
fn end(out: &mut Vec<u8>, at: usize) {
    out.push(0);
    let len = i32::try_from(out.len() - at).unwrap_or(i32::MAX);
    out[at..at + 4].copy_from_slice(&len.to_le_bytes());
}

/// Writes the name of an element of type `ty`.
fn name(out: &mut Vec<u8>, ty: u8, key: &[u8]) {
    out.push(ty);
    out.extend_from_slice(key);
    out.push(0);
}

/// Copies field `f` of `bytes` whole.
fn copy(out: &mut Vec<u8>, bytes: &[u8], f: &Field<'_>) {
    out.extend_from_slice(&bytes[f.at..f.e.value + f.e.len]);
}

/// Writes the value of element `e` of `bytes` under `key`.
fn renamed(out: &mut Vec<u8>, key: &[u8], bytes: &[u8], e: &Element) {
    name(out, e.ty, key);
    out.extend_from_slice(&bytes[e.value..e.value + e.len]);
}

/// The name of the element at `i` of an array.
fn index_key(i: usize, buf: &mut String) -> &[u8] {
    buf.clear();
    let _ = write!(buf, "{i}");
    buf.as_bytes()
}

struct Run<'p, 'c, 'a> {
    p: &'p Projector,
    ctx: &'c ProjectionContext<'a>,
}

impl Run<'_, '_, '_> {
    /// Writes the included fields of `doc` as a document; `done` once `$` has taken its element.
    fn include(&self, block: usize, doc: RawDoc<'_>, done: bool, out: &mut Vec<u8>) -> Result<()> {
        let bytes = doc.as_bytes();
        let at = begin(out);
        for f in fields(bytes, doc.iter().map(|(_, v)| v)) {
            let f = f?;
            let (key, v, e) = (f.key(bytes), f.v, f.e);
            let Some(step) = self.p.lookup(block, key) else { continue };
            match step.code() {
                Code::Keep => copy(out, bytes, &f),
                Code::Slice => self.slice(step, key, bytes, &f, out),
                Code::Positional if done => copy(out, bytes, &f),
                Code::Positional => self.positional(key, bytes, &f, out)?,
                Code::Descend => match v {
                    RawValue::Document(d) => {
                        name(out, e.ty, key);
                        self.include(step.arg(), d, done, out)?;
                    }
                    RawValue::Array(arr) if !done && step.flags() & POSITIONAL_BELOW != 0 => {
                        name(out, e.ty, key);
                        let list = begin(out);
                        let mut buf = String::new();
                        let k = index_key(0, &mut buf);
                        match self.element(arr)? {
                            (RawValue::Document(d), _) => {
                                name(out, 0x03, k);
                                self.include(step.arg(), d, true, out)?;
                            }
                            (RawValue::Array(inner), _) => {
                                name(out, 0x04, k);
                                self.include_array(step.arg(), inner, true, out)?;
                            }
                            _ => return Err(mismatch()),
                        }
                        end(out, list);
                    }
                    RawValue::Array(arr) => {
                        name(out, e.ty, key);
                        self.include_array(step.arg(), arr, done, out)?;
                    }
                    _ => {}
                },
                Code::Drop | Code::ElemMatch | Code::Late | Code::Level | Code::Ext => {}
            }
        }
        end(out, at);
        Ok(())
    }

    /// Writes the elements of `arr` that are documents or arrays, through block `block`.
    fn include_array(
        &self,
        block: usize,
        arr: RawArray<'_>,
        done: bool,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let at = begin(out);
        let mut buf = String::new();
        let mut n = 0;
        for v in arr.iter() {
            match v {
                RawValue::Document(d) => {
                    name(out, 0x03, index_key(n, &mut buf));
                    self.include(block, d, done, out)?;
                }
                RawValue::Array(inner) => {
                    name(out, 0x04, index_key(n, &mut buf));
                    self.include_array(block, inner, done, out)?;
                }
                _ => continue,
            }
            n += 1;
        }
        end(out, at);
        Ok(())
    }

    /// Writes `doc` without its excluded fields.
    fn exclude(&self, block: usize, doc: RawDoc<'_>, out: &mut Vec<u8>) -> Result<()> {
        let bytes = doc.as_bytes();
        let at = begin(out);
        for f in fields(bytes, doc.iter().map(|(_, v)| v)) {
            let f = f?;
            let (key, v, e) = (f.key(bytes), f.v, f.e);
            match self.p.lookup(block, key) {
                Some(s) if s.code() == Code::Drop => {}
                Some(s) if s.code() == Code::Slice => self.slice(s, key, bytes, &f, out),
                // In an exclusion `$elemMatch` keeps the field's place, or removes it.
                Some(s) if s.code() == Code::ElemMatch => {
                    if let RawValue::Array(arr) = v
                        && let Some(i) = first_match_at(self.ctx, &self.p.preds[s.arg()], arr)?
                    {
                        self.one_element(key, arr, i, out)?;
                    }
                }
                Some(s) if s.code() == Code::Descend => match v {
                    RawValue::Document(d) => {
                        name(out, e.ty, key);
                        self.exclude(s.arg(), d, out)?;
                    }
                    RawValue::Array(arr) => {
                        name(out, e.ty, key);
                        self.exclude_array(s.arg(), arr, out)?;
                    }
                    _ => copy(out, bytes, &f),
                },
                _ => copy(out, bytes, &f),
            }
        }
        end(out, at);
        Ok(())
    }

    /// Writes the elements of `arr`, its documents and arrays through block `block`.
    fn exclude_array(&self, block: usize, arr: RawArray<'_>, out: &mut Vec<u8>) -> Result<()> {
        let bytes = arr.as_doc().as_bytes();
        let at = begin(out);
        let mut buf = String::new();
        for (n, f) in fields(bytes, arr.iter()).enumerate() {
            let f = f?;
            let k = index_key(n, &mut buf);
            match f.v {
                RawValue::Document(d) => {
                    name(out, 0x03, k);
                    self.exclude(block, d, out)?;
                }
                RawValue::Array(inner) => {
                    name(out, 0x04, k);
                    self.exclude_array(block, inner, out)?;
                }
                _ => renamed(out, k, bytes, &f.e),
            }
        }
        end(out, at);
        Ok(())
    }

    /// `$slice` of the field's array; anything else is kept.
    fn slice(&self, step: Step, key: &[u8], bytes: &[u8], f: &Field<'_>, out: &mut Vec<u8>) {
        let RawValue::Array(arr) = f.v else { return copy(out, bytes, f) };
        let (skip, limit) = self.p.slices[step.arg()];
        let inner = arr.as_doc().as_bytes();
        let items: Vec<Element> =
            fields(inner, arr.iter()).map_while(|f| f.ok().map(|f| f.e)).collect();
        let n = i64::try_from(items.len()).unwrap_or(i64::MAX);
        let (start, take) = match skip {
            None if limit >= 0 => (0, limit),
            None => ((n + limit).max(0), n),
            Some(s) if s >= 0 => (s.min(n), limit),
            Some(s) => ((n + s).max(0), limit),
        };
        let start = usize::try_from(start).unwrap_or(usize::MAX);
        let take = usize::try_from(take).unwrap_or(usize::MAX);
        name(out, f.e.ty, key);
        let at = begin(out);
        let mut buf = String::new();
        for (i, item) in items.iter().skip(start).take(take).enumerate() {
            renamed(out, index_key(i, &mut buf), inner, item);
        }
        end(out, at);
    }

    /// `a.$`: the matched element of the field's array, or the field itself when it is not one.
    fn positional(&self, key: &[u8], bytes: &[u8], f: &Field<'_>, out: &mut Vec<u8>) -> Result<()> {
        let RawValue::Array(arr) = f.v else {
            copy(out, bytes, f);
            return Ok(());
        };
        let (_, i) = self.element(arr)?;
        self.one_element(key, arr, i, out)
    }

    /// Writes field `key` as an array of the element at `i` of `arr`.
    fn one_element(
        &self,
        key: &[u8],
        arr: RawArray<'_>,
        i: usize,
        out: &mut Vec<u8>,
    ) -> Result<()> {
        let inner = arr.as_doc().as_bytes();
        let f = fields(inner, arr.iter()).nth(i).ok_or_else(mismatch)??;
        name(out, 0x04, key);
        let at = begin(out);
        renamed(out, b"0", inner, &f.e);
        end(out, at);
        Ok(())
    }

    /// The element of `arr` at the position the filter matched, and that position.
    fn element<'v>(&self, arr: RawArray<'v>) -> Result<(RawValue<'v>, usize)> {
        let pos = self.ctx.position.ok_or_else(no_position)?;
        arr.iter().nth(pos).map(|v| (v, pos)).ok_or_else(mismatch)
    }
}
