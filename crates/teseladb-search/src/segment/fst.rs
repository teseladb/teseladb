//! The term dictionary of a field: a minimal acyclic finite-state transducer from terms to `u64`
//! (FR-009).
//!
//! It is built from terms in strictly increasing byte order with the direct construction of
//! minimal acyclic subsequential transducers (Mihov and Maurel, 2001; Daciuk and others,
//! Computational Linguistics 26(1), 2000): the path of the last term stays open, and each node
//! that leaves it is written once and shared with every equal node written before. Outputs are
//! summed along the path and pushed towards the root, so terms that share a prefix share its
//! output.
//!
//! Encoding: each node is written after the nodes it points to, as a flag byte (bit 0: final, bit
//! 1: dense), the final output if final and the number of arcs, as LEB128. A node of fewer than
//! [`DENSE_ARCS`] arcs then has, per arc in label order, the label byte, the output and the
//! target's address, as LEB128. A dense node has its labels in order, one byte each, then the
//! width in bytes of its numbers and, per arc, the output and the target in that width, little
//! endian: a lookup finds the label with one SIMD comparison per 32 labels and goes straight to
//! its arc (FR-029). The trailer is the root's address and the number of terms, eight bytes each
//! in little endian. Every target lies before its node, so a file cannot hold a cycle;
//! [`Fst::new`] checks every node once, and lookups then never fail.
//!
//! A dictionary can carry a table of its most frequent terms in front of the transducer (FR-029):
//! [`Builder::finish_with`] writes it after the nodes, the trailer then starts with the table's
//! address and the number of terms has [`HOT`] set. The table is open addressing with linear
//! probing on the FNV-1a hash of the term: the number of slots (a power of two, at least twice the
//! terms), the number of terms, a `u32` per slot (the term's index plus one, 0 when empty) and per
//! term its value, offset and length, then the bytes of the terms. [`Fst::get`] looks there before
//! walking the transducer.

use std::collections::HashMap;

use crate::segment::Corrupt;
use crate::segment::varint;

const FINAL: u8 = 1;
const DENSE: u8 = 2;
const TRAILER: usize = 16;
/// The arcs from which a node is written dense.
pub const DENSE_ARCS: usize = 16;
/// The bit of the number of terms that says the dictionary has a table of frequent terms.
pub const HOT: u64 = 1 << 63;
/// Bytes of the table's head and of each of its terms.
const HOT_HEAD: usize = 8;
const HOT_ENTRY: usize = 16;
/// A table of frequent terms that does not hold together.
const BAD_TABLE: Corrupt = Corrupt("fst: table of frequent terms");

/// The FNV-1a hash of `term`.
fn hash(term: &[u8]) -> u64 {
    term.iter()
        .fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// The bytes a number of a dense node takes.
fn width_of(v: u64) -> usize {
    (64 - v.leading_zeros() as usize).div_ceil(8).max(1)
}

/// The `width` bytes at `at` as a little-endian number.
fn number(bytes: &[u8], at: usize, width: usize) -> Option<u64> {
    let mut buf = [0u8; 8];
    buf[..width].copy_from_slice(bytes.get(at..at + width)?);
    Some(u64::from_le_bytes(buf))
}

/// An arc of a node still being built: label, output and the target once written.
#[derive(Clone, Debug, Default)]
struct OpenArc {
    label: u8,
    output: u64,
    target: u64,
}

/// A node of the open path.
#[derive(Clone, Debug, Default)]
struct OpenNode {
    arcs: Vec<OpenArc>,
    is_final: bool,
    final_output: u64,
}

impl OpenNode {
    /// Adds `extra` to every output that leaves the node.
    fn prepend(&mut self, extra: u64) {
        for a in &mut self.arcs {
            a.output += extra;
        }
        if self.is_final {
            self.final_output += extra;
        }
    }
}

/// Builds an [`Fst`] from terms in strictly increasing order.
#[derive(Debug, Default)]
pub struct Builder {
    bytes: Vec<u8>,
    /// Written nodes by their encoding: an equal node is written once.
    written: HashMap<Vec<u8>, u64>,
    /// The open path: `open[i]` is the node after the first `i` bytes of the last term.
    open: Vec<OpenNode>,
    last: Option<Vec<u8>>,
    len: u64,
}

impl Builder {
    /// An empty builder.
    pub fn new() -> Builder {
        Builder { open: vec![OpenNode::default()], ..Builder::default() }
    }

    /// Adds `term` with `value`; `term` must follow the previous term in byte order.
    pub fn insert(&mut self, term: &[u8], value: u64) {
        let prefix = match &self.last {
            Some(last) => {
                assert!(term > last.as_slice(), "terms go in strictly increasing order");
                last.iter().zip(term).take_while(|(a, b)| a == b).count()
            }
            None => 0,
        };
        self.close_to(prefix);
        // Push outputs: each arc of the shared prefix keeps what both terms have in common.
        let mut rest = value;
        for i in 0..prefix {
            let arc = self.open[i].arcs.last_mut().expect("the shared prefix has its arcs");
            let common = arc.output.min(rest);
            let extra = arc.output - common;
            arc.output = common;
            rest -= common;
            if extra > 0 {
                self.open[i + 1].prepend(extra);
            }
        }
        for (i, &label) in term.iter().enumerate().skip(prefix) {
            let output = if i == prefix { rest } else { 0 };
            self.open[i].arcs.push(OpenArc { label, output, target: 0 });
            self.open.push(OpenNode::default());
        }
        let end = &mut self.open[term.len()];
        end.is_final = true;
        end.final_output = if term.len() == prefix { rest } else { 0 };
        self.last = Some(term.to_vec());
        self.len += 1;
    }

    /// Writes the dictionary and returns its bytes.
    pub fn finish(self) -> Vec<u8> {
        self.finish_with(&[])
    }

    /// Writes the dictionary with `hot`, terms it holds and their values, in a table in front of
    /// the transducer; with none it is the dictionary of [`Builder::finish`].
    pub fn finish_with(mut self, hot: &[(&[u8], u64)]) -> Vec<u8> {
        self.close_to(0);
        let root = self.open.pop().expect("the root stays open");
        let root = self.write(&root);
        let mut len = self.len;
        if !hot.is_empty() {
            let table = self.bytes.len() as u64;
            write_table(&mut self.bytes, hot);
            self.bytes.extend_from_slice(&table.to_le_bytes());
            len |= HOT;
        }
        self.bytes.extend_from_slice(&root.to_le_bytes());
        self.bytes.extend_from_slice(&len.to_le_bytes());
        self.bytes
    }

    /// Writes the open nodes deeper than `depth`, from the deepest up.
    fn close_to(&mut self, depth: usize) {
        while self.open.len() > depth + 1 {
            let node = self.open.pop().expect("deeper than depth");
            let address = self.write(&node);
            let parent = self.open.last_mut().expect("depth 0 stays open");
            parent.arcs.last_mut().expect("a closed node hangs from an arc").target = address;
        }
    }

    fn write(&mut self, node: &OpenNode) -> u64 {
        let dense = node.arcs.len() >= DENSE_ARCS;
        let mut enc = Vec::with_capacity(2 + node.arcs.len() * 4);
        enc.push(if node.is_final { FINAL } else { 0 } | if dense { DENSE } else { 0 });
        if node.is_final {
            varint::put(&mut enc, node.final_output);
        }
        varint::put(&mut enc, node.arcs.len() as u64);
        if dense {
            let width =
                node.arcs.iter().map(|a| width_of(a.output.max(a.target))).max().unwrap_or(1);
            enc.extend(node.arcs.iter().map(|a| a.label));
            enc.push(width as u8);
            for a in &node.arcs {
                enc.extend_from_slice(&a.output.to_le_bytes()[..width]);
                enc.extend_from_slice(&a.target.to_le_bytes()[..width]);
            }
        } else {
            for a in &node.arcs {
                enc.push(a.label);
                varint::put(&mut enc, a.output);
                varint::put(&mut enc, a.target);
            }
        }
        if let Some(&address) = self.written.get(&enc) {
            return address;
        }
        let address = self.bytes.len() as u64;
        self.bytes.extend_from_slice(&enc);
        self.written.insert(enc, address);
        address
    }
}

/// Appends the table of frequent terms `hot` to `out`.
fn write_table(out: &mut Vec<u8>, hot: &[(&[u8], u64)]) {
    let slots = (hot.len() * 2).next_power_of_two();
    let mut table = vec![0u32; slots];
    for (i, (term, _)) in hot.iter().enumerate() {
        let mut at = hash(term) as usize & (slots - 1);
        while table[at] != 0 {
            at = (at + 1) & (slots - 1);
        }
        table[at] = i as u32 + 1;
    }
    out.extend_from_slice(&(slots as u32).to_le_bytes());
    out.extend_from_slice(&(hot.len() as u32).to_le_bytes());
    for slot in table {
        out.extend_from_slice(&slot.to_le_bytes());
    }
    let mut offset = 0u32;
    for (term, value) in hot {
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        out.extend_from_slice(&(term.len() as u32).to_le_bytes());
        offset += term.len() as u32;
    }
    for (term, _) in hot {
        out.extend_from_slice(term);
    }
}

/// The table of frequent terms of a dictionary.
#[derive(Clone, Copy, Debug)]
struct Hot<'a> {
    bytes: &'a [u8],
    slots: usize,
    terms: usize,
}

impl<'a> Hot<'a> {
    /// The table in `bytes`, its sizes checked.
    fn new(bytes: &'a [u8]) -> Result<Hot<'a>, Corrupt> {
        let word = |at: usize| {
            bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
        };
        let (slots, terms) =
            (word(0).ok_or(BAD_TABLE)? as usize, word(4).ok_or(BAD_TABLE)? as usize);
        if !slots.is_power_of_two() || slots < terms * 2 || terms == 0 {
            return Err(BAD_TABLE);
        }
        let hot = Hot { bytes, slots, terms };
        let text = hot.text_at();
        let mut total = 0usize;
        for i in 0..terms {
            let (_, offset, len) = hot.entry(i).ok_or(BAD_TABLE)?;
            if offset != total {
                return Err(BAD_TABLE);
            }
            total += len;
        }
        if text.checked_add(total) != Some(bytes.len()) {
            return Err(BAD_TABLE);
        }
        let mut used = 0;
        for slot in 0..slots {
            let index = word(HOT_HEAD + slot * 4).ok_or(BAD_TABLE)? as usize;
            if index > terms {
                return Err(BAD_TABLE);
            }
            used += usize::from(index != 0);
        }
        if used != terms {
            return Err(BAD_TABLE);
        }
        Ok(hot)
    }

    /// Where the bytes of the terms start.
    fn text_at(&self) -> usize {
        HOT_HEAD + self.slots * 4 + self.terms * HOT_ENTRY
    }

    /// The value, offset and length of term `i`.
    fn entry(&self, i: usize) -> Option<(u64, usize, usize)> {
        let at = HOT_HEAD + self.slots * 4 + i * HOT_ENTRY;
        let b = self.bytes.get(at..at + HOT_ENTRY)?;
        let value = u64::from_le_bytes(b[..8].try_into().expect("8 bytes"));
        let offset = u32::from_le_bytes(b[8..12].try_into().expect("4 bytes")) as usize;
        let len = u32::from_le_bytes(b[12..].try_into().expect("4 bytes")) as usize;
        Some((value, offset, len))
    }

    /// The term `i`.
    fn term(&self, i: usize) -> Option<&'a [u8]> {
        let (_, offset, len) = self.entry(i)?;
        let at = self.text_at() + offset;
        self.bytes.get(at..at + len)
    }

    /// The value of `term` if the table has it. The probes end at an empty slot: there are at
    /// least twice as many slots as terms.
    fn get(&self, term: &[u8]) -> Option<u64> {
        let mut at = hash(term) as usize & (self.slots - 1);
        loop {
            let b = self.bytes.get(HOT_HEAD + at * 4..HOT_HEAD + at * 4 + 4)?;
            let slot = u32::from_le_bytes(b.try_into().expect("4 bytes")) as usize;
            if slot == 0 {
                return None;
            }
            if self.term(slot - 1)? == term {
                return Some(self.entry(slot - 1)?.0);
            }
            at = (at + 1) & (self.slots - 1);
        }
    }
}

/// A decoded node.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    final_output: Option<u64>,
    /// `(label, output, target)` in label order.
    arcs: Vec<(u8, u64, usize)>,
}

/// A dictionary read from its bytes.
#[derive(Clone, Copy, Debug)]
pub struct Fst<'a> {
    bytes: &'a [u8],
    root: usize,
    len: u64,
    hot: Option<Hot<'a>>,
}

impl<'a> Fst<'a> {
    /// Opens a dictionary, checking every node it holds.
    pub fn new(bytes: &'a [u8]) -> Result<Fst<'a>, Corrupt> {
        let end = bytes.len().checked_sub(TRAILER).ok_or(Corrupt("fst: no trailer"))?;
        let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
        let len = word(end + 8);
        let (body, hot) = if len & HOT != 0 {
            let at = end.checked_sub(8).ok_or(Corrupt("fst: no trailer"))?;
            let table = usize::try_from(word(at)).map_err(|_| Corrupt("fst: table"))?;
            if table > at {
                return Err(Corrupt("fst: table"));
            }
            (table, Some(Hot::new(&bytes[table..at])?))
        } else {
            (end, None)
        };
        let root = usize::try_from(word(end)).map_err(|_| Corrupt("fst: root"))?;
        if root >= body {
            return Err(Corrupt("fst: root out of bounds"));
        }
        let fst = Fst { bytes: &bytes[..body], root, len: len & !HOT, hot: None };
        // Every node from its encoding, in address order: each target must be a node already seen.
        let mut nodes = vec![false; body];
        let mut at = 0;
        while at < body {
            let (node, next) = fst.decode(at).ok_or(Corrupt("fst: node"))?;
            if node.arcs.windows(2).any(|w| w[0].0 >= w[1].0) {
                return Err(Corrupt("fst: arcs out of order"));
            }
            if node.arcs.iter().any(|&(_, _, t)| t >= at || !nodes[t]) {
                return Err(Corrupt("fst: arc target"));
            }
            nodes[at] = true;
            at = next;
        }
        if !nodes[root] {
            return Err(Corrupt("fst: root is not a node"));
        }
        // Every frequent term has the value the transducer gives it.
        if let Some(h) = hot {
            for i in 0..h.terms {
                let term = h.term(i).ok_or(BAD_TABLE)?;
                if fst.walk_get(term) != h.entry(i).map(|e| e.0) || h.get(term).is_none() {
                    return Err(BAD_TABLE);
                }
            }
        }
        Ok(Fst { hot, ..fst })
    }

    /// Number of terms in the table of frequent terms.
    pub fn frequent_terms(&self) -> usize {
        self.hot.map_or(0, |h| h.terms)
    }

    /// Number of terms.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the dictionary has no term.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The value of `term`: from the table of frequent terms when it is there, else by walking
    /// the transducer.
    pub fn get(&self, term: &[u8]) -> Option<u64> {
        if let Some(v) = self.hot.and_then(|h| h.get(term)) {
            return Some(v);
        }
        self.walk_get(term)
    }

    /// The value of `term` by walking the transducer, without decoding its nodes.
    fn walk_get(&self, term: &[u8]) -> Option<u64> {
        let mut at = self.root;
        let mut sum = 0_u64;
        for &b in term {
            let (out, target) = self.arc(at, b)?;
            sum = sum.wrapping_add(out);
            at = target;
        }
        let b = self.bytes;
        if b.get(at)? & FINAL == 0 {
            return None;
        }
        let mut pos = at + 1;
        varint::get(b, &mut pos).map(|f| sum.wrapping_add(f))
    }

    /// The output and target of the arc labelled `label` of the node at `at`.
    fn arc(&self, at: usize, label: u8) -> Option<(u64, usize)> {
        let b = self.bytes;
        let flags = *b.get(at)?;
        let mut pos = at + 1;
        if flags & FINAL != 0 {
            varint::get(b, &mut pos)?;
        }
        let count = usize::try_from(varint::get(b, &mut pos)?).ok()?;
        if flags & DENSE != 0 {
            let labels = b.get(pos..pos + count)?;
            let i = crate::byte_simd::find(labels, label)?;
            let width = usize::from(*b.get(pos + count)?);
            let arc = pos + count + 1 + i * 2 * width;
            let target = usize::try_from(number(b, arc + width, width)?).ok()?;
            return Some((number(b, arc, width)?, target));
        }
        for _ in 0..count {
            let l = *b.get(pos)?;
            pos += 1;
            let output = varint::get(b, &mut pos)?;
            let target = varint::get(b, &mut pos)?;
            if l == label {
                return Some((output, usize::try_from(target).ok()?));
            }
            if l > label {
                return None;
            }
        }
        None
    }

    /// Every term and its value, in order.
    pub fn entries(&self) -> Vec<(Vec<u8>, u64)> {
        self.search(&Everything)
    }

    /// The terms `automaton` accepts, with their values, in order (FR-018).
    pub fn search<A: Automaton>(&self, automaton: &A) -> Vec<(Vec<u8>, u64)> {
        let mut out = Vec::new();
        let mut term = Vec::new();
        self.walk(self.root, automaton.start(), 0, automaton, &mut term, &mut out);
        out
    }

    fn walk<A: Automaton>(
        &self,
        at: usize,
        state: A::State,
        sum: u64,
        automaton: &A,
        term: &mut Vec<u8>,
        out: &mut Vec<(Vec<u8>, u64)>,
    ) {
        let node = self.node(at);
        if let Some(f) = node.final_output
            && automaton.is_match(&state)
        {
            out.push((term.clone(), sum.wrapping_add(f)));
        }
        for (label, output, target) in node.arcs {
            if let Some(next) = automaton.step(&state, label)
                && automaton.can_match(&next)
            {
                term.push(label);
                self.walk(target, next, sum.wrapping_add(output), automaton, term, out);
                term.pop();
            }
        }
    }

    fn node(&self, at: usize) -> Node {
        self.decode(at).expect("checked by Fst::new").0
    }

    /// The node at `at` and the address after it.
    fn decode(&self, at: usize) -> Option<(Node, usize)> {
        let b = self.bytes;
        let flags = *b.get(at)?;
        if flags & !(FINAL | DENSE) != 0 {
            return None;
        }
        let mut pos = at + 1;
        let final_output = if flags & FINAL != 0 { Some(varint::get(b, &mut pos)?) } else { None };
        let count = varint::get(b, &mut pos)?;
        if count > 256 {
            return None;
        }
        let mut arcs = Vec::with_capacity(count as usize);
        if flags & DENSE != 0 {
            let count = count as usize;
            let labels = b.get(pos..pos + count)?;
            let width = usize::from(*b.get(pos + count)?);
            if !(1..=8).contains(&width) {
                return None;
            }
            let mut arc = pos + count + 1;
            for &label in labels {
                let output = number(b, arc, width)?;
                let target = usize::try_from(number(b, arc + width, width)?).ok()?;
                arcs.push((label, output, target));
                arc += 2 * width;
            }
            return Some((Node { final_output, arcs }, arc));
        }
        for _ in 0..count {
            let label = *b.get(pos)?;
            pos += 1;
            let output = varint::get(b, &mut pos)?;
            let target = usize::try_from(varint::get(b, &mut pos)?).ok()?;
            arcs.push((label, output, target));
        }
        Some((Node { final_output, arcs }, pos))
    }
}

/// A deterministic automaton over bytes that [`Fst::search`] runs over the dictionary.
pub trait Automaton {
    /// A state.
    type State;
    /// The start state.
    fn start(&self) -> Self::State;
    /// The state after reading `byte`, or `None` if no term continues that way.
    fn step(&self, state: &Self::State, byte: u8) -> Option<Self::State>;
    /// Whether the bytes read so far form an accepted term.
    fn is_match(&self, state: &Self::State) -> bool;
    /// Whether some continuation could still be accepted; a `false` prunes the subtree.
    fn can_match(&self, state: &Self::State) -> bool;
}

/// Accepts every term.
#[derive(Clone, Copy, Debug)]
pub struct Everything;

impl Automaton for Everything {
    type State = ();

    fn start(&self) {}

    fn step(&self, _: &(), _: u8) -> Option<()> {
        Some(())
    }

    fn is_match(&self, _: &()) -> bool {
        true
    }

    fn can_match(&self, _: &()) -> bool {
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn build(entries: &[(Vec<u8>, u64)]) -> Vec<u8> {
        let mut b = Builder::new();
        for (t, v) in entries {
            b.insert(t, *v);
        }
        b.finish()
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn random_entries(rng: &mut Rng, n: usize, alphabet: u64, max_len: u64) -> Vec<(Vec<u8>, u64)> {
        let mut terms: Vec<Vec<u8>> = (0..n)
            .map(|_| {
                (0..rng.below(max_len + 1)).map(|_| b'a' + rng.below(alphabet) as u8).collect()
            })
            .collect();
        terms.sort();
        terms.dedup();
        terms.into_iter().map(|t| (t, rng.below(1 << 40))).collect()
    }

    #[test]
    fn finds_every_term_and_nothing_else() {
        let mut rng = Rng(0x1234_5678_9abc_def1);
        for round in 0..200 {
            let entries = random_entries(&mut rng, 1 + round * 3, 1 + round as u64 % 6, 8);
            let bytes = build(&entries);
            let fst = Fst::new(&bytes).expect("valid");
            assert_eq!(fst.len(), entries.len() as u64);
            assert!(!fst.is_empty());
            for (t, v) in &entries {
                assert_eq!(fst.get(t), Some(*v), "{t:?}");
            }
            assert_eq!(fst.entries(), entries);
            for _ in 0..50 {
                let probe: Vec<u8> = (0..rng.below(9)).map(|_| b'a' + rng.below(7) as u8).collect();
                let want = entries.iter().find(|(t, _)| *t == probe).map(|e| e.1);
                assert_eq!(fst.get(&probe), want);
            }
        }
    }

    fn build_hot(entries: &[(Vec<u8>, u64)], every: usize) -> Vec<u8> {
        let mut b = Builder::new();
        for (t, v) in entries {
            b.insert(t, *v);
        }
        let hot: Vec<(&[u8], u64)> =
            entries.iter().step_by(every).map(|(t, v)| (t.as_slice(), *v)).collect();
        b.finish_with(&hot)
    }

    #[test]
    fn dense_nodes_and_frequent_terms_answer_like_the_terms() {
        use crate::query::fuzzy::{FuzzyOptions, Levenshtein, fuzzy_terms, prefix_terms};
        use crate::query::regex::Regex;
        let mut rng = Rng(0x0dd_ba11_cafe_f00d);
        for round in 0..40 {
            // 40 letters make nodes of 16 arcs and more, written dense.
            // Moved from `a` up to `0` up, so that every byte is ASCII and every term text.
            let entries: Vec<(Vec<u8>, u64)> = random_entries(&mut rng, 50 + round * 40, 40, 5)
                .into_iter()
                .map(|(t, v)| (t.into_iter().map(|b| b - (b'a' - b'0')).collect(), v))
                .collect();
            let bytes = build_hot(&entries, 1 + round % 5);
            let fst = Fst::new(&bytes).expect("valid");
            assert!(fst.frequent_terms() > 0);
            assert_eq!(fst.len(), entries.len() as u64);
            assert_eq!(fst.entries(), entries);
            for (t, v) in &entries {
                assert_eq!(fst.get(t), Some(*v));
                assert_eq!(fst.walk_get(t), Some(*v));
            }
            for _ in 0..30 {
                let probe: Vec<u8> =
                    (0..rng.below(6)).map(|_| b'0' + rng.below(42) as u8).collect();
                let want = entries.iter().find(|(t, _)| *t == probe).map(|e| e.1);
                assert_eq!(fst.get(&probe), want, "{probe:?}");
                let text = String::from_utf8(probe).expect("ASCII");
                let prefixed: Vec<_> = entries
                    .iter()
                    .filter(|(t, _)| t.starts_with(text.as_bytes()))
                    .cloned()
                    .collect();
                assert_eq!(prefix_terms(&fst, &text), prefixed);
                let lev = Levenshtein::new(&text, 1, 0);
                let near: Vec<&[u8]> = entries
                    .iter()
                    .filter(|(t, _)| lev.distance(std::str::from_utf8(t).expect("ASCII")).is_some())
                    .map(|(t, _)| t.as_slice())
                    .collect();
                let options =
                    FuzzyOptions { max_edits: 1, prefix_length: 0, max_expansions: 1 << 20 };
                let mut got: Vec<Vec<u8>> =
                    fuzzy_terms(&fst, &text, options).into_iter().map(|e| e.term).collect();
                got.sort();
                assert_eq!(got, near);
            }
            let regex = Regex::new("[0-9].*[A-Z]").expect("pattern");
            let matching: Vec<Vec<u8>> = entries
                .iter()
                .filter(|(t, _)| regex.is_full_match(std::str::from_utf8(t).expect("ASCII")))
                .map(|(t, _)| t.clone())
                .collect();
            let got: Vec<Vec<u8>> = fst
                .search(&crate::query::fuzzy::Utf8(&regex))
                .into_iter()
                .map(|(t, _)| t)
                .collect();
            assert_eq!(got, matching);
        }
    }

    #[test]
    fn dense_nodes_are_written_from_sixteen_arcs() {
        let entries: Vec<(Vec<u8>, u64)> =
            (0..DENSE_ARCS as u8).map(|i| (vec![b'a' + i], u64::from(i) * 300)).collect();
        let bytes = build(&entries);
        let fst = Fst::new(&bytes).expect("valid");
        assert_eq!(bytes[fst.root] & DENSE, DENSE);
        assert_eq!(fst.entries(), entries);
        let sparse = build(&entries[1..]);
        let fst = Fst::new(&sparse).expect("valid");
        assert_eq!(sparse[fst.root] & DENSE, 0);
        assert_eq!(width_of(0), 1);
        assert_eq!(width_of(255), 1);
        assert_eq!(width_of(256), 2);
        assert_eq!(width_of(u64::MAX), 8);
    }

    #[test]
    fn damaged_tables_of_frequent_terms_are_rejected() {
        let mut rng = Rng(7);
        let entries = random_entries(&mut rng, 400, 30, 5);
        let bytes = build_hot(&entries, 3);
        assert!(Fst::new(&bytes).is_ok());
        let fst = Fst::new(&bytes).unwrap();
        let table = fst.bytes.len();
        let end = bytes.len() - TRAILER - 8;
        for at in table..end {
            for bit in [0, 3, 7] {
                let mut bad = bytes.clone();
                bad[at] ^= 1 << bit;
                if let Ok(f) = Fst::new(&bad) {
                    // Accepted bytes still give every term its value.
                    for (t, v) in &entries {
                        assert_eq!(f.get(t), Some(*v));
                    }
                }
            }
        }
        let mut bad = bytes.clone();
        bad[end..end + 8].copy_from_slice(&(bytes.len() as u64).to_le_bytes());
        assert_eq!(Fst::new(&bad).unwrap_err(), Corrupt("fst: table"));
        let mut short = bytes[end..].to_vec();
        short[..8].copy_from_slice(&0u64.to_le_bytes());
        assert!(Fst::new(&short).is_err());
        assert!(Fst::new(&bytes[bytes.len() - 16..]).is_err());
    }

    #[test]
    fn empty_term_and_empty_dictionary() {
        let bytes = build(&[(Vec::new(), 7), (b"a".to_vec(), 3)]);
        let fst = Fst::new(&bytes).expect("valid");
        assert_eq!(fst.get(b""), Some(7));
        assert_eq!(fst.get(b"a"), Some(3));
        assert_eq!(fst.get(b"b"), None);
        let bytes = Builder::new().finish();
        let fst = Fst::new(&bytes).expect("valid");
        assert!(fst.is_empty());
        assert_eq!(fst.get(b""), None);
        assert!(fst.entries().is_empty());
    }

    #[test]
    fn shared_suffixes_are_written_once() {
        // 26 prefixes × 1000 shared suffixes with equal values: the suffix tree is written once.
        let mut entries = Vec::new();
        for p in b'a'..=b'z' {
            for s in 0..1000_u32 {
                entries.push((format!("{}{s:04}", p as char).into_bytes(), 5));
            }
        }
        let bytes = build(&entries);
        assert!(bytes.len() < 8_000, "{} bytes", bytes.len());
        let fst = Fst::new(&bytes).expect("valid");
        assert_eq!(fst.get(b"q0999"), Some(5));
        assert_eq!(fst.entries().len(), 26_000);
    }

    #[test]
    fn outputs_are_pushed_and_restored() {
        let entries =
            [(b"ab".to_vec(), 10), (b"abc".to_vec(), 4), (b"abd".to_vec(), 12), (b"b".to_vec(), 0)];
        let bytes = build(&entries);
        assert_eq!(Fst::new(&bytes).expect("valid").entries(), entries.to_vec());
        let max = [(b"x".to_vec(), u64::MAX), (b"y".to_vec(), u64::MAX - 1)];
        assert_eq!(Fst::new(&build(&max)).expect("valid").entries(), max.to_vec());
    }

    #[test]
    #[should_panic(expected = "strictly increasing")]
    fn unsorted_terms_are_rejected() {
        let mut b = Builder::new();
        b.insert(b"b", 1);
        b.insert(b"a", 1);
    }

    struct Prefix(&'static [u8]);

    impl Automaton for Prefix {
        type State = usize;

        fn start(&self) -> usize {
            0
        }

        fn step(&self, s: &usize, byte: u8) -> Option<usize> {
            match self.0.get(*s) {
                Some(&b) if b == byte => Some(s + 1),
                Some(_) => None,
                None => Some(*s),
            }
        }

        fn is_match(&self, s: &usize) -> bool {
            *s == self.0.len()
        }

        fn can_match(&self, _: &usize) -> bool {
            true
        }
    }

    /// Accepts nothing and stops after two bytes.
    struct Shallow;

    impl Automaton for Shallow {
        type State = usize;

        fn start(&self) -> usize {
            0
        }

        fn step(&self, s: &usize, _: u8) -> Option<usize> {
            Some(s + 1)
        }

        fn is_match(&self, _: &usize) -> bool {
            false
        }

        fn can_match(&self, s: &usize) -> bool {
            *s < 2
        }
    }

    #[test]
    fn search_runs_an_automaton_over_the_terms() {
        let entries: Vec<_> = ["car", "card", "care", "cat", "dog"]
            .iter()
            .map(|t| (t.as_bytes().to_vec(), 1))
            .collect();
        let bytes = build(&entries);
        let fst = Fst::new(&bytes).expect("valid");
        let got: Vec<_> = fst.search(&Prefix(b"car")).into_iter().map(|e| e.0).collect();
        assert_eq!(got, vec![b"car".to_vec(), b"card".to_vec(), b"care".to_vec()]);
        // "car" is final but not a match for "card"; a depth limit prunes below two bytes.
        let got: Vec<_> = fst.search(&Prefix(b"card")).into_iter().map(|e| e.0).collect();
        assert_eq!(got, vec![b"card".to_vec()]);
        assert!(fst.search(&Shallow).is_empty());
        let e = Everything;
        e.start();
        assert!(e.step(&(), 0).is_some() && e.can_match(&()));
    }

    #[test]
    fn damaged_bytes_are_rejected_or_read_safely() {
        let mut rng = Rng(99);
        let entries = random_entries(&mut rng, 300, 5, 6);
        let bytes = build(&entries);
        assert!(Fst::new(&bytes[..10]).is_err());
        for _ in 0..3000 {
            let mut bad = bytes.clone();
            let at = rng.below(bad.len() as u64) as usize;
            bad[at] ^= 1 << rng.below(8);
            if let Ok(fst) = Fst::new(&bad) {
                // Accepted bytes always read without panicking.
                let _ = fst.entries();
                let _ = fst.get(&entries[0].0);
            }
        }
    }

    #[test]
    fn hand_made_corruptions_are_named() {
        let trailer = |root: u64, body: &[u8]| {
            let mut v = body.to_vec();
            v.extend_from_slice(&root.to_le_bytes());
            v.extend_from_slice(&1_u64.to_le_bytes());
            v
        };
        assert_eq!(
            Fst::new(&trailer(5, &[1, 0, 0])).unwrap_err(),
            Corrupt("fst: root out of bounds")
        );
        assert!(Fst::new(&trailer(u64::MAX, &[1, 0, 0])).unwrap_err().0.starts_with("fst: root"));
        assert_eq!(Fst::new(&trailer(0, &[4, 0])).unwrap_err(), Corrupt("fst: node"));
        // Arc pointing at itself, arcs out of order, root inside a node, too many arcs.
        assert_eq!(
            Fst::new(&trailer(0, &[0, 1, b'a', 0, 0])).unwrap_err(),
            Corrupt("fst: arc target")
        );
        let two = [1, 0, 0, 0, 2, b'b', 0, 0, b'a', 0, 0];
        assert_eq!(Fst::new(&trailer(3, &two)).unwrap_err(), Corrupt("fst: arcs out of order"));
        assert_eq!(
            Fst::new(&trailer(1, &[1, 0, 0])).unwrap_err(),
            Corrupt("fst: root is not a node")
        );
        let mut many = vec![0];
        varint::put(&mut many, 257);
        assert_eq!(Fst::new(&trailer(0, &many)).unwrap_err(), Corrupt("fst: node"));
        assert_eq!(Fst::new(&trailer(0, &[0, 1, b'a', 0])).unwrap_err(), Corrupt("fst: node"));
        assert_eq!(Fst::new(&trailer(0, &[0, 1])).unwrap_err(), Corrupt("fst: node"));
        assert_eq!(Fst::new(&trailer(0, &[1])).unwrap_err(), Corrupt("fst: node"));
        let mut huge = vec![0, 1, b'a', 0];
        varint::put(&mut huge, u64::MAX);
        let _ = Fst::new(&trailer(0, &huge));
    }
}
