//! The tokenizer of `reflib.thai` as `refsearch` 1.70.6 runs it (011 T009, D-109): runs of Thai
//! letters are cut into words with the Thai dictionary of the Java runtime, and the other
//! characters follow its word rules. The word list in `data/thai-words.txt` was deduced by asking
//! `refsearch` (D-109): a candidate (the words of the ICU 74.2 Thai dictionary and the tokens `refsearch`
//! gave for 12 000 sentences) is a word when `refsearch` cuts it, and it after and before `ภาษา`, as
//! this segmenter does with it in the list and not without it.
//!
//! A run of Thai letters is walked letter by letter through the word list, longest words first.
//! Each place a word ends is a possible break; when the walk cannot go on, it goes back to the
//! last possible break not yet proved wrong. When no break is left, the breaks of the walk that
//! reached furthest are kept and the walk starts again one letter after that point, so letters no
//! word covers join the word after them (or the last word, at the end of the run). Measured on
//! 12 000 sentences: see `tests/sentences_refsearch.rs`.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::analysis::props;

/// A node of the word trie.
#[derive(Default)]
struct Node {
    next: HashMap<char, usize>,
    word: bool,
}

/// The Thai word list as a trie.
struct Trie {
    nodes: Vec<Node>,
}

impl Trie {
    fn build(words: &str) -> Trie {
        let mut nodes = vec![Node::default()];
        for w in words.lines().filter(|l| !l.is_empty()) {
            let mut at = 0;
            for c in w.chars() {
                let len = nodes.len();
                at = *nodes[at].next.entry(c).or_insert(len);
                if at == len {
                    nodes.push(Node::default());
                }
            }
            nodes[at].word = true;
        }
        Trie { nodes }
    }

    fn get() -> &'static Trie {
        static TRIE: OnceLock<Trie> = OnceLock::new();
        TRIE.get_or_init(|| Trie::build(include_str!("../../data/thai-words.txt")))
    }
}

/// Whether `c` is a Thai letter, vowel or mark that the dictionary walks over (U+0E01 to U+0E3A
/// and U+0E40 to U+0E4E).
pub fn thai_letter(c: char) -> bool {
    matches!(c, '\u{0E01}'..='\u{0E3A}' | '\u{0E40}'..='\u{0E4E}')
}

/// The ends of the words of a run of Thai letters, the last one at `text.len()`.
fn breaks(text: &[char], trie: &Trie) -> Vec<usize> {
    let end = text.len();
    let mut current: Vec<usize> = Vec::new();
    let mut possible: Vec<usize> = Vec::new();
    let mut wrong: Vec<usize> = Vec::new();
    let mut farthest = 0;
    let mut best: Option<Vec<usize>> = None;
    let mut i = 0;
    let mut node = 0;
    loop {
        if trie.nodes[node].word {
            possible.push(i);
        }
        let next = if i < end { trie.nodes[node].next.get(&text[i]).copied() } else { None };
        if let Some(n) = next {
            node = n;
            i += 1;
            continue;
        }
        if i > farthest {
            farthest = i;
            best = Some(current.clone());
        }
        while possible.last().is_some_and(|p| wrong.contains(p)) {
            possible.pop();
        }
        match possible.pop() {
            None => match &best {
                Some(b) => {
                    current = b.clone();
                    if farthest < end {
                        i = farthest + 1;
                    } else {
                        break;
                    }
                }
                None => {
                    if current.last() != Some(&i) && i != 0 {
                        current.push(i);
                    }
                    i += 1;
                    current.push(i);
                }
            },
            Some(p) => {
                while current.last().is_some_and(|&c| p < c) {
                    if let Some(c) = current.pop() {
                        wrong.push(c);
                    }
                }
                current.push(p);
                i = p;
            }
        }
        node = 0;
        if i >= end {
            break;
        }
    }
    current.pop();
    current.push(end);
    current
}

/// Whether `c` is a letter or a digit (general category L or Nd, Nl, No), as Java's
/// `Character.isLetterOrDigit` reads it.
fn letter_or_digit(c: char) -> bool {
    matches!(props::category(c), 1..=5 | 9)
}

/// Whether `c` is a decimal digit.
fn digit(c: char) -> bool {
    props::category(c) == 9
}

/// The byte ranges of the tokens of `text`: Thai runs cut into words, and the other words by the
/// Java word rules `refsearch` follows here (letters joined by one `-`, `.` or `'`, digits joined by
/// one `.` or `,`); a piece that starts with punctuation, such as `.5` after a letter, is no token.
pub fn thai_words(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |i: usize| chars.get(i).map_or(text.len(), |&(b, _)| b);
    let trie = Trie::get();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        if thai_letter(c) {
            let start = i;
            while i < chars.len() && thai_letter(chars[i].1) {
                i += 1;
            }
            let run: Vec<char> = chars[start..i].iter().map(|&(_, c)| c).collect();
            let mut from = 0;
            for b in breaks(&run, trie) {
                if b > from && letter_or_digit(run[from]) {
                    out.push((at(start + from), at(start + b)));
                }
                from = from.max(b);
            }
            continue;
        }
        if !letter_or_digit(c) {
            // A `.` or `,` right before a digit starts a number piece that is not a token.
            if matches!(c, '.' | ',') && chars.get(i + 1).is_some_and(|&(_, d)| digit(d)) {
                i = word_end(&chars, i + 1);
            } else {
                i += 1;
            }
            continue;
        }
        let start = i;
        i = word_end(&chars, i);
        out.push((at(start), at(i)));
    }
    out
}

/// The end of the word that starts with the letter or digit at `i`.
fn word_end(chars: &[(usize, char)], mut i: usize) -> usize {
    let part = |c: char| letter_or_digit(c) && !thai_letter(c);
    while i < chars.len() && part(chars[i].1) {
        i += 1;
        if i + 1 < chars.len() && part(chars[i + 1].1) {
            let (before, mid, after) = (chars[i - 1].1, chars[i].1, chars[i + 1].1);
            let letters = !digit(before) && !digit(after) && matches!(mid, '-' | '.' | '\'');
            let digits = digit(before) && digit(after) && matches!(mid, '.' | ',');
            if letters || digits {
                i += 1;
            }
        }
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cut(text: &str) -> Vec<&str> {
        thai_words(text).into_iter().map(|(a, b)| &text[a..b]).collect()
    }

    #[test]
    fn cuts_like_refsearch() {
        assert_eq!(cut("ภาษาไทย"), ["ภาษา", "ไทย"]);
        // Letters no word covers join the next word, or the last one at the end of the run.
        assert_eq!(cut("ไทยฆฆภาษา"), ["ไทย", "ฆฆภาษา"]);
        assert_eq!(cut("ภาษาไทยฆฆ"), ["ภาษา", "ไทยฆฆ"]);
        assert_eq!(cut("ไทยฆฆ"), ["ไทยฆฆ"]);
        assert_eq!(cut("ภาษาฆฆไทยฆฆภาษา"), ["ภาษา", "ฆฆไทย", "ฆฆภาษา"]);
        assert_eq!(cut("มีความ"), ["มีค", "วาม"]);
    }

    #[test]
    fn follows_the_java_word_rules_elsewhere() {
        assert_eq!(
            cut("a-b-c a--b a.b.c a'b a_b 1,5 1-2 C++"),
            ["a-b-c", "a", "b", "a.b.c", "a'b", "a", "b", "1,5", "1", "2", "C"]
        );
        assert_eq!(cut("1..2 ไทย123 ๑๐"), ["1", "ไทย", "123", "๑๐"]);
    }
}
