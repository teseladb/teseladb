//! Writing canonical BSON: arrays with their positions as names, regular expression options in
//! alphabetical order and the inner length of subtype 2. What would make invalid bytes is refused:
//! a name or a regular expression with a `00` byte, more bytes or more levels than the limits
//! ([`crate::MAX_SIZE`] and [`crate::MAX_DEPTH`] for a stored document).

use crate::Limits;
use crate::error::{BsonError, invalid};
use crate::raw::{RawArray, RawDoc, RawValue};
use crate::types;
use crate::value::{Document, Value};

impl Document {
    /// The document in canonical BSON, within the limits of a stored document.
    ///
    /// # Errors
    ///
    /// [`BsonError::TooLarge`] or [`BsonError::TooDeep`] past the limits of a stored document, and
    /// [`BsonError::InvalidBson`] for a name or a regular expression with a `00` byte.
    pub fn to_bytes(&self) -> Result<Vec<u8>, BsonError> {
        self.to_bytes_with(Limits::default())
    }

    /// The document in canonical BSON within `limits` (a command reply may exceed a stored
    /// document's).
    ///
    /// # Errors
    ///
    /// As [`Self::to_bytes`], with `limits` in place of a stored document's.
    pub fn to_bytes_with(&self, limits: Limits) -> Result<Vec<u8>, BsonError> {
        // Room for all of it at once: a reply of many documents grown as it is written was
        // copied over and over by its reallocations.
        let mut out = Vec::with_capacity(self.encoded_len().min(limits.max_size.saturating_add(1)));
        self.append_to(&mut out, limits)?;
        Ok(out)
    }

    /// Writes the document in canonical BSON at the end of `out`, within `limits`, as
    /// [`Document::to_bytes_with`] does: a message that carries it (a reply's header, its
    /// sections) is written in one buffer without copying the document again. On an error `out`
    /// may hold part of it.
    ///
    /// # Errors
    ///
    /// As [`Document::to_bytes_with`].
    pub fn append_to(&self, out: &mut Vec<u8>, limits: Limits) -> Result<(), BsonError> {
        append(out, self, limits)
    }

    /// Writes the document as [`Document::append_to`] does at the end of `out`, taking each
    /// document inside it that keeps its bytes ([`Document::from_stored`],
    /// [`Document::from_canonical`]), of at least [`Fragments::new`]'s threshold, by reference:
    /// a reply of stored documents is sent from their own buffers with one `writev`.
    ///
    /// # Errors
    ///
    /// As [`Document::to_bytes_with`].
    pub fn append_fragments<'a>(
        &'a self,
        out: &mut Fragments<'a>,
        limits: Limits,
    ) -> Result<(), BsonError> {
        append(out, self, limits)
    }
}

fn append<'a, S: Sink<'a>>(out: &mut S, d: &'a Document, limits: Limits) -> Result<(), BsonError> {
    let start = out.len();
    write_whole(out, d, 1, limits)?;
    let size = out.len() - start;
    if size > limits.max_size {
        return Err(BsonError::TooLarge { size, max: limits.max_size });
    }
    Ok(())
}

/// Where the writer puts bytes: a buffer, or [`Fragments`].
trait Sink<'a> {
    /// Bytes written so far.
    fn len(&self) -> usize;
    fn put(&mut self, bytes: &[u8]);
    fn byte(&mut self, b: u8);
    /// Room for an `i32` length, and where it is.
    fn placeholder(&mut self) -> Mark;
    /// Writes into the room at `at` the length from there up to the end.
    fn patch(&mut self, at: Mark, limits: Limits) -> Result<(), BsonError>;
    /// The bytes of a document that keeps them.
    fn kept(&mut self, bytes: &'a [u8]);
}

/// The room for a length: its place among the bytes written by the sink itself, and among all
/// the bytes.
#[derive(Clone, Copy)]
struct Mark {
    own: usize,
    at: usize,
}

impl Sink<'_> for Vec<u8> {
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn put(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
    fn byte(&mut self, b: u8) {
        self.push(b);
    }
    fn placeholder(&mut self) -> Mark {
        let at = self.len();
        self.extend_from_slice(&[0; 4]);
        Mark { own: at, at }
    }
    fn patch(&mut self, m: Mark, limits: Limits) -> Result<(), BsonError> {
        let len = within(self.len() - m.at, limits)?;
        self[m.own..m.own + 4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
    fn kept(&mut self, bytes: &[u8]) {
        self.extend_from_slice(bytes);
    }
}

/// A message written in pieces: the bytes written here, with the bytes of the documents that
/// keep them spliced in by reference where they go. [`Fragments::slices`] gives them in order,
/// for a vectored write.
#[derive(Debug, Default)]
pub struct Fragments<'a> {
    own: Vec<u8>,
    /// Each borrowed piece, after the first `.0` bytes of `own`.
    splices: Vec<(usize, &'a [u8])>,
    borrowed: usize,
    min: usize,
}

impl<'a> Fragments<'a> {
    /// No bytes yet; a document that keeps at least `min_borrowed` bytes goes by reference, a
    /// smaller one is copied (a piece of a vectored write costs more than copying a few bytes).
    #[must_use]
    pub fn new(min_borrowed: usize) -> Fragments<'a> {
        Fragments { own: Vec::new(), splices: Vec::new(), borrowed: 0, min: min_borrowed.max(1) }
    }

    /// All the bytes, own and borrowed.
    #[must_use]
    pub fn len(&self) -> usize {
        self.own.len() + self.borrowed
    }

    /// Whether there are no bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The bytes taken by reference.
    #[must_use]
    pub fn borrowed(&self) -> usize {
        self.borrowed
    }

    /// Appends `bytes`, copied.
    pub fn extend(&mut self, bytes: &[u8]) {
        self.own.extend_from_slice(bytes);
    }

    /// Writes `bytes` over the own bytes that start at `at` (counted over all the bytes), which
    /// have to be written already and come before every borrowed piece: a header's length, once
    /// the message is written.
    ///
    /// # Panics
    ///
    /// If those bytes are not own bytes before the first borrowed piece.
    pub fn overwrite(&mut self, at: usize, bytes: &[u8]) {
        let first = self.splices.first().map_or(self.own.len(), |s| s.0);
        assert!(at + bytes.len() <= first, "only the bytes before the first borrowed piece");
        self.own[at..at + bytes.len()].copy_from_slice(bytes);
    }

    /// The pieces in order.
    #[must_use]
    pub fn slices(&self) -> Vec<&[u8]> {
        let mut out = Vec::with_capacity(2 * self.splices.len() + 1);
        let mut from = 0;
        for &(at, piece) in &self.splices {
            if at > from {
                out.push(&self.own[from..at]);
            }
            out.push(piece);
            from = at;
        }
        if from < self.own.len() || out.is_empty() {
            out.push(&self.own[from..]);
        }
        out
    }

    /// All the bytes in one buffer.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        self.slices().concat()
    }

    /// All the bytes in one buffer, without a copy when nothing was borrowed.
    #[must_use]
    pub fn into_vec(self) -> Vec<u8> {
        if self.splices.is_empty() { self.own } else { self.to_vec() }
    }
}

impl<'a> Sink<'a> for Fragments<'a> {
    fn len(&self) -> usize {
        Fragments::len(self)
    }
    fn put(&mut self, bytes: &[u8]) {
        self.own.extend_from_slice(bytes);
    }
    fn byte(&mut self, b: u8) {
        self.own.push(b);
    }
    fn placeholder(&mut self) -> Mark {
        let m = Mark { own: self.own.len(), at: self.len() };
        self.own.extend_from_slice(&[0; 4]);
        m
    }
    fn patch(&mut self, m: Mark, limits: Limits) -> Result<(), BsonError> {
        let len = within(self.len() - m.at, limits)?;
        self.own[m.own..m.own + 4].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }
    fn kept(&mut self, bytes: &'a [u8]) {
        if bytes.len() < self.min {
            self.own.extend_from_slice(bytes);
        } else {
            self.splices.push((self.own.len(), bytes));
            self.borrowed += bytes.len();
        }
    }
}

impl Document {
    /// The bytes the document takes written as BSON, without writing it: its length, each
    /// element and the closing zero. A document that keeps its canonical bytes is as long as
    /// they are.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        if let Some(b) = self.canonical_bytes() {
            return b.len();
        }
        5 + self.iter().map(|(k, v)| element_len(k.len(), v)).sum::<usize>()
    }
}

/// An element: its type, its name with its zero and its value.
fn element_len(name: usize, v: &Value) -> usize {
    2 + name + value_len(v)
}

/// A string written with its length and its zero.
fn string_len(s: &str) -> usize {
    5 + s.len()
}

/// The decimal digits of `i`, the name of an array's element `i`.
fn digits(i: usize) -> usize {
    i.checked_ilog10().map_or(1, |d| usize::try_from(d).unwrap_or(0) + 1)
}

fn value_len(v: &Value) -> usize {
    match v {
        Value::Double(_) | Value::DateTime(_) | Value::Timestamp { .. } | Value::Int64(_) => 8,
        Value::String(s) | Value::JavaScript(s) | Value::Symbol(s) => string_len(s),
        Value::Document(d) => d.encoded_len(),
        Value::Array(items) => {
            5 + items.iter().enumerate().map(|(i, x)| element_len(digits(i), x)).sum::<usize>()
        }
        Value::Binary { subtype, bytes } => {
            5 + bytes.len() + if *subtype == types::BINARY_OLD { 4 } else { 0 }
        }
        Value::Undefined | Value::Null | Value::MinKey | Value::MaxKey => 0,
        Value::ObjectId(_) => 12,
        Value::Boolean(_) => 1,
        Value::Regex { pattern, options } => pattern.len() + options.len() + 2,
        Value::DbPointer { namespace, .. } => string_len(namespace) + 12,
        Value::JavaScriptWithScope { code, scope } => 4 + string_len(code) + scope.encoded_len(),
        Value::Int32(_) => 4,
        Value::Decimal128(_) => 16,
    }
}

/// Refuses a string or binary value of `len` bytes past the limit before it is written, so that
/// every length written fits in BSON's `i32`.
fn within(len: usize, limits: Limits) -> Result<i32, BsonError> {
    match i32::try_from(len) {
        Ok(n) if len <= limits.max_size => Ok(n),
        _ => Err(BsonError::TooLarge { size: len, max: limits.max_size }),
    }
}

/// Document `d` at nesting level `depth`: the bytes it was read from while it is unchanged
/// ([`Document::from_canonical`]), its fields otherwise.
fn write_whole<'a, S: Sink<'a>>(
    out: &mut S,
    d: &'a Document,
    depth: usize,
    limits: Limits,
) -> Result<(), BsonError> {
    let Some(raw) = d.canonical_bytes() else {
        return write_document(out, d.iter(), depth, limits);
    };
    // A canonical document has at most `MAX_COMMAND_DEPTH` levels: its own are counted only
    // when that many could pass the limit.
    let deepest = |n: usize| depth - 1 + n > limits.max_depth;
    if deepest(crate::MAX_COMMAND_DEPTH) && deepest(levels(RawDoc::trusted(raw))) {
        return Err(BsonError::TooDeep { offset: out.len(), path: String::new() });
    }
    within(raw.len(), limits)?;
    out.kept(raw);
    Ok(())
}

/// Levels of `raw`, itself the first: documents, arrays and the scope of JavaScript code.
pub(crate) fn levels(raw: RawDoc<'_>) -> usize {
    let inner = raw.iter().map(|(_, v)| match v {
        RawValue::Document(d)
        | RawValue::Array(RawArray(d))
        | RawValue::JavaScriptWithScope { scope: d, .. } => levels(d),
        _ => 0,
    });
    1 + inner.max().unwrap_or(0)
}

/// A document of `fields` at nesting level `depth`.
fn write_document<'a, S: Sink<'a>, K: AsRef<str>>(
    out: &mut S,
    fields: impl Iterator<Item = (K, &'a Value)>,
    depth: usize,
    limits: Limits,
) -> Result<(), BsonError> {
    if depth > limits.max_depth {
        return Err(BsonError::TooDeep { offset: out.len(), path: String::new() });
    }
    let start = out.placeholder();
    for (key, value) in fields {
        let key = key.as_ref();
        out.byte(value.element_type());
        cstring(out, key)?;
        write_value(out, value, depth, limits).map_err(|e| e.under(key))?;
    }
    out.byte(0);
    out.patch(start, limits)
}

fn write_value<'a, S: Sink<'a>>(
    out: &mut S,
    value: &'a Value,
    depth: usize,
    limits: Limits,
) -> Result<(), BsonError> {
    match value {
        Value::Double(x) => out.put(&x.to_le_bytes()),
        Value::String(s) | Value::JavaScript(s) | Value::Symbol(s) => string(out, s, limits)?,
        Value::Document(d) => write_whole(out, d, depth + 1, limits)?,
        Value::Array(items) => {
            let names: Vec<String> = (0..items.len()).map(|i| i.to_string()).collect();
            write_document(out, names.iter().zip(items), depth + 1, limits)?;
        }
        Value::Binary { subtype, bytes } => {
            let old = *subtype == types::BINARY_OLD;
            let len = within(bytes.len() + if old { 4 } else { 0 }, limits)?;
            out.put(&len.to_le_bytes());
            out.byte(*subtype);
            if old {
                out.put(&(len - 4).to_le_bytes());
            }
            out.put(bytes);
        }
        Value::Undefined | Value::Null | Value::MinKey | Value::MaxKey => {}
        Value::ObjectId(id) => out.put(id),
        Value::Boolean(b) => out.byte(u8::from(*b)),
        Value::DateTime(ms) => out.put(&ms.to_le_bytes()),
        Value::Regex { pattern, options } => {
            cstring(out, pattern)?;
            let mut sorted: Vec<char> = options.chars().collect();
            sorted.sort_unstable();
            cstring(out, &sorted.into_iter().collect::<String>())?;
        }
        Value::DbPointer { namespace, id } => {
            string(out, namespace, limits)?;
            out.put(id);
        }
        Value::JavaScriptWithScope { code, scope } => {
            let start = out.placeholder();
            string(out, code, limits)?;
            write_document(out, scope.iter(), depth + 1, limits)?;
            out.patch(start, limits)?;
        }
        Value::Int32(n) => out.put(&n.to_le_bytes()),
        Value::Timestamp { time, increment } => {
            out.put(&increment.to_le_bytes());
            out.put(&time.to_le_bytes());
        }
        Value::Int64(n) => out.put(&n.to_le_bytes()),
        Value::Decimal128(d) => out.put(&d.to_le_bytes()),
    }
    Ok(())
}

/// A name or a regular expression part: UTF-8 without `00`, then `00`.
fn cstring<'a, S: Sink<'a>>(out: &mut S, s: &str) -> Result<(), BsonError> {
    if s.as_bytes().contains(&0) {
        return Err(invalid(out.len(), "name or regular expression with a 00 byte"));
    }
    out.put(s.as_bytes());
    out.byte(0);
    Ok(())
}

/// A string: length counting its terminator, UTF-8 bytes (`00` allowed inside) and `00`.
fn string<'a, S: Sink<'a>>(out: &mut S, s: &str, limits: Limits) -> Result<(), BsonError> {
    out.put(&within(s.len() + 1, limits)?.to_le_bytes());
    out.put(s.as_bytes());
    out.byte(0);
    Ok(())
}
