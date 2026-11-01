//! The morphological dictionaries of `reflib.morfologik` (Polish) and `reflib.ukrainian` (D-109):
//! the word forms of the morfologik dictionaries with the encoded lemma of each, as an acyclic
//! automaton of their bytes. `tests/morfologik_data.rs` builds the tables in `data/` from the
//! dictionaries `scripts/morfologik.sha256` pins and fails if they differ.
//!
//! Layout: the magic `TSLMRF01`, the encoder (0: the lemma keeps a slice of the form, 1: it keeps a
//! start of the form), the separator byte, the root offset as a little-endian `u32`, then the
//! nodes. A node is its arcs; an arc is a flags byte ([`FINAL`], [`LAST`], [`NEXT`], [`NONE`]),
//! its label and, without `NEXT` or `NONE`, the little-endian `u32` offset of its target. Each
//! path is a form, the separator and the encoded lemma: with encoder 0 two letters `A` + n and
//! `A` + m remove n bytes from the start and m from the end of the form and the rest is appended;
//! with encoder 1 one letter `A` + n removes n bytes from the end.

use std::sync::OnceLock;

/// Arc flag: a path ends with this arc.
const FINAL: u8 = 1;
/// Arc flag: the last arc of its node.
const LAST: u8 = 2;
/// Arc flag: the target node follows the arc.
const NEXT: u8 = 4;
/// Arc flag: the arc has no target node.
const NONE: u8 = 8;

const MAGIC: &[u8; 8] = b"TSLMRF01";
const HEADER: usize = 14;

/// A dictionary of word forms and their lemmas.
#[derive(Debug)]
pub struct Dictionary {
    data: &'static [u8],
    root: usize,
    keeps_slice: bool,
    separator: u8,
}

/// The dictionaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    /// PoliMorf, from morfologik-polish 2.1.9.
    Polish,
    /// dict_uk, from morfologik-ukrainian-search 4.9.1.
    Ukrainian,
}

/// A step of the walk that lists the paths below a node.
enum Step {
    /// The paths below a node, after the given bytes.
    Node(usize, Vec<u8>),
    /// A path that ends.
    Emit(Vec<u8>),
}

/// One arc read from the automaton.
struct Arc {
    label: u8,
    flags: u8,
    target: Option<usize>,
    end: usize,
}

impl Dictionary {
    /// The tables of `data`, if they are well formed.
    pub fn parse(data: &'static [u8]) -> Option<Dictionary> {
        if data.get(..8)? != MAGIC {
            return None;
        }
        let keeps_slice = match *data.get(8)? {
            0 => true,
            1 => false,
            _ => return None,
        };
        let separator = *data.get(9)?;
        let root = u32::from_le_bytes(data.get(10..HEADER)?.try_into().ok()?) as usize;
        (root >= HEADER && root < data.len()).then_some(Dictionary {
            data,
            root,
            keeps_slice,
            separator,
        })
    }

    /// The dictionary of `language`, parsed once; `None` when its table is missing.
    pub fn get(language: Language) -> Option<&'static Dictionary> {
        static POLISH: OnceLock<Option<Dictionary>> = OnceLock::new();
        static UKRAINIAN: OnceLock<Option<Dictionary>> = OnceLock::new();
        let (cell, data): (_, &'static [u8]) = match language {
            Language::Polish => (&POLISH, include_bytes!("../../data/morfologik-polish.bin")),
            Language::Ukrainian => {
                (&UKRAINIAN, include_bytes!("../../data/morfologik-ukrainian.bin"))
            }
        };
        cell.get_or_init(|| Dictionary::parse(data)).as_ref()
    }

    fn arc(&self, at: usize) -> Option<Arc> {
        let flags = *self.data.get(at)?;
        let label = *self.data.get(at + 1)?;
        if flags & (NEXT | NONE) != 0 {
            let end = at + 2;
            let target = (flags & NEXT != 0).then_some(end);
            return Some(Arc { label, flags, target, end });
        }
        let bytes = self.data.get(at + 2..at + 6)?;
        let target = u32::from_le_bytes(bytes.try_into().ok()?) as usize;
        Some(Arc { label, flags, target: Some(target), end: at + 6 })
    }

    /// The arcs of the node at `at`.
    fn arcs(&self, mut at: usize) -> Vec<Arc> {
        let mut out = Vec::new();
        while let Some(arc) = self.arc(at) {
            at = arc.end;
            let last = arc.flags & LAST != 0;
            out.push(arc);
            if last {
                break;
            }
        }
        out
    }

    /// The node `bytes` lead to from the root.
    fn walk(&self, bytes: &[u8]) -> Option<usize> {
        let mut node = self.root;
        for &b in bytes {
            node = self.arcs(node).into_iter().find(|a| a.label == b)?.target?;
        }
        Some(node)
    }

    /// The encoded lemmas of `form`, in byte order.
    fn encodings(&self, form: &str) -> Vec<Vec<u8>> {
        let mut key = form.as_bytes().to_vec();
        key.push(self.separator);
        let Some(start) = self.walk(&key) else { return Vec::new() };
        // Depth first in label order: a path that ends is listed before the longer ones.
        let mut out = Vec::new();
        let mut work = vec![Step::Node(start, Vec::new())];
        while let Some(step) = work.pop() {
            let (node, prefix) = match step {
                Step::Emit(path) => {
                    out.push(path);
                    continue;
                }
                Step::Node(node, prefix) => (node, prefix),
            };
            for arc in self.arcs(node).into_iter().rev() {
                let mut path = prefix.clone();
                path.push(arc.label);
                if let Some(target) = arc.target {
                    work.push(Step::Node(target, path.clone()));
                }
                if arc.flags & FINAL != 0 {
                    work.push(Step::Emit(path));
                }
            }
        }
        out
    }

    /// The lemma `code` encodes for `form`.
    fn decode(&self, form: &str, code: &[u8]) -> Option<String> {
        let f = form.as_bytes();
        let count = |b: &u8| b.checked_sub(b'A').map(usize::from);
        let (start, cut, rest) = if self.keeps_slice {
            (count(code.first()?)?, count(code.get(1)?)?, code.get(2..)?)
        } else {
            (0, count(code.first()?)?, code.get(1..)?)
        };
        let kept = f.get(start..f.len().checked_sub(cut)?)?;
        String::from_utf8([kept, rest].concat()).ok()
    }

    /// The distinct lemmas of `form` in the dictionary's order, none when it is not a form.
    pub fn lemmas(&self, form: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for code in self.encodings(form) {
            if let Some(lemma) = self.decode(form, &code)
                && !out.contains(&lemma)
            {
                out.push(lemma);
            }
        }
        out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rejects_malformed_tables() {
        assert!(Dictionary::parse(b"").is_none());
        assert!(Dictionary::parse(b"TSLMRF01\x02;\x0e\0\0\0\x09A").is_none());
        assert!(Dictionary::parse(b"TSLMRF01\x00;\xff\0\0\0\x09A").is_none());
        assert!(Dictionary::parse(b"TSLMRF02\x00;\x0e\0\0\0\x09A").is_none());
    }

    #[test]
    fn lemmas_measured_on_refsearch() {
        let polish = Dictionary::get(Language::Polish).expect("Polish table");
        assert_eq!(polish.lemmas("plakatów"), ["plakat"]);
        assert_eq!(polish.lemmas("objęciu"), ["objęcie", "objąć"]);
        assert_eq!(polish.lemmas("AGD"), ["AGD", "artykuły gospodarstwa domowego"]);
        assert!(polish.lemmas("FIA").is_empty());
        assert!(polish.decode("ab", b"Z").is_none());
        assert!(polish.decode("ab", b"AZ").is_none());
        let ukrainian = Dictionary::get(Language::Ukrainian).expect("Ukrainian table");
        assert_eq!(ukrainian.lemmas("людей"), ["людина"]);
        assert_eq!(ukrainian.lemmas("києва"), ["Київ"]);
    }
}
