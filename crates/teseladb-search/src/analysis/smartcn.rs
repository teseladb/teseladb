//! The tokenizer of the built-in `smartcn` analyzer (011 T009, D-109). The reference search library's smartcn cuts Chinese with a
//! dictionary and a bigram model that exist only inside the reference search library; TeselaDB cuts each run of Han
//! characters with the ICU 74.2 Chinese and Japanese dictionary instead (`data/smartcn-words.txt`,
//! written by `scripts/gen-smartcn-dict.py`): the path of least total cost, where a dictionary word
//! costs its ICU cost and a character outside the dictionary costs 255. Words with characters of
//! traditional Chinese are left out of the dictionary, since smartcn only knows simplified words.
//! Runs of other letters and of digits are words of their own, and everything else is dropped.
//! The tokens differ from the reference search server's in part of the sentences; the measured agreement is recorded
//! as a divergence for 011 (the recorded sentence comparison).

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::analysis::props;
use crate::analysis::unicode::is_han;

/// The cost of a character that starts no dictionary word.
const UNKNOWN_COST: u32 = 255;

/// The dictionary: each word with its cost, and the longest word in characters.
struct Words {
    cost: HashMap<String, u32>,
    longest: usize,
}

impl Words {
    fn get() -> &'static Words {
        static WORDS: OnceLock<Words> = OnceLock::new();
        WORDS.get_or_init(|| {
            let mut cost = HashMap::new();
            let mut longest = 1;
            for line in include_str!("../../data/smartcn-words.txt").lines() {
                if line.starts_with('#') {
                    continue;
                }
                let (word, c) = line.split_once('\t').unwrap_or((line, ""));
                longest = longest.max(word.chars().count());
                cost.insert(word.to_string(), c.parse().unwrap_or(UNKNOWN_COST));
            }
            Words { cost, longest }
        })
    }
}

/// The ends of the cheapest cut of `run` into dictionary words and single characters.
fn cut(run: &[char], words: &Words) -> Vec<usize> {
    let n = run.len();
    let mut best = vec![u32::MAX; n + 1];
    let mut back = vec![0; n + 1];
    best[0] = 0;
    for i in 0..n {
        if best[i] == u32::MAX {
            continue;
        }
        let mut word = String::new();
        for j in i + 1..=n.min(i + words.longest) {
            word.push(run[j - 1]);
            let cost = match words.cost.get(&word) {
                Some(&c) => c,
                None if j == i + 1 => UNKNOWN_COST,
                None => continue,
            };
            if best[i] + cost < best[j] {
                best[j] = best[i] + cost;
                back[j] = i;
            }
        }
    }
    let mut ends = Vec::new();
    let mut j = n;
    while j > 0 {
        ends.push(j);
        j = back[j];
    }
    ends.reverse();
    ends
}

/// Whether `c` is a letter (general category L) that is not Han.
fn letter(c: char) -> bool {
    matches!(props::category(c), 1..=5) && !is_han(c)
}

/// Whether `c` is a decimal digit.
fn digit(c: char) -> bool {
    props::category(c) == 9
}

/// The byte ranges of the tokens of `text`.
pub fn smartcn_words(text: &str) -> Vec<(usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |i: usize| chars.get(i).map_or(text.len(), |&(b, _)| b);
    let words = Words::get();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i].1;
        let start = i;
        let kind: fn(char) -> bool = if is_han(c) {
            is_han
        } else if letter(c) {
            letter
        } else if digit(c) {
            digit
        } else {
            i += 1;
            continue;
        };
        while i < chars.len() && kind(chars[i].1) {
            i += 1;
        }
        if is_han(c) {
            let run: Vec<char> = chars[start..i].iter().map(|&(_, c)| c).collect();
            let mut from = 0;
            for end in cut(&run, words) {
                out.push((at(start + from), at(start + end)));
                from = end;
            }
        } else {
            out.push((at(start), at(i)));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens(text: &str) -> Vec<&str> {
        smartcn_words(text).into_iter().map(|(a, b)| &text[a..b]).collect()
    }

    #[test]
    fn cuts_runs() {
        assert_eq!(tokens("04年的Up And Down，"), ["04", "年", "的", "Up", "And", "Down"]);
        // Traditional characters stay single.
        assert_eq!(tokens("運動"), ["運", "動"]);
        assert!(tokens("基督教徒").len() < 4);
    }
}
