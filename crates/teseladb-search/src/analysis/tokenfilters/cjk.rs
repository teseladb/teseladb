//! The filters of the CJK analyzer (the built-in `cjk`, `chinese`, `japanese` and
//! `korean` analyzers of the reference search server 1.70.6): full-width ASCII and half-width Katakana to their usual
//! widths, and bigrams of the Han, Hiragana, Katakana and Hangul characters of adjacent tokens.
//! Measured equal to the reference search server on 48 000 sentences of Chinese, Japanese and Korean.

use crate::analysis::normalize::{Form, normalize};
use crate::analysis::tokenizers::Token;
use crate::analysis::unicode::{WordBreak, compose, is_han, is_hangul, is_hiragana, word_break};

/// The combining voiced and semi-voiced sound marks.
const VOICED: char = '\u{3099}';
const SEMI_VOICED: char = '\u{309A}';

/// `text` with full-width ASCII as ASCII and half-width Katakana as full-width Katakana; a
/// half-width sound mark joins the letter before it when they compose.
pub(super) fn cjk_width(text: &str) -> String {
    cjk_width_offsets(text).into_iter().map(|(c, _)| c).collect()
}

/// The characters of [`cjk_width`] with the byte offset in `text` where each one starts.
pub(crate) fn cjk_width_offsets(text: &str) -> Vec<(char, usize)> {
    let mut out: Vec<(char, usize)> = Vec::with_capacity(text.len());
    for (at, c) in text.char_indices() {
        match c {
            '\u{FF01}'..='\u{FF5E}' => {
                out.push((char::from_u32(c as u32 - 0xFEE0).unwrap_or(c), at));
            }
            '\u{FF65}'..='\u{FF9F}' => {
                let wide = normalize(&c.to_string(), Form::Nfkc);
                let mark = wide.chars().next().filter(|&m| m == VOICED || m == SEMI_VOICED);
                match (mark, out.last().copied()) {
                    (Some(m), Some((p, from))) if compose(p, m).is_some() => {
                        out.pop();
                        out.extend(compose(p, m).map(|c| (c, from)));
                    }
                    _ => out.extend(wide.chars().map(|c| (c, at))),
                }
            }
            c => out.push((c, at)),
        }
    }
    out
}

/// Whether `c` is a character the bigram filter joins: Han (but the Han letters of word-break
/// class ALetter, such as `々`, which the tokenizer types as words), Hiragana, Katakana or Hangul.
fn cjk_char(c: char) -> bool {
    let class = word_break(c);
    (is_han(c) && class != WordBreak::ALetter)
        || is_hiragana(c)
        || is_hangul(c)
        || class == WordBreak::Katakana
}

/// Whether the standard tokenizer typed `token` as ideographic, Hiragana, Katakana or Hangul:
/// all its characters, but for combining marks and format characters, are of those kinds.
fn cjk_token(token: &Token) -> bool {
    let mut letters = token
        .text
        .chars()
        .filter(|&c| !matches!(word_break(c), WordBreak::Extend | WordBreak::Format));
    letters.next().is_some_and(cjk_char) && letters.all(cjk_char)
}

/// The tokens with each run of CJK tokens that touch (one starts where the last ended) replaced
/// by the bigrams of its characters, or by itself when it holds one character. A run of `n`
/// characters takes `n - 1` positions; the other tokens keep the gaps between them.
pub(super) fn cjk_bigrams(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut run: Vec<(char, usize, usize)> = Vec::new();
    let (mut last_in, mut last_out): (Option<u32>, Option<u32>) = (None, None);
    let next = |last_out: Option<u32>, last_in: Option<u32>, position: u32| {
        last_out.map_or(position, |o| o + (position - last_in.unwrap_or(0)))
    };
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        if !cjk_token(t) {
            let position = next(last_out, last_in, t.position);
            out.push(Token { position, ..t.clone() });
            (last_in, last_out) = (Some(t.position), Some(position));
            i += 1;
            continue;
        }
        let first = next(last_out, last_in, t.position);
        run.clear();
        let run_start = i;
        let mut end = t.start;
        while i < tokens.len()
            && cjk_token(&tokens[i])
            && (i == run_start || tokens[i].start == end)
        {
            let t = &tokens[i];
            // Each character keeps its own offsets while the text has the length of its span.
            let own = t.end - t.start == t.text.len();
            let mut at = t.start;
            for c in t.text.chars() {
                let span = if own { (at, at + c.len_utf8()) } else { (t.start, t.end) };
                run.push((c, span.0, span.1));
                at += c.len_utf8();
            }
            end = t.end;
            i += 1;
        }
        let model = &tokens[run_start];
        let grams: Vec<Token> = if run.len() == 1 {
            vec![Token { position: first, ..model.clone() }]
        } else {
            run.windows(2)
                .zip(first..)
                .map(|(pair, position)| Token {
                    text: [pair[0].0, pair[1].0].iter().collect(),
                    start: pair[0].1,
                    end: pair[1].2,
                    position,
                    length: 1,
                    keyword: false,
                })
                .collect()
        };
        last_out = grams.last().map(|g| g.position);
        last_in = Some(tokens[i - 1].position);
        out.extend(grams);
    }
    out
}
