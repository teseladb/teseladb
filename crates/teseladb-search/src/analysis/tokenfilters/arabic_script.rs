//! The normalizers and stemmers of the Persian and Sorani analyzers, measured equal to the reference search server
//! 1.70.6 on the vocabulary of each language (recorded data, `analyzers/vocab/`) and, for
//! the Persian stemmer, on 30 330 words made of a stem and up to five endings.

use super::light::ends;
use crate::analysis::props::category;

/// The general category `Cf` in the tables of [`category`].
const FORMAT: usize = 27;

/// `text` with the Persian letter variants as the Arabic letters: Farsi yeh and yeh barree as
/// yeh, keheh as kaf, heh with hamza and heh goal as heh, and without the hamza above.
pub(super) fn persian_normal(text: &str) -> String {
    text.chars()
        .filter(|&c| c != '\u{654}')
        .map(|c| match c {
            '\u{6CC}' | '\u{6D2}' => '\u{64A}',
            '\u{6A9}' => '\u{643}',
            '\u{6C0}' | '\u{6C1}' | '\u{6C2}' => '\u{647}',
            c => c,
        })
        .collect()
}

/// The Persian endings, removed in this order from the end of the word, each at most once and
/// only when two letters are left: `ات`, `ان`, `ترين`, `تر`, three times `ي`, and `ها`.
const PERSIAN_ENDINGS: &[&str] = &[
    "\u{627}\u{62A}",
    "\u{627}\u{646}",
    "\u{62A}\u{631}\u{64A}\u{646}",
    "\u{62A}\u{631}",
    "\u{64A}",
    "\u{64A}",
    "\u{64A}",
    "\u{647}\u{627}",
];

/// The Persian stem of a normalized word: [`PERSIAN_ENDINGS`] removed in turn.
pub(super) fn persian_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    for ending in PERSIAN_ENDINGS {
        let n = ending.chars().count();
        if ends(&w, ending) && w.len() >= n + 2 {
            w.truncate(w.len() - n);
        }
    }
    w.into_iter().collect()
}

/// The Sorani letter `ە` (ae).
const AE: char = '\u{6D5}';

/// `text` in the Sorani spelling: yeh and alef maksura as Farsi yeh, kaf as keheh, a final heh or
/// one before a zero-width non-joiner (which goes) as `ە`, teh marbuta as `ە`, heh doachashmee as
/// heh, an initial reh as `ڕ`, and without fatha, damma, kasra, sukun, tatweel and format
/// characters.
pub(super) fn sorani_normal(text: &str) -> String {
    let c: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\u{64A}' | '\u{649}' => out.push('\u{6CC}'),
            '\u{643}' => out.push('\u{6A9}'),
            '\u{647}' if i + 1 == c.len() => out.push(AE),
            '\u{647}' if c[i + 1] == '\u{200C}' => {
                out.push(AE);
                i += 1;
            }
            '\u{629}' => out.push(AE),
            '\u{6BE}' => out.push('\u{647}'),
            '\u{631}' if i == 0 => out.push('\u{695}'),
            '\u{64E}' | '\u{64F}' | '\u{650}' | '\u{652}' | '\u{640}' => {}
            ch if category(ch) == FORMAT => {}
            ch => out.push(ch),
        }
        i += 1;
    }
    out
}

/// The Sorani noun endings: the ending, the letters the word must exceed and the letters removed
/// (`ایە` and `ەیە` lose only `یە`).
const SORANI_ENDINGS: &[(&str, usize, usize)] = &[
    ("\u{6CE}\u{6A9}\u{6CC}", 6, 3),
    ("\u{6CC}\u{6D5}\u{6A9}\u{6CC}", 7, 4),
    ("\u{6CE}\u{6A9}", 5, 2),
    ("\u{6CC}\u{6D5}\u{6A9}", 6, 3),
    ("\u{6D5}\u{6A9}\u{6D5}", 6, 3),
    ("\u{6A9}\u{6D5}", 5, 2),
    ("\u{6D5}\u{6A9}\u{627}\u{646}", 7, 4),
    ("\u{6A9}\u{627}\u{646}", 6, 3),
    ("\u{6CC}\u{627}\u{646}\u{6CC}", 7, 4),
    ("\u{627}\u{646}\u{6CC}", 6, 3),
    ("\u{6CC}\u{627}\u{646}", 6, 3),
    ("\u{627}\u{646}", 5, 2),
    ("\u{6CC}\u{627}\u{646}\u{6D5}", 7, 4),
    ("\u{627}\u{646}\u{6D5}", 6, 3),
    ("\u{627}\u{6CC}\u{6D5}", 5, 2),
    ("\u{6D5}\u{6CC}\u{6D5}", 5, 2),
    ("\u{6D5}", 4, 1),
    ("\u{6CC}", 4, 1),
];

/// The Sorani stem: a postposition (`دا`, the `ا` of `نا`, `ەوە`), then a possessive (`مان`,
/// `یان`, `تان`), then the first ending of [`SORANI_ENDINGS`].
pub(super) fn sorani_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    let n = w.len();
    if n > 5 && ends(&w, "\u{62F}\u{627}") {
        w.truncate(n - 2);
    } else if n > 4 && ends(&w, "\u{646}\u{627}") {
        w.truncate(n - 1);
    } else if n > 6 && ends(&w, "\u{6D5}\u{648}\u{6D5}") {
        w.truncate(n - 3);
    }
    let possessives = ["\u{645}\u{627}\u{646}", "\u{6CC}\u{627}\u{646}", "\u{62A}\u{627}\u{646}"];
    if w.len() > 6 && possessives.iter().any(|e| ends(&w, e)) {
        w.truncate(w.len() - 3);
    }
    let ending =
        SORANI_ENDINGS.iter().find(|(e, more_than, _)| w.len() > *more_than && ends(&w, e));
    if let Some(&(_, _, cut)) = ending {
        w.truncate(w.len() - cut);
    }
    w.into_iter().collect()
}
