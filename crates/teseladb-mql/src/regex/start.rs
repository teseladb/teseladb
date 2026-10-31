//! PCRE2's start-of-match optimizations, which decide where a match is attempted: the characters
//! a match can start with, its shortest length, and a character every match contains. They only
//! skip attempts that cannot match, except that a verb such as `(*COMMIT)` in a skipped attempt
//! does not act, so PCRE2's results with verbs depend on them (`pcre2pattern`, «Optimizations
//! that affect backtracking verbs»). `(*NO_START_OPT)` turns them off.

use super::ast::{Class, Node, Verb};

/// What a match must look like where it starts.
#[derive(Debug)]
pub(super) struct StartInfo {
    /// The first character is one of these, when known.
    first: Option<Vec<First>>,
    /// A character, with its case variants, that every match contains.
    required: Option<Vec<char>>,
    /// The shortest match, in characters.
    min_len: usize,
}

/// A possible first character.
#[derive(Debug)]
enum First {
    Chars(Vec<char>),
    Class(Class),
}

impl StartInfo {
    /// The optimizations of `node`; none when `enabled` is false.
    pub(super) fn new(node: &Node, enabled: bool) -> StartInfo {
        if !enabled {
            return StartInfo { first: None, required: None, min_len: 0 };
        }
        let accept = has_accept(node);
        let min_len = if accept { 0 } else { min_len(node) };
        let first = if min_len == 0 { None } else { first(node) };
        // PCRE2 skips ahead to one known first character even with verbs in the pattern, but
        // builds no table of several first characters across them; `(*ACCEPT)` can end a match
        // before any required character.
        let verbs = has_verb(node);
        let first = first.filter(|f| !verbs || matches!(f.as_slice(), [First::Chars(_)]));
        let required = if accept { None } else { required(node) };
        StartInfo { first, required, min_len }
    }

    /// Whether `text` from `from` on can hold a match at all.
    pub(super) fn possible(&self, text: &str, from: usize) -> bool {
        self.required.as_ref().is_none_or(|set| text[from..].contains(set.as_slice()))
    }

    /// Whether a match can start at `pos`.
    pub(super) fn may_start(&self, text: &str, pos: usize) -> bool {
        // Bytes are never fewer than characters.
        if text.len() - pos < self.min_len {
            return false;
        }
        let Some(first) = &self.first else { return true };
        let Some(c) = text[pos..].chars().next() else { return false };
        first.iter().any(|f| match f {
            First::Chars(set) => set.contains(&c),
            First::Class(class) => class.matches(c),
        })
    }
}

/// Whether `node` holds a backtracking verb other than `(*MARK)`.
fn has_verb(node: &Node) -> bool {
    any(node, &|n| matches!(n, Node::Verb(v) if !matches!(v, Verb::Mark(_))))
}

/// Whether `node` holds `(*ACCEPT)`.
fn has_accept(node: &Node) -> bool {
    any(node, &is_accept)
}

/// Whether `node` or a node inside it satisfies `pred`.
fn any(node: &Node, pred: &dyn Fn(&Node) -> bool) -> bool {
    if pred(node) {
        return true;
    }
    match node {
        Node::Group(n, _)
        | Node::Atomic(n)
        | Node::ScriptRun(n)
        | Node::Repeat { node: n, .. }
        | Node::Look { node: n, .. } => any(n, pred),
        Node::Concat(items) | Node::Alt(items) => items.iter().any(|n| any(n, pred)),
        Node::Cond { yes, no, .. } => any(yes, pred) || any(no, pred),
        _ => false,
    }
}

/// Whether `node` is `(*ACCEPT)`, after which nothing is matched.
fn is_accept(node: &Node) -> bool {
    *node == Node::Verb(Verb::Accept)
}

/// The shortest length of what `node` matches; calls and back references count as empty.
fn min_len(node: &Node) -> usize {
    match node {
        Node::Char { .. } | Node::Dot { .. } | Node::Class(_) | Node::Grapheme => 1,
        Node::NewlineSeq => 1,
        Node::Group(n, _) | Node::Atomic(n) | Node::ScriptRun(n) => min_len(n),
        Node::Concat(items) => items.iter().take_while(|n| !is_accept(n)).map(min_len).sum(),
        Node::Alt(alts) => alts.iter().map(min_len).min().unwrap_or(0),
        Node::Repeat { node, min, .. } => min_len(node).saturating_mul(*min as usize),
        Node::Cond { yes, no, .. } => min_len(yes).min(min_len(no)),
        _ => 0,
    }
}

/// The characters a match of `node` can start with; `None` when any can. An item that can be
/// empty lets the next one start the match too.
fn first(node: &Node) -> Option<Vec<First>> {
    match node {
        Node::Char { c, caseless } => Some(vec![First::Chars(caseless.variants(*c))]),
        Node::Class(class) => Some(vec![First::Class(class.clone())]),
        Node::Group(n, _) | Node::Atomic(n) | Node::ScriptRun(n) => first(n),
        Node::Repeat { node, .. } => first(node),
        Node::Concat(items) => {
            let mut out = Vec::new();
            for item in items {
                out.extend(first(item)?);
                if min_len(item) > 0 {
                    return Some(out);
                }
            }
            None
        }
        Node::Alt(alts) => {
            let mut out = Vec::new();
            for alt in alts {
                out.extend(first(alt)?);
            }
            Some(out)
        }
        Node::Empty
        | Node::Assert(_)
        | Node::Look { .. }
        | Node::KeepOut
        | Node::Fail
        | Node::Verb(Verb::Commit | Verb::Prune | Verb::Skip | Verb::SkipTo(_))
        | Node::Verb(Verb::Then | Verb::Mark(_)) => Some(Vec::new()),
        _ => None,
    }
}

/// A character, with its case variants, that every match of `node` contains.
fn required(node: &Node) -> Option<Vec<char>> {
    match node {
        Node::Char { c, caseless } => Some(caseless.variants(*c)),
        Node::Group(n, _) | Node::Atomic(n) | Node::ScriptRun(n) => required(n),
        Node::Repeat { node, min, .. } if *min > 0 => required(node),
        Node::Concat(items) => {
            items.iter().take_while(|n| !is_accept(n)).filter_map(required).last()
        }
        Node::Alt(alts) => {
            let mut sets = alts.iter().map(required);
            let first = sets.next()??;
            sets.all(|s| s.as_ref() == Some(&first)).then_some(first)
        }
        _ => None,
    }
}
