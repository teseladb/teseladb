//! The word a Snowball stemmer works on: a vector of characters with the suffix tests and
//! rewrites the algorithm descriptions of snowballstem.org are written with.

/// A word being stemmed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Word {
    /// The characters.
    pub c: Vec<char>,
}

impl Word {
    /// The word of `text`.
    pub fn new(text: &str) -> Word {
        Word { c: text.chars().collect() }
    }

    /// The length in characters.
    pub fn len(&self) -> usize {
        self.c.len()
    }

    /// Whether the word is empty.
    pub fn is_empty(&self) -> bool {
        self.c.is_empty()
    }

    /// The text.
    pub fn text(&self) -> String {
        self.c.iter().collect()
    }

    /// Whether the word ends with `suffix`.
    pub fn ends(&self, suffix: &str) -> bool {
        let n = suffix.chars().count();
        n <= self.c.len() && self.c[self.c.len() - n..].iter().copied().eq(suffix.chars())
    }

    /// Whether the word starts with `prefix`.
    pub fn starts(&self, prefix: &str) -> bool {
        let n = prefix.chars().count();
        n <= self.c.len() && self.c[..n].iter().copied().eq(prefix.chars())
    }

    /// The longest of `suffixes` the word ends with.
    pub fn longest<'a>(&self, suffixes: &[&'a str]) -> Option<&'a str> {
        suffixes.iter().copied().filter(|s| self.ends(s)).max_by_key(|s| s.chars().count())
    }

    /// The longest of `suffixes` the word ends with whose start is at or after `from`.
    pub fn longest_from<'a>(&self, suffixes: &[&'a str], from: usize) -> Option<&'a str> {
        self.longest(suffixes).filter(|s| self.start_of(s) >= from)
    }

    /// The longest of `suffixes` that lies wholly at or after `from`.
    pub fn longest_within<'a>(&self, suffixes: &[&'a str], from: usize) -> Option<&'a str> {
        suffixes
            .iter()
            .copied()
            .filter(|s| self.ends(s) && self.start_of(s) >= from)
            .max_by_key(|s| s.chars().count())
    }

    /// Where `suffix`, which the word ends with, starts.
    pub fn start_of(&self, suffix: &str) -> usize {
        self.c.len() - suffix.chars().count()
    }

    /// Removes the last `n` characters.
    pub fn cut(&mut self, n: usize) {
        self.c.truncate(self.c.len() - n);
    }

    /// Replaces `suffix`, which the word ends with, by `by`.
    pub fn replace(&mut self, suffix: &str, by: &str) {
        self.cut(suffix.chars().count());
        self.c.extend(by.chars());
    }

    /// The character `back` places from the end (0 for the last one).
    pub fn back(&self, back: usize) -> Option<char> {
        self.c.len().checked_sub(back + 1).map(|i| self.c[i])
    }

    /// The character at `i`.
    pub fn at(&self, i: usize) -> Option<char> {
        self.c.get(i).copied()
    }
}

/// The standard region R1 from `start`: the region after the first non-vowel that follows a
/// vowel, or the end of the word.
pub fn region(c: &[char], start: usize, vowel: impl Fn(char) -> bool) -> usize {
    let mut i = start;
    while i < c.len() && !vowel(c[i]) {
        i += 1;
    }
    while i < c.len() && vowel(c[i]) {
        i += 1;
    }
    (i + 1).min(c.len()).max(start.min(c.len()))
}

/// R1 and R2 of `c`.
pub fn regions(c: &[char], vowel: impl Fn(char) -> bool + Copy) -> (usize, usize) {
    let r1 = region(c, 0, vowel);
    (r1, region(c, r1, vowel))
}

/// The region RV of the Romance stemmers: after the next vowel when the second letter is a
/// consonant, after the next consonant when the first two are vowels, otherwise after the third
/// letter; the end of the word when there is no such position.
pub fn rv(c: &[char], vowel: impl Fn(char) -> bool) -> usize {
    if c.len() < 2 {
        return c.len();
    }
    let next = |from: usize, want_vowel: bool| {
        (from..c.len()).find(|&i| vowel(c[i]) == want_vowel).map_or(c.len(), |i| i + 1)
    };
    if !vowel(c[1]) {
        next(2, true)
    } else if vowel(c[0]) {
        next(2, false)
    } else {
        3.min(c.len())
    }
}
