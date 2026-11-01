//! The dictionary tokenizers of `reflib.kuromoji` (Japanese, IPADIC 2.7.0-20070801) and
//! `reflib.nori` (Korean, mecab-ko-dic 2.1.1-20180720), as `refsearch` 1.70.6 runs them (011 T009,
//! D-109). Both find the cheapest path through a lattice of dictionary words: each word has a
//! cost, and each pair of neighbouring words the connection cost of the right context id of the
//! first and the left context id of the second. A character no dictionary word starts at, or
//! whose class always does, also starts an unknown word of its class, grouped with the following
//! characters of the same class when the class groups.
//!
//! Japanese runs in search mode: on the best path a word longer than two kanji, or longer than
//! seven characters, is weighed again with a penalty per extra character, and the path through
//! its parts wins when it costs no more. Korean adds a penalty to an ending, particle, copula
//! or suffix that follows a space, and splits compound, inflected and pre-analysed words into
//! their morphemes. Both drop punctuation and work on UTF-16 code units, as Java does.
//!
//! The dictionaries are `data/kuromoji.bin` and `data/nori.bin`, written by
//! `scripts/gen-morph-dicts.py`, whose header describes the layout. Measured equal to `refsearch`
//! on 12 000 sentences of each language (`tests/fixtures/refsearch/analyzers/sentences/`).

use std::sync::OnceLock;

use crate::analysis::props;

/// The dictionary of one language.
pub struct Dictionary {
    data: Vec<u8>,
    strings: usize,
    string_count: usize,
    string_bytes: usize,
    surface_offsets: usize,
    surface_count: usize,
    surface_units: usize,
    first_entry: usize,
    entries: usize,
    right_ids: usize,
    left_ids: usize,
    matrix: usize,
    classes: usize,
    flags: Vec<u8>,
    unknown: Vec<(usize, usize)>,
}

/// The size of an entry record.
const RECORD: usize = 16;

/// A word of the dictionary or an unknown word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Left context id.
    pub left: u16,
    /// Right context id.
    pub right: u16,
    /// Word cost.
    pub cost: i16,
    /// The part-of-speech string.
    pub tag: u32,
    /// The extra string (0 when there is none).
    pub extra: u32,
}

/// A reader over the little-endian layout.
struct Cursor<'a> {
    data: &'a [u8],
    at: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Option<usize> {
        let at = self.at;
        self.at = at.checked_add(n).filter(|&e| e <= self.data.len())?;
        Some(at)
    }

    fn u8(&mut self) -> Option<u8> {
        let at = self.take(1)?;
        Some(self.data[at])
    }

    fn u16(&mut self) -> Option<u16> {
        let at = self.take(2)?;
        Some(u16::from_le_bytes([self.data[at], self.data[at + 1]]))
    }

    fn u32(&mut self) -> Option<usize> {
        let at = self.take(4)?;
        let b = &self.data[at..at + 4];
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
}

impl Dictionary {
    /// Reads a dictionary from its gzip file.
    fn parse(gz: &[u8]) -> Option<Dictionary> {
        let data = gzip_decompress(gz)?;
        let mut c = Cursor { data: &data, at: 0 };
        let magic = c.take(8)?;
        if &data[magic..magic + 8] != b"TSLMOR01" {
            return None;
        }
        let string_count = c.u32()?;
        let strings = c.take(4 * (string_count + 1))?;
        let string_bytes = c.at;
        let total = read_u32(&data, strings + 4 * string_count);
        c.take(total)?;
        let surface_count = c.u32()?;
        let surface_offsets = c.take(4 * (surface_count + 1))?;
        let surface_units = c.at;
        let units = read_u32(&data, surface_offsets + 4 * surface_count);
        c.take(2 * units)?;
        let first_entry = c.take(4 * (surface_count + 1))?;
        let entry_count = c.u32()?;
        let entries = c.take(RECORD * entry_count)?;
        let right_ids = usize::from(c.u16()?);
        let left_ids = usize::from(c.u16()?);
        let matrix = c.take(2 * right_ids * left_ids)?;
        let classes = c.take(0x10000)?;
        let class_count = usize::from(c.u8()?);
        let flags_at = c.take(class_count)?;
        let flags = data[flags_at..flags_at + class_count].to_vec();
        let unknown_count = usize::from(c.u8()?);
        let mut unknown = Vec::with_capacity(unknown_count);
        for _ in 0..unknown_count {
            let n = c.u32()?;
            unknown.push((c.take(RECORD * n)?, n));
        }
        Some(Dictionary {
            strings,
            string_count,
            string_bytes,
            surface_offsets,
            surface_count,
            surface_units,
            first_entry,
            entries,
            right_ids,
            left_ids,
            matrix,
            classes,
            flags,
            unknown,
            data,
        })
    }

    /// The Japanese dictionary.
    pub fn japanese() -> Option<&'static Dictionary> {
        static DICT: OnceLock<Option<Dictionary>> = OnceLock::new();
        DICT.get_or_init(|| Dictionary::parse(include_bytes!("../../data/kuromoji.bin"))).as_ref()
    }

    /// The Korean dictionary.
    pub fn korean() -> Option<&'static Dictionary> {
        static DICT: OnceLock<Option<Dictionary>> = OnceLock::new();
        DICT.get_or_init(|| Dictionary::parse(include_bytes!("../../data/nori.bin"))).as_ref()
    }

    /// String `i`.
    pub fn string(&self, i: u32) -> &str {
        let i = (i as usize).min(self.string_count);
        let from = read_u32(&self.data, self.strings + 4 * i);
        let to = read_u32(&self.data, self.strings + 4 * (i + 1).min(self.string_count));
        std::str::from_utf8(&self.data[self.string_bytes + from..self.string_bytes + to])
            .unwrap_or("")
    }

    fn surface(&self, i: usize) -> impl Iterator<Item = u16> + '_ {
        let from = read_u32(&self.data, self.surface_offsets + 4 * i);
        let to = read_u32(&self.data, self.surface_offsets + 4 * (i + 1));
        (from..to).map(move |u| read_u16(&self.data, self.surface_units + 2 * u))
    }

    fn record(&self, at: usize) -> Entry {
        Entry {
            left: read_u16(&self.data, at),
            right: read_u16(&self.data, at + 2),
            cost: read_u16(&self.data, at + 4) as i16,
            tag: read_u32(&self.data, at + 8) as u32,
            extra: read_u32(&self.data, at + 12) as u32,
        }
    }

    /// The entries of surface `i`.
    fn entries_of(&self, i: usize) -> impl Iterator<Item = Entry> + '_ {
        let from = read_u32(&self.data, self.first_entry + 4 * i);
        let to = read_u32(&self.data, self.first_entry + 4 * (i + 1));
        (from..to).map(move |e| self.record(self.entries + RECORD * e))
    }

    /// The unknown-word entries of a character class.
    fn unknown_of(&self, class: u8) -> impl Iterator<Item = Entry> + '_ {
        let (at, n) = self.unknown.get(usize::from(class)).copied().unwrap_or((0, 0));
        (0..n).map(move |e| self.record(at + RECORD * e))
    }

    /// The connection cost from right context `right` to left context `left`.
    fn connection(&self, right: u16, left: u16) -> i32 {
        let (r, l) = (usize::from(right), usize::from(left));
        if r >= self.right_ids || l >= self.left_ids {
            return 0;
        }
        i32::from(read_u16(&self.data, self.matrix + 2 * (r * self.left_ids + l)) as i16)
    }

    /// The character class of a UTF-16 code unit.
    fn class(&self, unit: u16) -> u8 {
        self.data[self.classes + usize::from(unit)]
    }

    fn invokes(&self, class: u8) -> bool {
        self.flags.get(usize::from(class)).is_some_and(|f| f & 1 != 0)
    }

    fn groups(&self, class: u8) -> bool {
        self.flags.get(usize::from(class)).is_some_and(|f| f & 2 != 0)
    }

    /// Calls `found` with the end and the surface index of each dictionary word that starts at
    /// `pos`, shortest first.
    fn words_at(&self, text: &[u16], pos: usize, mut found: impl FnMut(usize, usize)) {
        let (mut lo, mut hi) = (0, self.surface_count);
        for len in 1..=text.len() - pos {
            let key = &text[pos..pos + len];
            // The surfaces in [lo, hi) start with key[..len - 1]; narrow them to key.
            lo += self.partition(lo, hi, |s| prefix_cmp(s, key) == std::cmp::Ordering::Less);
            hi = lo + self.partition(lo, hi, |s| prefix_cmp(s, key) != std::cmp::Ordering::Greater);
            if lo >= hi {
                return;
            }
            if self.surface(lo).count() == len {
                found(pos + len, lo);
            }
        }
    }

    /// The number of surfaces in `[lo, hi)` for which `before` holds, all of which come first.
    fn partition(
        &self,
        lo: usize,
        hi: usize,
        before: impl Fn(&mut dyn Iterator<Item = u16>) -> bool,
    ) -> usize {
        let (mut a, mut b) = (lo, hi);
        while a < b {
            let mid = a + (b - a) / 2;
            if before(&mut self.surface(mid)) { a = mid + 1 } else { b = mid }
        }
        a - lo
    }
}

/// How surface `s` compares with `key` on its first `key.len()` units (a shorter surface that
/// is a prefix of `key` compares less).
fn prefix_cmp(s: &mut dyn Iterator<Item = u16>, key: &[u16]) -> std::cmp::Ordering {
    for &k in key {
        match s.next() {
            None => return std::cmp::Ordering::Less,
            Some(u) if u != k => return u.cmp(&k),
            Some(_) => {}
        }
    }
    std::cmp::Ordering::Equal
}

fn read_u16(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

fn read_u32(data: &[u8], at: usize) -> usize {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize
}

/// Inflates the gzip file of a dictionary.
fn gzip_decompress(gz: &[u8]) -> Option<Vec<u8>> {
    teseladb_deflate::gzip::decompress(gz).ok()
}

/// The general category of a UTF-16 code unit, as an index of [`props::CATEGORIES`]; a
/// surrogate is `Cs`.
fn unit_category(unit: u16) -> usize {
    char::from_u32(u32::from(unit)).map_or(28, props::category)
}

/// Whether a code unit is punctuation as the tokenizers drop it: separators, controls, format
/// characters, punctuation and symbols.
fn punctuation(unit: u16) -> bool {
    matches!(unit_category(unit), 12..=27)
}

/// Whether a code unit is punctuation to the Korean tokenizer: the same, plus the Hangul
/// middle dot U+318D.
fn punctuation_ko(unit: u16) -> bool {
    unit == 0x318D || punctuation(unit)
}

/// Where a word came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Known,
    Unknown,
}

/// A way to arrive at a position: its cost, the right context id of its last word, where that
/// word starts (and, in Korean, where it starts after a skipped space), the arrival it extends
/// and the word.
#[derive(Clone, Copy, Debug)]
struct Arrival {
    cost: i32,
    right: u16,
    back: usize,
    word_start: usize,
    back_index: usize,
    entry: Entry,
    source: Source,
}

/// A word to weigh again from a position, in search mode.
#[derive(Clone, Copy, Debug)]
struct Forward {
    to: usize,
    entry: Entry,
    source: Source,
}

/// A word of the best path: its UTF-16 range and entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Word {
    /// First code unit.
    pub start: usize,
    /// End code unit.
    pub end: usize,
    /// The entry.
    pub entry: Entry,
}

/// The lattice and its state while tokenizing one text.
struct Lattice<'a> {
    dict: &'a Dictionary,
    text: &'a [u16],
    positions: Vec<Vec<Arrival>>,
    forwards: Vec<Vec<Forward>>,
    next: usize,
    last: usize,
    out: Vec<Word>,
}

impl<'a> Lattice<'a> {
    fn new(dict: &'a Dictionary, text: &'a [u16]) -> Lattice<'a> {
        let mut positions = vec![Vec::new(); text.len() + 1];
        positions[0].push(Arrival {
            cost: 0,
            right: 0,
            back: 0,
            word_start: 0,
            back_index: 0,
            entry: Entry { left: 0, right: 0, cost: 0, tag: 0, extra: 0 },
            source: Source::Known,
        });
        Lattice {
            dict,
            text,
            positions,
            forwards: vec![Vec::new(); text.len() + 1],
            next: 1,
            last: 0,
            out: Vec::new(),
        }
    }

    /// Marks position `p` as reached, as RefLib's position array does on each access.
    fn touch(&mut self, p: usize) {
        self.next = self.next.max(p + 1);
    }

    /// Adds `entry` from position `from` (starting at `word_start`) to `end`, after the
    /// cheapest arrival at `from`; `extra` gives the penalty of each arrival.
    fn add(
        &mut self,
        from: usize,
        word_start: usize,
        end: usize,
        entry: Entry,
        source: Source,
        penalty: i32,
        extra: impl Fn(&Arrival) -> i32,
    ) {
        let mut least: Option<(i32, usize)> = None;
        for (i, a) in self.positions[from].iter().enumerate() {
            let cost = a.cost + self.dict.connection(a.right, entry.left) + extra(a);
            if least.is_none_or(|(c, _)| cost < c) {
                least = Some((cost, i));
            }
        }
        let Some((cost, index)) = least else { return };
        self.touch(end);
        self.positions[end].push(Arrival {
            cost: cost + i32::from(entry.cost) + penalty,
            right: entry.right,
            back: from,
            word_start,
            back_index: index,
            entry,
            source,
        });
    }

    /// The length of the unknown word of `class` at `pos`, Japanese style: the following units
    /// of the same class and punctuation kind, when the class groups.
    fn unknown_length_ja(&self, pos: usize, class: u8) -> usize {
        if !self.dict.groups(class) {
            return 1;
        }
        let punct = punctuation(self.text[pos]);
        let mut len = 1;
        while pos + len < self.text.len()
            && len < MAX_UNKNOWN
            && self.dict.class(self.text[pos + len]) == class
            && punctuation(self.text[pos + len]) == punct
        {
            len += 1;
        }
        len
    }

    /// The cheapest arrival at the end of the text, with the connection to the end.
    fn final_index(&self) -> usize {
        let mut least: Option<(i32, usize)> = None;
        for (i, a) in self.positions[self.text.len()].iter().enumerate() {
            let cost = a.cost + self.dict.connection(a.right, 0);
            if least.is_none_or(|(c, _)| cost < c) {
                least = Some((cost, i));
            }
        }
        least.map_or(0, |(_, i)| i)
    }
}

/// The longest unknown word.
const MAX_UNKNOWN: usize = 1024;

/// Kanji words longer than this pay a penalty in search mode.
const KANJI_LENGTH: usize = 2;
/// The penalty per extra kanji.
const KANJI_PENALTY: i32 = 3000;
/// Other words longer than this pay a penalty in search mode.
const OTHER_LENGTH: usize = 7;
/// The penalty per extra character.
const OTHER_PENALTY: i32 = 1700;

/// The Japanese character classes of kanji (KANJI and KANJINUMERIC).
const KANJI_CLASSES: [u8; 2] = [10, 11];

impl Lattice<'_> {
    /// The search-mode penalty of the word `pos..pos + len`.
    fn penalty(&self, pos: usize, len: usize) -> i32 {
        if len <= KANJI_LENGTH {
            return 0;
        }
        let kanji =
            self.text[pos..pos + len].iter().all(|&u| KANJI_CLASSES.contains(&self.dict.class(u)));
        if kanji {
            (len - KANJI_LENGTH) as i32 * KANJI_PENALTY
        } else if len > OTHER_LENGTH {
            (len - OTHER_LENGTH) as i32 * OTHER_PENALTY
        } else {
            0
        }
    }

    /// Weighs again the words between `start` and `end` with the search-mode penalties, from
    /// arrival `best_start` at `start`.
    fn rescore(&mut self, start: usize, end: usize, best_start: usize) {
        for p in (start + 1..=end).rev() {
            let arrivals = std::mem::take(&mut self.positions[p]);
            for a in &arrivals {
                if a.back >= start {
                    self.forwards[a.back].push(Forward { to: p, entry: a.entry, source: a.source });
                }
            }
        }
        for p in start..end {
            let forwards = std::mem::take(&mut self.forwards[p]);
            if self.positions[p].is_empty() {
                continue;
            }
            if p == start {
                let from = self.positions[p][best_start];
                let right = if start == 0 { 0 } else { from.entry.right };
                for f in forwards {
                    let cost = from.cost
                        + i32::from(f.entry.cost)
                        + self.dict.connection(right, f.entry.left)
                        + self.penalty(p, f.to - p);
                    self.touch(f.to);
                    self.positions[f.to].push(Arrival {
                        cost,
                        right: f.entry.right,
                        back: p,
                        word_start: p,
                        back_index: best_start,
                        entry: f.entry,
                        source: f.source,
                    });
                }
            } else {
                for f in forwards {
                    let pen = self.penalty(p, f.to - p);
                    self.add(p, p, f.to, f.entry, f.source, pen, |_| 0);
                }
            }
        }
    }

    /// Follows the best path back from `index` at `end` to the last backtrace, in search mode.
    fn backtrace_ja(&mut self, end: usize, index: usize) {
        if end == self.last {
            return;
        }
        let mut pos = end;
        let mut best = index;
        // Where the compound replaced by its parts starts, while the path is inside it.
        let mut alt: Option<usize> = None;
        let mut last_left: Option<u16> = None;
        let mut words = Vec::new();
        while pos > self.last {
            let mut a = self.positions[pos][best];
            if alt.is_none() {
                let pen = self.penalty(a.back, pos - a.back);
                if pen > 0 {
                    let mut max = a.cost + pen;
                    if let Some(l) = last_left {
                        max += self.dict.connection(a.entry.right, l);
                    }
                    self.rescore(a.back, pos, a.back_index);
                    let mut least: Option<(i32, usize)> = None;
                    for (i, b) in self.positions[pos].iter().enumerate() {
                        let cost = b.cost
                            + last_left.map_or(0, |l| self.dict.connection(b.entry.right, l));
                        if least.is_none_or(|(c, _)| cost < c) {
                            least = Some((cost, i));
                        }
                    }
                    if let Some((cost, i)) = least
                        && cost <= max
                        && self.positions[pos][i].back != a.back
                    {
                        alt = Some(a.back);
                        a = self.positions[pos][i];
                    }
                }
            }
            if alt.is_some_and(|start| start >= a.back) {
                alt = None;
            }
            if !punctuation(self.text[a.back]) {
                words.push(Word { start: a.back, end: pos, entry: a.entry });
            }
            last_left = Some(a.entry.left);
            pos = a.back;
            best = a.back_index;
        }
        self.last = end;
        words.reverse();
        self.out.extend(words);
    }
}

impl Lattice<'_> {
    /// Japanese: the words of the best path in search mode.
    fn run_ja(mut self) -> Vec<Word> {
        let n = self.text.len();
        let mut pos = 0;
        while pos < n {
            self.touch(pos);
            let frontier = self.next == pos + 1;
            if self.positions[pos].is_empty() {
                pos += 1;
                continue;
            }
            if pos > self.last && self.positions[pos].len() == 1 && frontier {
                self.backtrace_ja(pos, 0);
                self.positions[pos][0].cost = 0;
            }
            let mut known = Vec::new();
            self.dict.words_at(self.text, pos, |end, surface| known.push((end, surface)));
            let any = !known.is_empty();
            for (end, surface) in known {
                let entries: Vec<Entry> = self.dict.entries_of(surface).collect();
                for e in entries {
                    self.add(pos, pos, end, e, Source::Known, 0, |_| 0);
                }
            }
            let class = self.dict.class(self.text[pos]);
            if !any || self.dict.invokes(class) {
                let len = self.unknown_length_ja(pos, class);
                let entries: Vec<Entry> = self.dict.unknown_of(class).collect();
                for e in entries {
                    self.add(pos, pos, pos + len, e, Source::Unknown, 0, |_| 0);
                }
            }
            pos += 1;
        }
        if n > 0 {
            self.touch(n);
            let index = self.final_index();
            self.backtrace_ja(n, index);
        }
        self.out
    }
}

/// The Korean tags that pay a penalty after a space: endings, particles, the copula and the
/// suffixes.
const SPACE_PENALTY_TAGS: [&str; 6] = ["E", "J", "VCP", "XSA", "XSN", "XSV"];
/// The penalty they pay.
const SPACE_PENALTY: i32 = 3000;

/// The script of a UTF-16 code unit as an index of [`props::script_names`]; `None` for
/// surrogates and code points without a script.
fn unit_script(unit: u16) -> Option<usize> {
    char::from_u32(u32::from(unit)).and_then(props::script)
}

/// Whether a script index is Common or Inherited (Unknown, `None`, is neither).
fn common_or_inherited(script: Option<usize>) -> bool {
    static INDICES: OnceLock<(Option<usize>, Option<usize>)> = OnceLock::new();
    let (common, inherited) = *INDICES.get_or_init(|| {
        let find =
            |name: &str| props::script_names().iter().position(|l| l.split(';').any(|a| a == name));
        (find("Common"), find("Inherited"))
    });
    script.is_some() && (script == common || script == inherited)
}

impl Lattice<'_> {
    /// The length of the Korean unknown word of `class` at `pos`: the following units of the
    /// same script (Common and Inherited join any), punctuation kind and digit kind whose class
    /// groups, when the class of the first one does; returns the length and the class.
    fn unknown_length_ko(&self, pos: usize, class: u8) -> (usize, u8) {
        if !self.dict.groups(class) {
            return (1, class);
        }
        let first = self.text[pos];
        let mut script = unit_script(first);
        let mut class = class;
        let punct = punctuation_ko(first);
        let digit = unit_category(first) == DIGIT;
        let mut len = 1;
        while pos + len < self.text.len() && len < MAX_UNKNOWN {
            let u = self.text[pos + len];
            let s = unit_script(u);
            let same = script == s
                || common_or_inherited(script)
                || common_or_inherited(s)
                || unit_category(u) == NON_SPACING_MARK;
            if !(same
                && punctuation_ko(u) == punct
                && (unit_category(u) == DIGIT) == digit
                && self.dict.groups(self.dict.class(u)))
            {
                break;
            }
            len += 1;
            if common_or_inherited(script) && !common_or_inherited(s) {
                script = s;
                class = self.dict.class(u);
            }
        }
        (len, class)
    }

    /// Korean: the words of the best path, with where each starts after a skipped space.
    fn run_ko(mut self) -> Vec<Word> {
        let n = self.text.len();
        let mut pos = 0;
        while pos < n {
            self.touch(pos);
            let frontier = self.next == pos + 1;
            if self.positions[pos].is_empty() {
                pos += 1;
                continue;
            }
            if pos > self.last && self.positions[pos].len() == 1 && frontier {
                self.backtrace_ko(pos, 0);
                self.positions[pos][0].cost = 0;
            }
            let from = pos;
            if unit_category(self.text[pos]) == SPACE_SEPARATOR && pos + 1 < n {
                pos += 1;
            }
            let dict = self.dict;
            let spaced = pos > from;
            let penalty = |e: &Entry| {
                if spaced && SPACE_PENALTY_TAGS.contains(&dict.string(e.tag)) {
                    SPACE_PENALTY
                } else {
                    0
                }
            };
            let mut known = Vec::new();
            dict.words_at(self.text, pos, |end, surface| known.push((end, surface)));
            let any = !known.is_empty();
            for (end, surface) in known {
                for e in dict.entries_of(surface) {
                    let p = penalty(&e);
                    self.add(from, pos, end, e, Source::Known, 0, |_| p);
                }
            }
            let class = dict.class(self.text[pos]);
            if !any || dict.invokes(class) {
                let (len, class) = self.unknown_length_ko(pos, class);
                for e in dict.unknown_of(class) {
                    let p = penalty(&e);
                    self.add(from, pos, pos + len, e, Source::Unknown, 0, |_| p);
                }
            }
            pos += 1;
        }
        if n > 0 {
            self.touch(n);
            let index = self.final_index();
            self.backtrace_ko(n, index);
        }
        self.out
    }

    /// Follows the best path back from `index` at `end` to the last backtrace.
    fn backtrace_ko(&mut self, end: usize, index: usize) {
        if end == self.last {
            return;
        }
        let mut pos = end;
        let mut best = index;
        let mut words = Vec::new();
        while pos > self.last {
            let a = self.positions[pos][best];
            words.push(Word { start: a.word_start, end: pos, entry: a.entry });
            pos = a.back;
            best = a.back_index;
        }
        self.last = end;
        words.reverse();
        self.out.extend(words);
    }
}

/// General categories of [`props::CATEGORIES`] the Korean tokenizer looks at.
const NON_SPACING_MARK: usize = 6;
const DIGIT: usize = 9;
const SPACE_SEPARATOR: usize = 23;

/// A token of the dictionary tokenizers: its text and UTF-16 range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MorphToken {
    /// The text.
    pub text: String,
    /// First code unit.
    pub start: usize,
    /// End code unit.
    pub end: usize,
}

/// The tags `reflib.kuromoji` drops, measured on `refsearch` (particles, auxiliary verbs,
/// conjunctions, fillers, interjection-like others and symbols).
pub const JAPANESE_STOP_TAGS: &[&str] = &[
    "その他-間投",
    "フィラー",
    "助動詞",
    "助詞-並立助詞",
    "助詞-係助詞",
    "助詞-副助詞",
    "助詞-副助詞／並立助詞／終助詞",
    "助詞-副詞化",
    "助詞-接続助詞",
    "助詞-格助詞-一般",
    "助詞-格助詞-引用",
    "助詞-格助詞-連語",
    "助詞-特殊",
    "助詞-終助詞",
    "助詞-連体化",
    "接続詞",
    "記号-一般",
    "記号-句点",
    "記号-括弧閉",
    "記号-括弧開",
    "記号-空白",
    "記号-読点",
];

/// The tags `reflib.nori` drops, measured on `refsearch`.
pub const KOREAN_STOP_TAGS: &[&str] = &[
    "E", "IC", "J", "MAG", "MAJ", "MM", "NA", "SC", "SE", "SP", "SSC", "SSO", "UNA", "VSV", "XPN",
    "XSA", "XSN", "XSV",
];

/// Japanese: for each word of `text` (already width-folded, in UTF-16 units), its base form (or
/// its surface) and range, with `None` for words whose tag is a stop tag (they keep their
/// position).
pub fn kuromoji(text: &[u16]) -> Vec<Option<MorphToken>> {
    let Some(dict) = Dictionary::japanese() else { return Vec::new() };
    Lattice::new(dict, text)
        .run_ja()
        .into_iter()
        .map(|w| {
            if JAPANESE_STOP_TAGS.contains(&dict.string(w.entry.tag)) {
                return None;
            }
            let text = if w.entry.extra == 0 {
                String::from_utf16_lossy(&text[w.start..w.end])
            } else {
                dict.string(w.entry.extra).to_string()
            };
            Some(MorphToken { text, start: w.start, end: w.end })
        })
        .collect()
}

/// Korean: the tokens of `text` (UTF-16 units) with compound, inflected and pre-analysed words
/// split into their morphemes, each one's reading when it has one, and `None` for tokens whose
/// tag is a stop tag.
pub fn nori(text: &[u16]) -> Vec<Option<MorphToken>> {
    let Some(dict) = Dictionary::korean() else { return Vec::new() };
    let mut out = Vec::new();
    for w in Lattice::new(dict, text).run_ko() {
        let tag = dict.string(w.entry.tag);
        let extra = dict.string(w.entry.extra);
        let mut fields = extra.split('\u{1}');
        let kind = fields.next().unwrap_or("M");
        let reading = fields.next().unwrap_or("");
        let morphemes: Vec<&str> = fields.collect();
        if kind == "M" || kind.is_empty() || morphemes.is_empty() {
            if punctuation_ko(text[w.start]) {
                continue;
            }
            if KOREAN_STOP_TAGS.contains(&tag) {
                out.push(None);
                continue;
            }
            let text = if reading.is_empty() {
                String::from_utf16_lossy(&text[w.start..w.end])
            } else {
                reading.to_string()
            };
            out.push(Some(MorphToken { text, start: w.start, end: w.end }));
            continue;
        }
        let mut offset = w.start;
        for m in morphemes {
            let (mtag, surface) = m.split_once('\u{2}').unwrap_or((m, ""));
            let units = surface.encode_utf16().count();
            // A compound's parts follow one another; the parts of other kinds span the word.
            let (start, end) = if kind == "C" {
                let range = (offset, (offset + units).min(w.end));
                offset = range.1;
                range
            } else {
                (w.start, w.end)
            };
            if KOREAN_STOP_TAGS.contains(&mtag) {
                out.push(None);
            } else {
                out.push(Some(MorphToken { text: surface.to_string(), start, end }));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_dictionaries() {
        let gz = include_bytes!("../../data/kuromoji.bin");
        let raw = gzip_decompress(gz);
        assert!(raw.is_some(), "inflate");
        assert!(Dictionary::japanese().is_some());
        assert!(Dictionary::korean().is_some());
    }
}
