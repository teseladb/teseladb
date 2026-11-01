//! The morfologik tables (`data/morfologik-*.bin`, D-109) against the dictionaries that
//! `scripts/morfologik.sha256` pins: each dictionary is read out of its jar, its automaton (format
//! CFSA2) is walked down to the encoded lemma of every word form, leaving out the tags, and those
//! paths are built into a minimal acyclic automaton in the layout `src/analysis/morfologik.rs`
//! reads. With `TESELADB_WRITE_TABLES=1` the rebuilt tables are written.

use std::collections::HashMap;
use std::path::PathBuf;

use teseladb_deflate::inflate;

/// The jar of each table, the dictionary inside it, the encoder of the table (0 or 1) and the
/// separator of the dictionary.
const DICTIONARIES: &[(&str, &str, &str, u8, u8)] = &[
    (
        "morfologik-polish.bin",
        "org/carrot2/morfologik-polish/2.1.9/morfologik-polish-2.1.9.jar",
        "morfologik/stemming/polish/polish.dict",
        0,
        b';',
    ),
    (
        "morfologik-ukrainian.bin",
        "ua/net/nlp/morfologik-ukrainian-search/4.9.1/morfologik-ukrainian-search-4.9.1.jar",
        "ua/net/nlp/ukrainian.dict",
        1,
        b'+',
    ),
];

fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]]) as usize
}

/// The bytes of `name` in the zip archive `zip`, through its central directory.
fn unzip(zip: &[u8], name: &str) -> Vec<u8> {
    let end = (0..zip.len() - 21).rev().find(|&i| zip[i..i + 4] == [0x50, 0x4B, 0x05, 0x06]);
    let end = end.expect("end of central directory");
    let (entries, mut at) = (u16_at(zip, end + 10), u32_at(zip, end + 16));
    for _ in 0..entries {
        assert_eq!(zip[at..at + 4], [0x50, 0x4B, 0x01, 0x02], "central directory entry");
        let method = u16_at(zip, at + 10);
        let size = u32_at(zip, at + 20);
        let (name_len, extra, comment) =
            (u16_at(zip, at + 28), u16_at(zip, at + 30), u16_at(zip, at + 32));
        let local = u32_at(zip, at + 42);
        let entry = &zip[at + 46..at + 46 + name_len];
        at += 46 + name_len + extra + comment;
        if entry != name.as_bytes() {
            continue;
        }
        let data = local + 30 + u16_at(zip, local + 26) + u16_at(zip, local + 28);
        let raw = &zip[data..data + size];
        return match method {
            0 => raw.to_vec(),
            8 => inflate(raw).expect("deflate data"),
            other => panic!("{name}: compression method {other}"),
        };
    }
    panic!("{name} is not in the archive")
}

/// A CFSA2 automaton: arcs of a flags byte (target follows, last arc, final arc, label index),
/// an optional label and an optional variable-length target relative to the arcs' start.
struct Cfsa2<'a> {
    b: &'a [u8],
    labels: &'a [u8],
    base: usize,
}

impl Cfsa2<'_> {
    fn new(b: &[u8]) -> Cfsa2<'_> {
        assert!(b[..4] == *b"\\fsa" && b[4] == 0xC6, "a CFSA2 automaton");
        let n = usize::from(b[7]);
        Cfsa2 { b, labels: &b[8..8 + n], base: 8 + n }
    }

    /// The arcs of the node at `at`: label, final, target (0: none) and the node's end.
    fn arcs(&self, mut at: usize) -> (Vec<(u8, bool, usize)>, usize) {
        let mut out = Vec::new();
        loop {
            let f = self.b[at];
            at += 1;
            let index = usize::from(f & 0x1F);
            let label = if index == 0 {
                at += 1;
                self.b[at - 1]
            } else {
                self.labels[index]
            };
            let target = if f & 0x80 != 0 {
                usize::MAX
            } else {
                let (mut v, mut shift) = (0_usize, 0);
                loop {
                    let x = self.b[at];
                    at += 1;
                    v |= usize::from(x & 0x7F) << shift;
                    shift += 7;
                    if x & 0x80 == 0 {
                        break;
                    }
                }
                if v == 0 { 0 } else { v + self.base }
            };
            out.push((label, f & 0x20 != 0, target));
            if f & 0x40 != 0 {
                break;
            }
        }
        let end = at;
        let out =
            out.into_iter().map(|(l, fin, t)| (l, fin, if t == usize::MAX { end } else { t }));
        (out.collect(), end)
    }

    /// Every word form with its separator and encoded lemma, up to the second separator.
    fn entries(&self, separator: u8) -> Vec<Vec<u8>> {
        let (first, _) = self.arcs(self.base);
        let root = first[0].2;
        let mut out = Vec::new();
        let mut stack = vec![(root, Vec::new(), 0)];
        while let Some((node, prefix, seen)) = stack.pop() {
            for (label, _, target) in self.arcs(node).0 {
                let mut path = prefix.clone();
                path.push(label);
                let seen = seen + usize::from(label == separator);
                if seen == 2 {
                    path.pop();
                    out.push(path);
                } else if target != 0 {
                    stack.push((target, path, seen));
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// A node of the automaton being built: whether a path ends at it and its arcs.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Node {
    is_final: bool,
    arcs: Vec<(u8, usize)>,
}

/// A minimal acyclic automaton of sorted byte strings, built incrementally (J. Daciuk, S. Mihov,
/// B. Watson and R. Watson, "Incremental construction of minimal acyclic finite-state automata",
/// Computational Linguistics 26(1), 2000).
struct Builder {
    nodes: Vec<Node>,
    register: HashMap<Node, usize>,
    path: Vec<usize>,
    last: Vec<u8>,
}

impl Builder {
    fn new() -> Builder {
        let root = Node { is_final: false, arcs: Vec::new() };
        Builder { nodes: vec![root], register: HashMap::new(), path: vec![0], last: Vec::new() }
    }

    /// Replaces the nodes of the current path below depth `keep` by their registered equals.
    fn minimize(&mut self, keep: usize) {
        while self.path.len() > keep + 1 {
            let child = self.path.pop().expect("path");
            let parent = *self.path.last().expect("parent");
            let node = self.nodes[child].clone();
            let id = *self.register.entry(node).or_insert(child);
            let arc = self.nodes[parent].arcs.last_mut().expect("arc");
            arc.1 = id;
        }
    }

    fn add(&mut self, word: &[u8]) {
        let common = word.iter().zip(&self.last).take_while(|(a, b)| a == b).count();
        self.minimize(common);
        for &b in &word[common..] {
            let id = self.nodes.len();
            self.nodes.push(Node { is_final: false, arcs: Vec::new() });
            let parent = *self.path.last().expect("parent");
            self.nodes[parent].arcs.push((b, id));
            self.path.push(id);
        }
        let end = *self.path.last().expect("end");
        self.nodes[end].is_final = true;
        self.last = word.to_vec();
    }

    /// The table: header, then the nodes reachable from the root, each node's last child
    /// written right after it when it is not written yet.
    fn finish(mut self, encoder: u8, separator: u8) -> Vec<u8> {
        self.minimize(0);
        let (nodes, root) = (self.nodes, 0);
        // A final path end is an arc flag: an arc into a final node without arcs has no target.
        let leaf = |id: usize| nodes[id].arcs.is_empty();
        let mut order = Vec::new();
        let mut placed = vec![false; nodes.len()];
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if placed[id] || leaf(id) {
                continue;
            }
            placed[id] = true;
            order.push(id);
            let arcs = &nodes[id].arcs;
            for &(_, t) in arcs[..arcs.len() - 1].iter().rev() {
                stack.push(t);
            }
            stack.push(arcs[arcs.len() - 1].1);
        }
        let next_of: HashMap<usize, usize> = order.windows(2).map(|w| (w[0], w[1])).collect();
        let arc_size = |id: usize, i: usize, t: usize| {
            let last = i + 1 == nodes[id].arcs.len();
            if leaf(t) || (last && next_of.get(&id) == Some(&t)) { 2 } else { 6 }
        };
        let mut offset = HashMap::new();
        let mut at = 14;
        for &id in &order {
            offset.insert(id, at);
            at += nodes[id]
                .arcs
                .iter()
                .enumerate()
                .map(|(i, &(_, t))| arc_size(id, i, t))
                .sum::<usize>();
        }
        let mut out = b"TSLMRF01".to_vec();
        out.push(encoder);
        out.push(separator);
        out.extend((offset[&root] as u32).to_le_bytes());
        for &id in &order {
            let arcs = &nodes[id].arcs;
            for (i, &(label, t)) in arcs.iter().enumerate() {
                let mut flags = if nodes[t].is_final { 1 } else { 0 };
                if i + 1 == arcs.len() {
                    flags |= 2;
                }
                if leaf(t) {
                    out.extend([flags | 8, label]);
                } else if arc_size(id, i, t) == 2 {
                    out.extend([flags | 4, label]);
                } else {
                    out.extend([flags, label]);
                    out.extend((offset[&t] as u32).to_le_bytes());
                }
            }
        }
        out
    }
}

fn build(jar: &str, dict: &str, encoder: u8, separator: u8) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.tools/morfologik").join(jar);
    let zip = std::fs::read(&path).unwrap_or_else(|e| {
        panic!("{}: {e}; run scripts/fetch-pinned.sh scripts/morfologik.sha256", path.display())
    });
    let fsa = unzip(&zip, dict);
    let entries = Cfsa2::new(&fsa).entries(separator);
    let mut builder = Builder::new();
    for e in &entries {
        builder.add(e);
    }
    builder.finish(encoder, separator)
}

#[test]
fn embedded_morfologik_tables_match_the_dictionaries() {
    for &(table, jar, dict, encoder, separator) in DICTIONARIES {
        let built = build(jar, dict, encoder, separator);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data").join(table);
        if std::env::var_os("TESELADB_WRITE_TABLES").is_some() {
            std::fs::write(&path, &built).expect("write table");
        }
        let data = std::fs::read(&path).expect("table");
        assert!(built == data, "data/{table} is stale: rerun with TESELADB_WRITE_TABLES=1");
    }
}
