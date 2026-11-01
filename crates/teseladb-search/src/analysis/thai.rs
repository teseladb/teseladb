//! The tokenizer of `reflib.thai` as `refsearch` 1.70.6 runs it (011 T009, D-109): the Thai word
//! iterator of its Java runtime (Temurin 21.0.11), whose rules and dictionary walk this module
//! follows. The rules cut the text into segments: a run of dictionary characters, with `ๆ` or
//! `ฯๆ` after it, or with `ฯ` when the character after the `ฯ` is neither `ล` nor one to ignore;
//! `ฯลฯ`; any other character alone. A segment is divided with the dictionary only when the rules
//! looked up two dictionary characters or more to find it.
//!
//! The dictionary walk goes letter by letter through the word list, longest words first. Each
//! place a word ends is a possible break; when the walk cannot go on, it goes back to the last
//! possible break not yet proved wrong. When no break is left, the breaks of the walk that reached
//! furthest are kept and the walk starts again one letter after that point, so the letter it
//! stopped at joins the word before. A character outside the dictionary's alphabet at the end of a
//! word stops the walk, and the rest of the segment joins that word.
//!
//! `data/thai-words.txt` holds the words and `data/thai-prefixes.txt` the strings the dictionary
//! walks through without a word ending, asked of the runtime of `refsearch` for every place of the
//! 12 000 sentences of `tests/sentences_refsearch.rs` and for the earlier word list, which kept only
//! the strings the runtime has as words (01-11-2026). Measured: the 12 000 sentences give the
//! tokens of `refsearch`.

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
    /// The trie of `words`, with the paths of `prefixes` (strings the dictionary walks through
    /// without them being words) as well.
    fn build(words: &str, prefixes: &str) -> Trie {
        let mut nodes = vec![Node::default()];
        for p in prefixes.lines().filter(|l| !l.is_empty()) {
            let mut at = 0;
            for c in p.chars() {
                let len = nodes.len();
                at = *nodes[at].next.entry(c).or_insert(len);
                if at == len {
                    nodes.push(Node::default());
                }
            }
        }
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
        TRIE.get_or_init(|| {
            Trie::build(
                include_str!("../../data/thai-words.txt"),
                include_str!("../../data/thai-prefixes.txt"),
            )
        })
    }
}

/// Whether `c` is a Thai letter, vowel or mark that the dictionary walks over (U+0E01 to U+0E3A
/// and U+0E40 to U+0E4E, without the lakkhangyao U+0E45, which is a word of its own).
pub fn thai_letter(c: char) -> bool {
    matches!(c, '\u{0E01}'..='\u{0E3A}' | '\u{0E40}'..='\u{0E44}' | '\u{0E46}'..='\u{0E4E}')
}

/// Whether `c` is a format character (but the soft hyphen) or a combining mark that is not a
/// Thai letter: it stays inside the word around it.
fn ignorable(c: char) -> bool {
    const MN: usize = 6;
    const ME: usize = 8;
    const CF: usize = 27;
    c != '\u{AD}' && !thai_letter(c) && matches!(props::category(c), MN | ME | CF)
}

/// Whether the dictionary has no column for `c`: a character to ignore, or one of the Thai
/// characters outside its alphabet (`ฃ ฅ ํ ๎`, asked of the Java runtime of `refsearch`). At the end of
/// a word the walk stops there and the rest of the segment joins that word.
fn outside(c: char) -> bool {
    matches!(c, '\u{0E03}' | '\u{0E05}' | '\u{0E4D}' | '\u{0E4E}') || ignorable(c)
}

/// The ends of the words of a run of Thai letters, the last one at `text.len()`. A format
/// character or combining mark right after a whole word ends the walk: the rest of the run joins
/// that word.
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
        let word = trie.nodes[node].word;
        if word {
            possible.push(i);
        }
        if i < end && outside(text[i]) && word {
            break;
        }
        let next = match text.get(i) {
            Some(c) if !outside(*c) => trie.nodes[node].next.get(c).copied(),
            _ => None,
        };
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
                        current.pop();
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
            current.pop();
            break;
        }
    }
    current.push(end);
    current
}

/// Paiyannoi, the abbreviation sign, and maiyamok, the repetition sign: no dictionary characters
/// for Java's word rules, which join them to the run before them.
const PAIYANNOI: char = '\u{0E2F}';
const MAIYAMOK: char = '\u{0E46}';
const LO_LING: char = '\u{0E25}';

/// Whether `c` is a dictionary character of Java's Thai word rules
/// (`[\u0e01-\u0e2e\u0e30-\u0e3a\u0e40-\u0e44\u0e47-\u0e4e]`).
fn dictionary(c: char) -> bool {
    matches!(c, '\u{0E01}'..='\u{0E2E}' | '\u{0E30}'..='\u{0E3A}' | '\u{0E40}'..='\u{0E44}' | '\u{0E47}'..='\u{0E4E}')
}

/// Whether `ฯลฯ` starts at `i`.
fn etc(chars: &[(usize, char)], i: usize) -> bool {
    let at = |k: usize| chars.get(i + k).map(|&(_, c)| c);
    at(0) == Some(PAIYANNOI) && at(1) == Some(LO_LING) && at(2) == Some(PAIYANNOI)
}

/// The end of the segment that Java's Thai word rules start at `i`: `ฯลฯ`; or a run of
/// dictionary characters (the characters to ignore inside it), then `ๆ` or `ฯๆ`, or `ฯ` when
/// the character after it is neither `ล` nor one to ignore, or is `ล` followed by neither `ฯ` nor
/// one to ignore.
fn segment_end(chars: &[(usize, char)], i: usize) -> usize {
    if etc(chars, i) {
        return i + 3;
    }
    let at = |k: usize| chars.get(k).map(|&(_, c)| c);
    let mut j = i;
    while at(j).is_some_and(|c| dictionary(c) || ignorable(c)) {
        j += 1;
    }
    let free = |c: Option<char>| c.is_some_and(|c| !ignorable(c));
    match (at(j), at(j + 1)) {
        (Some(MAIYAMOK), _) => j + 1,
        (Some(PAIYANNOI), Some(MAIYAMOK)) => j + 2,
        (Some(PAIYANNOI), Some(LO_LING)) if free(at(j + 2)) && at(j + 2) != Some(PAIYANNOI) => {
            j + 1
        }
        (Some(PAIYANNOI), next) if free(next) && next != Some(LO_LING) => j + 1,
        _ => j,
    }
}

/// The dictionary characters Java's rule-based iterator looks up to find the segment
/// `start..end`: its own and those it reads past its end before its rules stop, the character a
/// `ฯ` looks ahead to (two after `ล`) and the one after it, or the one after `ๆ`.
fn looked_up(chars: &[(usize, char)], start: usize, end: usize) -> usize {
    let at = |k: usize| chars.get(k).map(|&(_, c)| c);
    let ahead = |k: usize| if at(k) == Some(LO_LING) { 2 } else { 1 };
    let past = match (at(end - 1), at(end)) {
        (Some(PAIYANNOI), _) => ahead(end) + 1,
        (Some(MAIYAMOK), _) => 1,
        (_, Some(PAIYANNOI)) => 1 + ahead(end + 1),
        _ => 0,
    };
    (start..end + past).filter(|&k| at(k).is_some_and(dictionary)).count()
}

/// Whether `c` is a letter or a digit (general category L or Nd), as Java's
/// `Character.isLetterOrDigit` reads it.
fn letter_or_digit(c: char) -> bool {
    matches!(props::category(c), 1..=5 | 9)
}

/// Whether `c` is a decimal digit.
fn digit(c: char) -> bool {
    props::category(c) == 9
}

/// The signs that may close a word ending in a digit, measured on `refsearch` 1.70.6.
const NUMBER_SIGNS: [char; 7] =
    ['%', '&', '\u{A2}', '\u{2030}', '\u{2031}', '\u{066A}', '\u{0E2F}'];

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
        if dictionary(c) || (c == PAIYANNOI && etc(&chars, i)) {
            let start = i;
            i = segment_end(&chars, i);
            let run: Vec<char> = chars[start..i].iter().map(|&(_, c)| c).collect();
            // Java divides a segment with the dictionary only when its rules looked up two
            // dictionary characters or more: `ปฯ,` stays whole, `ปฯ พ` does not.
            let pieces = match looked_up(&chars, start, i) {
                0 | 1 => vec![run.len()],
                _ => breaks(&run, trie),
            };
            let mut from = 0;
            for b in pieces {
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

/// The end of the word that starts with the letter or digit at `i`; format characters and
/// combining marks inside it are skipped over.
fn word_end(chars: &[(usize, char)], mut i: usize) -> usize {
    let part = |c: char| letter_or_digit(c) && !thai_letter(c);
    let skip = |mut j: usize| {
        while j < chars.len() && ignorable(chars[j].1) {
            j += 1;
        }
        j
    };
    loop {
        let before = chars[i].1;
        i = skip(i + 1);
        let Some(&(_, mid)) = chars.get(i) else { break };
        if part(mid) {
            continue;
        }
        let after_at = skip(i + 1);
        if let Some(&(_, after)) = chars.get(after_at).filter(|&&(_, a)| part(a)) {
            let letters = !digit(before) && !digit(after) && matches!(mid, '-' | '.' | '\'');
            let digits = digit(before) && digit(after) && matches!(mid, '.' | ',');
            if letters || digits {
                i = after_at;
                continue;
            }
        }
        if digit(before) && NUMBER_SIGNS.contains(&mid) {
            i = after_at;
        }
        break;
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

    #[test]
    fn keeps_one_sign_after_a_number() {
        // Measured on `refsearch` 1.70.6: one of `% & ¢ ‰ ٪ ‱`, or `ฯ`, closes a word that ends in a digit.
        assert_eq!(
            cut("1% 1%% 1%2 a1% a% 1.% 1,5% 1& 1‰ 4ฯ"),
            ["1%", "1%", "1%", "2", "a1%", "a", "1", "1,5%", "1&", "1‰", "4ฯ"]
        );
    }

    #[test]
    fn format_and_combining_marks_join_the_word() {
        // Measured: format characters (but the soft hyphen) and combining marks stay inside a
        // word; after a whole dictionary word they join the rest of the Thai run to it.
        assert_eq!(
            cut("ab\u{200B}cd 4\u{200B}5 1\u{200B}.5 ab\u{200B}% \u{200B}ab"),
            ["ab\u{200B}cd", "4\u{200B}5", "1\u{200B}.5", "ab\u{200B}", "ab"]
        );
        assert_eq!(cut("ภาษา\u{200B}ภาษา"), ["ภาษา\u{200B}ภาษา"]);
        assert_eq!(cut("ไทยภาษา\u{200B}ไทย"), ["ไทย", "ภาษา\u{200B}ไทย"]);
        assert_eq!(cut("\u{200B}ภาษาไทย"), ["ภาษา", "ไทย"]);
        assert_eq!(cut("ภาษา\u{200B}"), ["ภาษา\u{200B}"]);
        assert_eq!(cut("ภาษา\u{AD}ภาษา"), ["ภาษา", "ภาษา"]);
    }

    #[test]
    fn lakkhangyao_is_a_word_of_its_own() {
        assert_eq!(cut("ฤๅเดช"), ["ฤ", "ๅ", "เดช"]);
        assert_eq!(cut("ฦๅ"), ["ฦ", "ๅ"]);
    }
}
