//! `wordDelimiterGraph`, from the attribute descriptions of the reference search server documentation and
//! the behaviour measured on the reference search server 1.70.6.
//!
//! A token is cut into parts: letters (categories `L*` and the marks `M*`) and digits (`N*`) are
//! part characters and anything else is a delimiter. Inside a run of part characters a part ends
//! where a lowercase letter meets an uppercase one (`splitOnCaseChange`; an uppercase letter
//! followed by a lowercase one stays together, so `XMLHttpRequest` gives `XMLHttp` and `Request`)
//! and where a letter meets a digit (`splitOnNumerics`). The `s` of an `'s` that follows a letter
//! and ends the token or comes before a delimiter is dropped (`stemEnglishPossessive`; only with
//! the ASCII apostrophe, and not after a digit: `3's` gives `3` and `s`). A part that starts with
//! a letter is a word part, the others number parts.
//!
//! The parts take consecutive positions. Joined tokens span the parts they join: each run of two
//! or more word parts (`concatenateWords`) or number parts (`concatenateNumbers`), all the parts
//! when there are two or more (`concatenateAll`, unless every part is a word part joined already
//! by `concatenateWords` or a number part joined by `concatenateNumbers`) and the original token
//! (`preserveOriginal`, unless it is its one part). The one part of a token is always emitted;
//! when there are more, parts not generated (`generateWordParts`, `generateNumberParts`) leave no
//! position. The tokens after a split one move along by the positions it took.

use crate::analysis::props::category;
use crate::analysis::scratch::Pooled;
use crate::analysis::tokens::{Meta, Tokens};

/// The `delimiterOptions` of `wordDelimiterGraph`, with the documented defaults.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DelimiterOptions {
    /// Emit word parts.
    pub generate_word_parts: bool,
    /// Emit number parts.
    pub generate_number_parts: bool,
    /// Join runs of word parts.
    pub concatenate_words: bool,
    /// Join runs of number parts.
    pub concatenate_numbers: bool,
    /// Join all the parts.
    pub concatenate_all: bool,
    /// Keep the original token.
    pub preserve_original: bool,
    /// Cut where a lowercase letter meets an uppercase one.
    pub split_on_case_change: bool,
    /// Cut where a letter meets a digit.
    pub split_on_numerics: bool,
    /// Remove a trailing `'s`.
    pub stem_english_possessive: bool,
    /// Leave keyword tokens whole.
    pub ignore_keywords: bool,
}

impl Default for DelimiterOptions {
    fn default() -> DelimiterOptions {
        DelimiterOptions {
            generate_word_parts: true,
            generate_number_parts: true,
            concatenate_words: false,
            concatenate_numbers: false,
            concatenate_all: false,
            preserve_original: false,
            split_on_case_change: true,
            split_on_numerics: true,
            stem_english_possessive: true,
            ignore_keywords: false,
        }
    }
}

/// Indexes of [`crate::analysis::props::CATEGORIES`].
const LU: usize = 1;
const LL: usize = 2;
const LAST_LETTER_OR_MARK: usize = 8;
const LAST_NUMBER: usize = 11;

/// What a character is to the splitter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Lower,
    Upper,
    Letter,
    Digit,
    Delimiter,
}

fn class(c: char) -> Class {
    match category(c) {
        LU => Class::Upper,
        LL => Class::Lower,
        k if (LU..=LAST_LETTER_OR_MARK).contains(&k) => Class::Letter,
        k if k <= LAST_NUMBER && k > LAST_LETTER_OR_MARK => Class::Digit,
        _ => Class::Delimiter,
    }
}

/// One part: its byte range in the token and whether it starts with a letter.
#[derive(Clone, Copy, Debug)]
struct Part {
    from: usize,
    to: usize,
    word: bool,
}

/// Whether `part` is the `s` of a possessive `'s`: an ASCII apostrophe right after a part that
/// ends in a letter, and nothing but a delimiter or the end after it.
fn is_possessive(text: &str, parts: &[Part], i: usize) -> bool {
    let p = parts[i];
    let before = i.checked_sub(1).map(|j| parts[j]);
    matches!(&text[p.from..p.to], "s" | "S")
        && text[..p.from].ends_with('\'')
        && before.is_some_and(|q| {
            q.to + 1 == p.from && text[..q.to].chars().next_back().is_some_and(char::is_alphabetic)
        })
        && text[p.to..].chars().next().is_none_or(|c| class(c) == Class::Delimiter)
}

/// Cuts `text` into parts.
fn parts(text: &str, options: &DelimiterOptions, out: &mut Vec<Part>) {
    let mut current: Option<(usize, Class, bool)> = None;
    for (at, c) in text.char_indices().chain([(text.len(), ' ')]) {
        let k = if at == text.len() { Class::Delimiter } else { class(c) };
        let letter = matches!(k, Class::Lower | Class::Upper | Class::Letter);
        if let Some((from, prev, word)) = current {
            let cut = k == Class::Delimiter
                || (options.split_on_case_change && prev == Class::Lower && k == Class::Upper)
                || (options.split_on_numerics && (prev == Class::Digit) != (k == Class::Digit));
            if !cut {
                current = Some((from, k, word));
                continue;
            }
            out.push(Part { from, to: at, word });
        }
        current = (k != Class::Delimiter).then_some((at, k, letter));
    }
}

/// The text of a token `wordDelimiterGraph` makes of one token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Text {
    /// The parts `from..to`, joined.
    Parts(usize, usize),
    /// The whole token.
    Whole,
}

/// A token `wordDelimiterGraph` makes of token `token` of the input: its text, its first part
/// and the parts it spans, and then its first node and the nodes it spans.
#[derive(Clone, Copy, Debug)]
struct Made {
    token: usize,
    text: Text,
    first: usize,
    count: usize,
}

/// Appends to `out` the tokens `wordDelimiterGraph` makes of `text`, with its parts in `parts`.
fn split(
    text: &str,
    token: usize,
    options: &DelimiterOptions,
    parts: &mut Vec<Part>,
    out: &mut Vec<Made>,
) {
    let mut all: Pooled<Part> = Pooled::new();
    self::parts(text, options, &mut all);
    // An `s` right after a dropped one is not a possessive (`straße's's` keeps the last `s`).
    parts.clear();
    let mut dropped = false;
    for i in 0..all.len() {
        dropped = !dropped && options.stem_english_possessive && is_possessive(text, &all, i);
        if !dropped {
            parts.push(all[i]);
        }
    }
    let made = |text: Text, first: usize, count: usize| Made { token, text, first, count };
    let generated = |p: &Part| {
        if p.word { options.generate_word_parts } else { options.generate_number_parts }
    };
    for (i, p) in parts.iter().enumerate() {
        if generated(p) || parts.len() == 1 {
            out.push(made(Text::Parts(i, i + 1), i, 1));
        }
    }
    let mut i = 0;
    while i < parts.len() {
        let word = parts[i].word;
        let end = parts[i..].iter().position(|p| p.word != word).map_or(parts.len(), |n| i + n);
        let wanted = if word { options.concatenate_words } else { options.concatenate_numbers };
        if wanted && end - i > 1 {
            out.push(made(Text::Parts(i, end), i, end - i));
        }
        i = end;
    }
    let all_words = parts.iter().all(|p| p.word);
    let all_numbers = parts.iter().all(|p| !p.word);
    let joined_already =
        (all_words && options.concatenate_words) || (all_numbers && options.concatenate_numbers);
    if options.concatenate_all && parts.len() > 1 && !joined_already {
        out.push(made(Text::Parts(0, parts.len()), 0, parts.len()));
    }
    let single = parts.len() == 1 && text[parts[0].from..parts[0].to] == *text;
    if options.preserve_original && !single {
        out.push(made(Text::Whole, 0, parts.len().max(1)));
    }
}

/// Writes the filter's tokens of `input` into `out`; `protected` says whether a token is in
/// `protectedWords`.
pub(super) fn apply(
    input: &Tokens,
    out: &mut Tokens,
    options: &DelimiterOptions,
    mut protected: impl FnMut(&str) -> bool,
) {
    // Positions each input position took, by position.
    let mut taken: Pooled<(u32, u32)> = Pooled::new();
    let mut made: Pooled<Made> = Pooled::new();
    let mut nodes: Pooled<usize> = Pooled::new();
    let mut parts: Pooled<Part> = Pooled::new();
    // The parts of each token, one after another, and where each token's start.
    let mut all_parts: Pooled<Part> = Pooled::new();
    let mut parts_at: Pooled<usize> = Pooled::new();
    for i in 0..input.len() {
        let (text, t) = (input.text(i), input.meta(i));
        let whole = protected(text) || (options.ignore_keywords && t.keyword);
        let from = made.len();
        parts.clear();
        if whole {
            made.push(Made { token: i, text: Text::Whole, first: 0, count: 1 });
        } else {
            split(text, i, options, &mut parts, &mut made);
        }
        parts_at.push(all_parts.len());
        all_parts.extend_from_slice(&parts);
        // Part indexes that start or end an emitted token become positions.
        nodes.clear();
        nodes.extend(made[from..].iter().flat_map(|m| [m.first, m.first + m.count]));
        nodes.sort_unstable();
        nodes.dedup();
        let span = nodes.len().saturating_sub(1).max(1) as u32;
        match taken.binary_search_by_key(&t.position, |&(p, _)| p) {
            Ok(k) => taken[k].1 = taken[k].1.max(span),
            Err(k) => taken.insert(k, (t.position, span)),
        }
        for m in made[from..].iter_mut() {
            let first = nodes.partition_point(|&x| x < m.first);
            let last = nodes.partition_point(|&x| x < m.first + m.count);
            (m.first, m.count) = (first, last - first);
        }
    }
    parts_at.push(all_parts.len());
    // How far each input position moves: the positions the ones before it took past one.
    let mut extra = 0;
    for entry in taken.iter_mut() {
        let span = entry.1;
        entry.1 = extra;
        extra += span - 1;
    }
    out.end += extra;
    for m in made.iter() {
        let (text, t) = (input.text(m.token), input.meta(m.token));
        let shift = taken.binary_search_by_key(&t.position, |&(p, _)| p).map_or(0, |k| taken[k].1);
        let meta =
            Meta { position: t.position + shift + m.first as u32, length: m.count as u32, ..t };
        let own = &all_parts[parts_at[m.token]..parts_at[m.token + 1]];
        out.push_with(meta, |o| match m.text {
            Text::Whole => o.push_str(text),
            Text::Parts(from, to) => {
                own[from..to].iter().for_each(|p| o.push_str(&text[p.from..p.to]))
            }
        });
    }
}

crate::analysis::scratch::pools! {
    PARTS: Part;
    MADE: Made;
    TAKEN: (u32, u32);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::analysis::props::CATEGORIES;

    #[test]
    fn category_indexes() {
        assert_eq!(CATEGORIES[LU], "Lu");
        assert_eq!(CATEGORIES[LL], "Ll");
        assert_eq!(CATEGORIES[LAST_LETTER_OR_MARK], "Me");
        assert_eq!(CATEGORIES[LAST_NUMBER], "No");
    }
}
