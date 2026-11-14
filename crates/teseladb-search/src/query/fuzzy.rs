//! Term expansion over the dictionary (FR-018): Levenshtein automata for `fuzzy`, prefix automata
//! for `autocomplete` and the bridge that runs automata over characters on the byte FST.
//!
//! RefDb Search documents `fuzzy` with `maxEdits` (1 or 2, by default 2) counted as a
//! Damerau-Levenshtein distance, `prefixLength` (characters at the start of the term that must
//! match exactly, by default 0) and `maxExpansions` (variations per token, by default 50). The
//! automaton here is the optimal-string-alignment form of that distance: an insertion, a deletion,
//! a substitution or a swap of two adjacent characters is one edit, and a swapped pair is not
//! edited again. It is simulated row by row over the dynamic-programming table (Wagner and
//! Fischer, JACM 1974, doi:10.1145/321796.321811), which keeps one state per dictionary prefix and
//! prunes a subtree as soon as every cell of its row exceeds `maxEdits`.
//!
//! When more terms match than `maxExpansions`, the closest ones are kept, and among equal
//! distances the first ones in term order. RefDb does not document that choice; the battery
//! (T020) compares it with `refsearch`.

use crate::segment::fst::{Automaton, Fst};

/// An automaton over Unicode scalar values. [`Utf8`] runs it over the bytes of the dictionary.
pub trait CharAutomaton {
    /// A state.
    type State: Clone;
    /// The start state.
    fn start(&self) -> Self::State;
    /// The state after reading `c`, or `None` if no accepted term continues that way.
    fn step(&self, state: &Self::State, c: char) -> Option<Self::State>;
    /// Whether the characters read so far form an accepted term.
    fn is_match(&self, state: &Self::State) -> bool;
    /// Whether some continuation could still be accepted.
    fn can_match(&self, state: &Self::State) -> bool;
}

impl<A: CharAutomaton> CharAutomaton for &A {
    type State = A::State;

    fn start(&self) -> A::State {
        (*self).start()
    }

    fn step(&self, state: &A::State, c: char) -> Option<A::State> {
        (*self).step(state, c)
    }

    fn is_match(&self, state: &A::State) -> bool {
        (*self).is_match(state)
    }

    fn can_match(&self, state: &A::State) -> bool {
        (*self).can_match(state)
    }
}

/// Runs a [`CharAutomaton`] over UTF-8 bytes. A term that is not valid UTF-8 never matches.
#[derive(Clone, Copy, Debug)]
pub struct Utf8<A>(pub A);

/// A character automaton's state with the bytes of the character being read.
#[derive(Clone, Debug)]
pub struct Utf8State<S> {
    inner: S,
    buf: [u8; 4],
    have: u8,
    need: u8,
}

/// Length of the UTF-8 sequence that starts with `lead`, or `None` if no sequence starts so.
fn sequence_len(lead: u8) -> Option<u8> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

impl<A: CharAutomaton> Automaton for Utf8<A> {
    type State = Utf8State<A::State>;

    fn start(&self) -> Self::State {
        Utf8State { inner: self.0.start(), buf: [0; 4], have: 0, need: 0 }
    }

    fn step(&self, state: &Self::State, byte: u8) -> Option<Self::State> {
        let mut next = state.clone();
        if next.have == 0 {
            next.need = sequence_len(byte)?;
        } else if byte & 0xC0 != 0x80 {
            return None;
        }
        next.buf[usize::from(next.have)] = byte;
        next.have += 1;
        if next.have < next.need {
            return Some(next);
        }
        // Overlong forms, surrogates and values above U+10FFFF fail here.
        let c = std::str::from_utf8(&next.buf[..usize::from(next.need)]).ok()?.chars().next()?;
        Some(Utf8State { inner: self.0.step(&state.inner, c)?, buf: [0; 4], have: 0, need: 0 })
    }

    fn is_match(&self, state: &Self::State) -> bool {
        state.have == 0 && self.0.is_match(&state.inner)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        self.0.can_match(&state.inner)
    }
}

/// The `fuzzy` options of RefDb Search.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuzzyOptions {
    /// Maximum edits, 1 or 2.
    pub max_edits: u8,
    /// Characters at the start of a matching term that must equal the query's.
    pub prefix_length: usize,
    /// Maximum terms a query token expands to.
    pub max_expansions: usize,
}

/// A `fuzzy` option out of its documented domain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuzzyError {
    /// `maxEdits` other than 1 or 2.
    MaxEdits,
    /// `prefixLength` below 0.
    PrefixLength,
    /// `maxExpansions` below 1.
    MaxExpansions,
}

impl FuzzyOptions {
    /// The documented defaults: `maxEdits: 2`, `prefixLength: 0`, `maxExpansions: 50`.
    pub const DEFAULT: FuzzyOptions =
        FuzzyOptions { max_edits: 2, prefix_length: 0, max_expansions: 50 };

    /// Options from the integers of a query, checked against their domain.
    pub fn new(
        max_edits: i64,
        prefix_length: i64,
        max_expansions: i64,
    ) -> Result<FuzzyOptions, FuzzyError> {
        let max_edits = match max_edits {
            1 => 1,
            2 => 2,
            _ => return Err(FuzzyError::MaxEdits),
        };
        let prefix_length = usize::try_from(prefix_length).map_err(|_| FuzzyError::PrefixLength)?;
        let max_expansions = usize::try_from(max_expansions)
            .ok()
            .filter(|&n| n >= 1)
            .ok_or(FuzzyError::MaxExpansions)?;
        Ok(FuzzyOptions { max_edits, prefix_length, max_expansions })
    }
}

/// Accepts the terms within `max_edits` optimal-string-alignment edits of a query term whose
/// first `prefix_length` characters they share.
#[derive(Clone, Debug)]
pub struct Levenshtein {
    prefix: Vec<char>,
    rest: Vec<char>,
    max: u8,
}

/// A [`Levenshtein`] state: the prefix read so far, then the last two rows of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevState {
    prefix_read: usize,
    prev: Vec<u8>,
    row: Vec<u8>,
    last: Option<char>,
}

impl Levenshtein {
    /// The automaton of `term` with `max_edits` edits and an exact prefix of `prefix_length`
    /// characters (the whole term if it is shorter).
    pub fn new(term: &str, max_edits: u8, prefix_length: usize) -> Levenshtein {
        let chars: Vec<char> = term.chars().collect();
        let split = prefix_length.min(chars.len());
        Levenshtein {
            prefix: chars[..split].to_vec(),
            rest: chars[split..].to_vec(),
            max: max_edits,
        }
    }

    /// The edits between the query term and `term`, if `term` is accepted.
    pub fn distance(&self, term: &str) -> Option<u8> {
        let mut state = self.start();
        for c in term.chars() {
            state = self.step(&state, c)?;
        }
        self.edits(&state)
    }

    /// The edits of an accepted state.
    fn edits(&self, state: &LevState) -> Option<u8> {
        let last = *state.row.last().expect("a row has the empty column");
        (state.prefix_read == self.prefix.len() && last <= self.max).then_some(last)
    }

    fn capped(&self, v: usize) -> u8 {
        u8::try_from(v).map_or(self.max + 1, |v| v.min(self.max + 1))
    }
}

impl CharAutomaton for Levenshtein {
    type State = LevState;

    fn start(&self) -> LevState {
        let row: Vec<u8> = (0..=self.rest.len()).map(|j| self.capped(j)).collect();
        LevState { prefix_read: 0, prev: vec![self.max + 1; row.len()], row, last: None }
    }

    fn step(&self, state: &LevState, c: char) -> Option<LevState> {
        if state.prefix_read < self.prefix.len() {
            return (self.prefix[state.prefix_read] == c)
                .then(|| LevState { prefix_read: state.prefix_read + 1, ..state.clone() });
        }
        let over = self.max + 1;
        let mut row = Vec::with_capacity(state.row.len());
        row.push((state.row[0] + 1).min(over));
        for j in 1..=self.rest.len() {
            let substitution = state.row[j - 1] + u8::from(self.rest[j - 1] != c);
            let mut best = (state.row[j] + 1).min(row[j - 1] + 1).min(substitution);
            if j >= 2 && state.last == Some(self.rest[j - 1]) && c == self.rest[j - 2] {
                best = best.min(state.prev[j - 2] + 1);
            }
            row.push(best.min(over));
        }
        row.iter().any(|&v| v <= self.max).then(|| LevState {
            prefix_read: state.prefix_read,
            prev: state.row.clone(),
            row,
            last: Some(c),
        })
    }

    fn is_match(&self, state: &LevState) -> bool {
        self.edits(state).is_some()
    }

    fn can_match(&self, state: &LevState) -> bool {
        state.prefix_read < self.prefix.len() || state.row.iter().any(|&v| v <= self.max)
    }
}

/// Accepts the terms that start with a given string (`autocomplete` and prefix queries).
#[derive(Clone, Debug)]
pub struct Prefix(pub Vec<u8>);

impl Automaton for Prefix {
    type State = usize;

    fn start(&self) -> usize {
        0
    }

    fn step(&self, read: &usize, byte: u8) -> Option<usize> {
        match self.0.get(*read) {
            Some(&b) if b == byte => Some(read + 1),
            Some(_) => None,
            None => Some(*read),
        }
    }

    fn is_match(&self, read: &usize) -> bool {
        *read == self.0.len()
    }

    fn can_match(&self, _: &usize) -> bool {
        true
    }
}

/// A dictionary term a query term expanded to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expansion {
    /// The term.
    pub term: Vec<u8>,
    /// Its value in the dictionary (where its postings start).
    pub value: u64,
    /// Edits from the query term; 0 for an exact match.
    pub edits: u8,
}

/// The terms of `fst` within `options` of `term`: at most `max_expansions`, the closest first and
/// then in term order.
pub fn fuzzy_terms(fst: &Fst<'_>, term: &str, options: FuzzyOptions) -> Vec<Expansion> {
    let automaton = Levenshtein::new(term, options.max_edits, options.prefix_length);
    let mut found: Vec<Expansion> = fst
        .search(&Utf8(&automaton))
        .into_iter()
        .map(|(term, value)| {
            // The automaton only accepts valid UTF-8 at a distance within the limit.
            let edits = std::str::from_utf8(&term)
                .ok()
                .and_then(|t| automaton.distance(t))
                .expect("accepted terms have a distance");
            Expansion { term, value, edits }
        })
        .collect();
    // `search` returns terms in order and the sort is stable: equal distances keep that order.
    found.sort_by_key(|e| e.edits);
    found.truncate(options.max_expansions);
    found
}

/// The terms of `fst` that start with `prefix`, in order.
pub fn prefix_terms(fst: &Fst<'_>, prefix: &str) -> Vec<(Vec<u8>, u64)> {
    fst.search(&Prefix(prefix.as_bytes().to_vec()))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::segment::fst::Builder;

    /// Reference distance: optimal string alignment over the whole table.
    fn osa(a: &[char], b: &[char]) -> usize {
        let mut d = vec![vec![0_usize; b.len() + 1]; a.len() + 1];
        for (i, row) in d.iter_mut().enumerate() {
            row[0] = i;
        }
        for (j, cell) in d[0].iter_mut().enumerate() {
            *cell = j;
        }
        for i in 1..=a.len() {
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                let mut v = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(d[i - 2][j - 2] + 1);
                }
                d[i][j] = v;
            }
        }
        d[a.len()][b.len()]
    }

    fn dict(terms: &[&str]) -> Vec<u8> {
        let mut sorted: Vec<&str> = terms.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        let mut b = Builder::new();
        for (i, t) in sorted.iter().enumerate() {
            b.insert(t.as_bytes(), i as u64);
        }
        b.finish()
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn word(&mut self, alphabet: &[char]) -> String {
            let len = (self.next() % 6) as usize;
            (0..len).map(|_| alphabet[(self.next() % alphabet.len() as u64) as usize]).collect()
        }
    }

    #[test]
    fn story_one_finds_refdb_from_mongdb() {
        let bytes = dict(&[
            teseladb_names::KERBEROS_SERVICE,
            teseladb_names::AUDIT_SCHEMA_NATIVE,
            "anotherproduct",
            "mango",
        ]);
        let fst = Fst::new(&bytes).unwrap();
        let options = FuzzyOptions::new(1, 0, 50).unwrap();
        let found = fuzzy_terms(&fst, "mongdb", options);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].term, b"refdb");
        assert_eq!(found[0].edits, 1);
    }

    #[test]
    fn a_swap_of_adjacent_characters_is_one_edit() {
        let lev = Levenshtein::new(teseladb_names::KERBEROS_SERVICE, 1, 0);
        assert_eq!(lev.distance("mongdob"), Some(1));
        assert_eq!(lev.distance("omngodb"), Some(1));
        assert_eq!(lev.distance(teseladb_names::KERBEROS_SERVICE), Some(0));
        // Two swaps are two edits.
        assert_eq!(lev.distance("omngdob"), None);
        assert_eq!(
            Levenshtein::new(teseladb_names::KERBEROS_SERVICE, 2, 0).distance("omngdob"),
            Some(2)
        );
        // A swapped pair is not edited again: "ca" → "abc" is 3 in this distance.
        assert_eq!(Levenshtein::new("ca", 2, 0).distance("abc"), None);
    }

    #[test]
    fn the_automaton_agrees_with_the_table_on_random_words() {
        let alphabet = ['a', 'b', 'c', 'ñ', '漢'];
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        for _ in 0..4000 {
            let q = rng.word(&alphabet);
            let t = rng.word(&alphabet);
            let max = 1 + (rng.next() % 2) as u8;
            let prefix = (rng.next() % 3) as usize;
            let (qc, tc): (Vec<char>, Vec<char>) = (q.chars().collect(), t.chars().collect());
            let split = prefix.min(qc.len());
            let expected = if tc.len() >= split && tc[..split] == qc[..split] {
                let d = osa(&qc[split..], &tc[split..]);
                (d <= usize::from(max)).then_some(d as u8)
            } else {
                None
            };
            assert_eq!(Levenshtein::new(&q, max, prefix).distance(&t), expected, "{q} {t}");
        }
    }

    #[test]
    fn expansion_over_the_dictionary_matches_a_scan() {
        let alphabet = ['a', 'b', 'c', 'é'];
        let mut rng = Rng(42);
        for round in 0..60 {
            let words: Vec<String> = (0..80).map(|_| rng.word(&alphabet)).collect();
            let refs: Vec<&str> = words.iter().map(String::as_str).collect();
            let bytes = dict(&refs);
            let fst = Fst::new(&bytes).unwrap();
            let q = rng.word(&alphabet);
            let options = FuzzyOptions {
                max_edits: 1 + (round % 2) as u8,
                prefix_length: round % 3,
                max_expansions: 1 + round % 7,
            };
            let lev = Levenshtein::new(&q, options.max_edits, options.prefix_length);
            let mut expected: Vec<(u8, Vec<u8>)> = fst
                .entries()
                .into_iter()
                .filter_map(|(t, _)| lev.distance(std::str::from_utf8(&t).unwrap()).map(|d| (d, t)))
                .collect();
            expected.sort();
            expected.truncate(options.max_expansions);
            let got: Vec<(u8, Vec<u8>)> =
                fuzzy_terms(&fst, &q, options).into_iter().map(|e| (e.edits, e.term)).collect();
            assert_eq!(got, expected, "query {q}");
        }
    }

    #[test]
    fn the_prefix_must_match_exactly() {
        let lev = Levenshtein::new(teseladb_names::KERBEROS_SERVICE, 2, 3);
        assert_eq!(lev.distance("mangodb"), None);
        assert_eq!(lev.distance("mongdb"), Some(1));
        // A prefix longer than the term leaves only insertions after the whole term.
        let short = Levenshtein::new("ab", 1, 5);
        assert_eq!(short.distance("abc"), Some(1));
        assert_eq!(short.distance("a"), None);
        assert_eq!(short.distance("ab"), Some(0));
    }

    #[test]
    fn options_are_checked_against_their_domain() {
        assert_eq!(FuzzyOptions::new(2, 0, 50), Ok(FuzzyOptions::DEFAULT));
        assert_eq!(FuzzyOptions::new(0, 0, 50), Err(FuzzyError::MaxEdits));
        assert_eq!(FuzzyOptions::new(3, 0, 50), Err(FuzzyError::MaxEdits));
        assert_eq!(FuzzyOptions::new(1, -1, 50), Err(FuzzyError::PrefixLength));
        assert_eq!(FuzzyOptions::new(1, 0, 0), Err(FuzzyError::MaxExpansions));
        assert_eq!(FuzzyOptions::new(1, 0, -3), Err(FuzzyError::MaxExpansions));
        assert_eq!(FuzzyOptions::new(1, 2, 3).unwrap().prefix_length, 2);
    }

    #[test]
    fn utf8_bytes_that_are_not_characters_never_match() {
        let lev = Levenshtein::new("a", 2, 0);
        let u = Utf8(&lev);
        let run = |bytes: &[u8]| {
            let mut s = u.start();
            for &b in bytes {
                match u.step(&s, b) {
                    Some(n) => s = n,
                    None => return false,
                }
            }
            u.is_match(&s)
        };
        assert!(run("añ".as_bytes()));
        assert!(!run(&[0x80]));
        assert!(!run(&[0xC3]));
        assert!(!run(&[0xC3, 0x41]));
        assert!(!run(&[0xE0, 0x80, 0x80]));
        assert!(!run(&[0xED, 0xA0, 0x80]));
        assert!(!run(&[0xF8]));
        assert!(run("𝄞".as_bytes()));
        assert!(u.can_match(&u.start()));
    }

    #[test]
    fn prefix_terms_are_in_order() {
        let bytes = dict(&["car", "card", "care", "cat", "dog", "ca"]);
        let fst = Fst::new(&bytes).unwrap();
        let got: Vec<Vec<u8>> = prefix_terms(&fst, "car").into_iter().map(|(t, _)| t).collect();
        assert_eq!(got, vec![b"car".to_vec(), b"card".to_vec(), b"care".to_vec()]);
        assert_eq!(prefix_terms(&fst, "").len(), 6);
        assert!(prefix_terms(&fst, "z").is_empty());
    }

    #[test]
    fn subtrees_too_far_away_are_not_visited() {
        let lev = Levenshtein::new("ab", 1, 0);
        let s = lev.start();
        let s = lev.step(&s, 'x').unwrap();
        assert!(lev.step(&s, 'y').is_none());
        let p = Levenshtein::new("ab", 1, 1);
        assert!(p.can_match(&p.start()));
        assert!(!p.is_match(&p.start()));
    }
}
