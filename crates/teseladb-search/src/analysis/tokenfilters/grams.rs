//! `edgeGram`, `nGram` and `shingle`. The grams of a token stay at its position, by start and
//! then by length, counted in characters; with `termNotInBounds: include` a token shorter than
//! `minGram` or longer than `maxGram` follows its grams. A shingle starts at the position of its
//! first token and spans the positions up to its last token's (`n - 1` for `n` tokens in a row).
//! Positions left empty by an earlier filter, also before the first token and after the last,
//! become `_` tokens right after the token before them, at most `maxShingleSize - 1` of them per
//! gap, and no shingle is made of `_` alone. Tokens stacked at one position (edge grams) count as
//! tokens in a row, as the reference search library's `ShingleFilter` reads them (measured on the reference search server: the query graph
//! of `h he hel` has the paths `h he` `he hel` and `h he hel`), so every later position moves by
//! the tokens stacked before it.

use super::Original;
use crate::analysis::scratch::Pooled;
use crate::analysis::tokens::{Meta, Tokens};

/// The text that stands for an empty position inside a shingle.
const FILLER: &str = "_";

/// Writes the grams of each token of `input` into `out`.
pub(super) fn grams(
    input: &Tokens,
    out: &mut Tokens,
    min: usize,
    max: usize,
    out_of_bounds: Original,
    edge: bool,
) {
    let mut bounds: Pooled<usize> = Pooled::new();
    for i in 0..input.len() {
        let (text, meta) = (input.text(i), input.meta(i));
        bounds.clear();
        bounds.extend(text.char_indices().map(|(i, _)| i).chain([text.len()]));
        let count = bounds.len() - 1;
        let starts = if edge { count.min(1) } else { count };
        for start in 0..starts {
            for len in min..=max.min(count - start) {
                out.push(&text[bounds[start]..bounds[start + len]], meta);
            }
        }
        if out_of_bounds == Original::Include && !(min..=max).contains(&count) {
            out.push(text, meta);
        }
    }
}

/// A token of the line shingles are cut from: the index of a token of the input, or a filler,
/// with its meta.
#[derive(Clone, Copy, Debug)]
struct Cell {
    token: Option<usize>,
    meta: Meta,
}

/// Writes the shingles of `min..=max` tokens of `input` into `out`.
pub(super) fn shingles(input: &Tokens, out: &mut Tokens, min: usize, max: usize) {
    let end = input.end;
    let mut line: Pooled<Cell> = Pooled::new();
    let mut next = 0;
    let mut stacked = 0;
    let mut before: Option<u32> = None;
    for i in 0..input.len() {
        let m = input.meta(i);
        if before == Some(m.position) {
            stacked += 1;
        }
        before = Some(m.position);
        let m = Meta { position: m.position + stacked, ..m };
        for position in (next..m.position).take(max - 1) {
            let meta = Meta { end: m.start, position, ..m };
            line.push(Cell { token: None, meta });
        }
        next = next.max(m.position + 1);
        line.push(Cell { token: Some(i), meta: m });
    }
    if let Some(last) = input.last_meta() {
        for position in (next..end + stacked).take(max - 1) {
            let meta = Meta { start: last.end, end: last.end, position, ..last };
            line.push(Cell { token: None, meta });
        }
    }
    let text = |c: &Cell| c.token.map_or(FILLER, |i| input.text(i));
    let filler = |c: &Cell| text(c) == FILLER && c.meta.start == c.meta.end;
    for (i, first) in line.iter().enumerate() {
        for n in min..=max.min(line.len() - i) {
            let run = &line[i..i + n];
            if run.iter().all(filler) {
                continue;
            }
            let meta = Meta {
                start: first.meta.start,
                end: run[n - 1].meta.end,
                position: first.meta.position,
                length: run[n - 1].meta.position - first.meta.position,
                keyword: false,
            };
            out.push_with(meta, |o| {
                for (k, c) in run.iter().enumerate() {
                    if k > 0 {
                        o.push(' ');
                    }
                    o.push_str(text(c));
                }
            });
        }
    }
}

crate::analysis::scratch::pools! {
    CELLS: Cell;
}
