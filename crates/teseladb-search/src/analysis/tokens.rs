//! The tokens of one text with all their texts in one string (FR-028): a tokenizer writes into a
//! [`Tokens`], each token filter reads one and writes another, and the two keep their capacity
//! from text to text, so analyzing a text again allocates nothing.

use super::tokenizers::Token;

/// Where a token is and what it is, without its text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Meta {
    /// Byte offset of its start in the original text.
    pub start: usize,
    /// Byte offset of its end in the original text.
    pub end: usize,
    /// Position, from 0, counting the gaps that filters leave.
    pub position: u32,
    /// Positions the token spans.
    pub length: u32,
    /// Marked as a keyword (`keywordRepeat`).
    pub keyword: bool,
}

/// One token of a [`Tokens`]: the byte range of its text in the shared string and its [`Meta`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot {
    from: usize,
    to: usize,
    meta: Meta,
}

/// The tokens of one text, in order, and the position after the last one the tokenizer gave,
/// which stays when a filter removes the last tokens.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    text: String,
    slots: Vec<Slot>,
    /// The position after the last token of the tokenizer, moved by filters that add positions.
    pub end: u32,
}

impl Tokens {
    /// Empties the list and keeps its capacity.
    pub fn clear(&mut self) {
        self.text.clear();
        self.slots.clear();
        self.end = 0;
    }

    /// The number of tokens.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether there is no token.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The text of token `i`.
    pub fn text(&self, i: usize) -> &str {
        let s = &self.slots[i];
        &self.text[s.from..s.to]
    }

    /// The [`Meta`] of token `i`.
    pub fn meta(&self, i: usize) -> Meta {
        self.slots[i].meta
    }

    /// The [`Meta`] of the last token.
    pub fn last_meta(&self) -> Option<Meta> {
        self.slots.last().map(|s| s.meta)
    }

    /// Changes the [`Meta`] of token `i`.
    pub fn set_meta(&mut self, i: usize, meta: Meta) {
        self.slots[i].meta = meta;
    }

    /// Appends a token.
    pub fn push(&mut self, text: &str, meta: Meta) {
        self.push_with(meta, |out| out.push_str(text));
    }

    /// Appends a token whose text is what `write` appends to the string it is given.
    pub fn push_with(&mut self, meta: Meta, write: impl FnOnce(&mut String)) {
        let from = self.text.len();
        write(&mut self.text);
        self.slots.push(Slot { from, to: self.text.len(), meta });
    }

    /// Appends a token with the text of the token being built, the bytes written since `from`
    /// ([`Tokens::mark`]).
    pub fn push_marked(&mut self, from: usize, meta: Meta) {
        self.slots.push(Slot { from, to: self.text.len(), meta });
    }

    /// Where the next token's text starts: a token can be written piece by piece with
    /// [`Tokens::write`] and then pushed with [`Tokens::push_marked`].
    pub fn mark(&self) -> usize {
        self.text.len()
    }

    /// The string the next token's text is appended to.
    pub fn write(&mut self) -> &mut String {
        &mut self.text
    }

    /// The text written since `from`, before it is pushed.
    pub fn written(&self, from: usize) -> &str {
        &self.text[from..]
    }

    /// Drops the text written since `from` that was not pushed.
    pub fn unwrite(&mut self, from: usize) {
        self.text.truncate(from);
    }

    /// Appends token `i` of `other`.
    pub fn push_from(&mut self, other: &Tokens, i: usize) {
        let meta = other.meta(i);
        self.push(other.text(i), meta);
    }

    /// The position after the last token, counting the positions each spans.
    pub fn span_end(&self) -> u32 {
        self.slots.iter().map(|s| s.meta.position + s.meta.length).max().unwrap_or(0)
    }

    /// The tokens as owned [`Token`]s.
    pub fn to_tokens(&self) -> Vec<Token> {
        (0..self.len())
            .map(|i| {
                let m = self.meta(i);
                Token {
                    text: self.text(i).to_owned(),
                    start: m.start,
                    end: m.end,
                    position: m.position,
                    length: m.length,
                    keyword: m.keyword,
                }
            })
            .collect()
    }

    /// The tokens of `tokens`, with the end their positions give.
    pub fn of(tokens: &[Token]) -> Tokens {
        let mut out = Tokens::default();
        for t in tokens {
            out.push(&t.text, meta_of(t));
        }
        out.end = out.span_end();
        out
    }
}

/// The [`Meta`] of `t`.
pub fn meta_of(t: &Token) -> Meta {
    Meta { start: t.start, end: t.end, position: t.position, length: t.length, keyword: t.keyword }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn tokens_share_one_string_and_keep_their_capacity() {
        let mut t = Tokens::default();
        let m = Meta { start: 0, end: 3, position: 0, length: 1, keyword: false };
        t.push("abc", m);
        t.push_with(Meta { position: 1, ..m }, |o| o.push_str("de"));
        let at = t.mark();
        t.write().push_str("xy");
        assert_eq!(t.written(at), "xy");
        t.unwrite(at);
        t.write().push('z');
        t.push_marked(at, Meta { position: 4, length: 2, ..m });
        assert_eq!((t.len(), t.text(1), t.text(2)), (3, "de", "z"));
        assert_eq!(t.span_end(), 6);
        assert_eq!(t.last_meta().map(|m| m.position), Some(4));
        t.set_meta(0, Meta { keyword: true, ..m });
        let owned = t.to_tokens();
        assert!(owned[0].keyword);
        let back = Tokens::of(&owned);
        assert_eq!((back.len(), back.end, back.text(2)), (3, 6, "z"));
        let mut copy = Tokens::default();
        copy.push_from(&back, 1);
        assert_eq!(copy.text(0), "de");
        let capacity = t.text.capacity();
        t.clear();
        assert!(t.is_empty());
        assert_eq!(t.text.capacity(), capacity);
    }
}
