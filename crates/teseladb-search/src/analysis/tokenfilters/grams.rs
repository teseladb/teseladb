//! `edgeGram`, `nGram` and `shingle`. The grams of a token stay at its position, by start and
//! then by length, counted in characters; with `termNotInBounds: include` a token shorter than
//! `minGram` or longer than `maxGram` follows its grams. A shingle starts at the position of its
//! first token and spans the positions up to its last token's (`n - 1` for `n` tokens in a row).
//! Positions left empty by an earlier filter, also before the first token and after the last,
//! become `_` tokens right after the token before them, at most `maxShingleSize - 1` of them per
//! gap, and no shingle is made of `_` alone. Tokens stacked at one position (edge grams) count as
//! tokens in a row, as RefLib's `ShingleFilter` reads them (measured on `refsearch`: the query graph
//! of `h he hel` has the paths `h he` `he hel` and `h he hel`), so every later position moves by
//! the tokens stacked before it.

use super::{Original, with_text};
use crate::analysis::tokenizers::Token;

/// The text that stands for an empty position inside a shingle.
const FILLER: &str = "_";

/// The grams of each token.
pub(super) fn grams(
    tokens: Vec<Token>,
    min: usize,
    max: usize,
    out_of_bounds: Original,
    edge: bool,
) -> Vec<Token> {
    let mut out = Vec::new();
    for t in tokens {
        let bounds: Vec<usize> =
            t.text.char_indices().map(|(i, _)| i).chain([t.text.len()]).collect();
        let count = bounds.len() - 1;
        let starts = if edge { count.min(1) } else { count };
        for start in 0..starts {
            for len in min..=max.min(count - start) {
                out.push(with_text(&t, t.text[bounds[start]..bounds[start + len]].to_string()));
            }
        }
        if out_of_bounds == Original::Include && !(min..=max).contains(&count) {
            out.push(t);
        }
    }
    out
}

/// The shingles of `min..=max` tokens.
pub(super) fn shingles(tokens: &[Token], min: usize, max: usize, end: u32) -> Vec<Token> {
    let mut line: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut next = 0;
    let mut stacked = 0;
    let mut before: Option<u32> = None;
    for t in tokens {
        if before == Some(t.position) {
            stacked += 1;
        }
        before = Some(t.position);
        let t = &Token { position: t.position + stacked, ..t.clone() };
        for position in (next..t.position).take(max - 1) {
            line.push(Token {
                text: FILLER.to_string(),
                start: t.start,
                end: t.start,
                position,
                ..t.clone()
            });
        }
        next = next.max(t.position + 1);
        line.push(t.clone());
    }
    if let Some(last) = tokens.last() {
        for position in (next..end + stacked).take(max - 1) {
            line.push(Token {
                text: FILLER.to_string(),
                start: last.end,
                end: last.end,
                position,
                ..last.clone()
            });
        }
    }
    let filler = |t: &Token| t.text == FILLER && t.start == t.end;
    let mut out = Vec::new();
    for (i, first) in line.iter().enumerate() {
        for n in min..=max.min(line.len() - i) {
            let run = &line[i..i + n];
            if run.iter().all(filler) {
                continue;
            }
            let text: Vec<&str> = run.iter().map(|t| t.text.as_str()).collect();
            out.push(Token {
                text: text.join(" "),
                start: first.start,
                end: run[n - 1].end,
                position: first.position,
                length: run[n - 1].position - first.position,
                keyword: false,
            });
        }
    }
    out
}
