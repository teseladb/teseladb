//! Character filters of RefDb Search custom analyzers (FR-007): `htmlStrip`, `icuNormalize`,
//! `mapping` and `persian`. A filter rewrites the text and keeps, for every byte boundary of its
//! output, the byte of the original text it comes from, so tokens carry original offsets for
//! highlighting.

use std::collections::BTreeSet;

use super::normalize::nfkc_casefold;

/// Text after character filters, with the way back to the original offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Filtered {
    /// The filtered text.
    pub text: String,
    /// For each byte offset of `text` (and its end), the offset in the original text.
    origin: Vec<usize>,
}

impl Filtered {
    /// The unfiltered `text`.
    pub fn new(text: &str) -> Filtered {
        Filtered { text: text.to_string(), origin: (0..=text.len()).collect() }
    }

    /// The offset in the original text of byte `at` of the filtered text.
    pub fn original(&self, at: usize) -> usize {
        self.origin[at]
    }

    /// Builds the output of a filter piece by piece.
    fn builder(&self) -> Rewriter<'_> {
        Rewriter { input: self, text: String::new(), origin: Vec::new() }
    }
}

struct Rewriter<'a> {
    input: &'a Filtered,
    text: String,
    origin: Vec<usize>,
}

impl Rewriter<'_> {
    /// Emits `out` in place of input bytes `from..to`: its first byte maps to `from` and the
    /// rest of it to `to`, so a token never starts inside a replacement.
    fn emit(&mut self, out: &str, from: usize, to: usize) {
        let (from, to) = (self.input.origin[from], self.input.origin[to]);
        for i in 0..out.len() {
            self.origin.push(if i == 0 { from } else { to });
        }
        self.text.push_str(out);
    }

    /// Copies input bytes `from..to` unchanged.
    fn copy(&mut self, from: usize, to: usize) {
        self.text.push_str(&self.input.text[from..to]);
        self.origin.extend(&self.input.origin[from..to]);
    }

    fn finish(mut self) -> Filtered {
        self.origin.push(self.input.origin[self.input.text.len()]);
        Filtered { text: self.text, origin: self.origin }
    }
}

/// A character filter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CharFilter {
    /// Removes HTML markup and decodes character references, except the tags listed.
    HtmlStrip {
        /// Tag names, in lowercase, left in the text.
        ignored_tags: BTreeSet<String>,
    },
    /// NFKC_Casefold, the default of the ICU normalizer the documentation names.
    IcuNormalize,
    /// Replaces each original string with its replacement, the longest match first.
    Mapping {
        /// `(original, replacement)` pairs; originals are not empty.
        mappings: Vec<(String, String)>,
    },
    /// Replaces the zero-width non-joiner with a space.
    Persian,
}

/// Elements whose tags break words: they become a line break instead of nothing.
const BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "blockquote",
    "body",
    "br",
    "dd",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "ul",
];

/// Elements whose content is not text.
const RAW_TAGS: &[&str] = &["script", "style"];

/// The most characters a character reference spans, `&` and `;` included.
const MAX_REFERENCE: usize = 34;

/// Named character references decoded by `htmlStrip`.
const ENTITIES: &[(&str, char)] =
    &[("amp", '&'), ("lt", '<'), ("gt", '>'), ("quot", '"'), ("apos", '\''), ("nbsp", ' ')];

impl CharFilter {
    /// Applies the filter.
    pub fn apply(&self, input: &Filtered) -> Filtered {
        match self {
            CharFilter::HtmlStrip { ignored_tags } => html_strip(input, ignored_tags),
            CharFilter::IcuNormalize => icu_normalize(input),
            CharFilter::Mapping { mappings } => mapping(input, mappings),
            CharFilter::Persian => {
                let mut out = input.builder();
                for (at, c) in input.text.char_indices() {
                    if c == '\u{200C}' {
                        out.emit(" ", at, at + c.len_utf8());
                    } else {
                        out.copy(at, at + c.len_utf8());
                    }
                }
                out.finish()
            }
        }
    }
}

/// NFKC_Casefold per run of a starter and the characters that combine with it, so offsets stay
/// at character boundaries of the original.
fn icu_normalize(input: &Filtered) -> Filtered {
    let mut out = input.builder();
    let text = &input.text;
    let mut start = 0;
    let boundaries = text.char_indices().map(|(i, _)| i).skip(1).chain([text.len()]);
    for end in boundaries {
        // A chunk does not end before a combining mark, a character NFKC_Casefold removes or one
        // that composes with the character before it.
        let next = text[end..].chars().next();
        let previous = text[..end].chars().next_back();
        let joins = |c: char| {
            super::unicode::ccc(c) != 0
                || super::unicode::casefold(c).is_some_and(|mut m| m.next().is_none())
                || previous.is_some_and(|p| super::unicode::compose(p, c).is_some())
        };
        if next.is_some_and(joins) {
            continue;
        }
        out.emit(&nfkc_casefold(&text[start..end]), start, end);
        start = end;
    }
    out.finish()
}

fn mapping(input: &Filtered, mappings: &[(String, String)]) -> Filtered {
    let mut out = input.builder();
    let text = &input.text;
    let mut at = 0;
    while at < text.len() {
        let best = mappings
            .iter()
            .filter(|(from, _)| !from.is_empty() && text[at..].starts_with(from.as_str()))
            .max_by_key(|(from, _)| from.len());
        match best {
            Some((from, to)) => {
                out.emit(to, at, at + from.len());
                at += from.len();
            }
            None => {
                let len = text[at..].chars().next().map_or(1, char::len_utf8);
                out.copy(at, at + len);
                at += len;
            }
        }
    }
    out.finish()
}

/// The lowercase name of the tag that starts at `<` and whether it closes.
fn tag_name(tag: &str) -> (String, bool) {
    let body = tag.trim_start_matches('<');
    let closing = body.starts_with('/');
    let name = body
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    (name, closing)
}

fn html_strip(input: &Filtered, ignored: &BTreeSet<String>) -> Filtered {
    let mut out = input.builder();
    let text = &input.text;
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(text.len(), |e| at + e + 3);
            out.emit("", at, end);
            at = end;
        } else if rest.starts_with('<')
            && rest[1..].starts_with(|c: char| c.is_ascii_alphabetic() || c == '/' || c == '!')
            && let Some(close) = rest.find('>')
        {
            let end = at + close + 1;
            let (name, closing) = tag_name(&rest[..=close]);
            if ignored.contains(&name) {
                out.copy(at, end);
                at = end;
                continue;
            }
            let mut end_all = end;
            if RAW_TAGS.contains(&name.as_str()) && !closing {
                let closer = format!("</{name}");
                end_all = text[end..]
                    .to_ascii_lowercase()
                    .find(&closer)
                    .and_then(|i| text[end + i..].find('>').map(|j| end + i + j + 1))
                    .unwrap_or(text.len());
            }
            // A whole `script` or `style` element leaves a line break, as measured on `refsearch`.
            let raw = RAW_TAGS.contains(&name.as_str()) && !closing;
            let block = BLOCK_TAGS.contains(&name.as_str()) || raw;
            let replacement = if block { "\n" } else { "" };
            out.emit(replacement, at, end_all);
            at = end_all;
        } else if rest.starts_with('&')
            && let Some((c, len)) = entity(rest)
        {
            out.emit(c.encode_utf8(&mut [0; 4]), at, at + len);
            at += len;
        } else {
            let len = rest.chars().next().map_or(1, char::len_utf8);
            out.copy(at, at + len);
            at += len;
        }
    }
    out.finish()
}

/// The character a reference at the start of `text` stands for and its length.
fn entity(text: &str) -> Option<(char, usize)> {
    // A reference name is short: do not scan the rest of the text for its `;`. Without one, a
    // named reference followed by no letter or digit still counts (`&amp` at the end).
    let Some(end) = text.char_indices().take(MAX_REFERENCE).find(|&(_, c)| c == ';').map(|e| e.0)
    else {
        return ENTITIES.iter().find_map(|&(name, c)| {
            let after = text[1..].strip_prefix(name)?;
            (!after.starts_with(|n: char| n.is_ascii_alphanumeric())).then_some((c, 1 + name.len()))
        });
    };
    let name = &text[1..end];
    let c = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
        char::from_u32(u32::from_str_radix(hex, 16).ok()?)?
    } else if let Some(dec) = name.strip_prefix('#') {
        char::from_u32(dec.parse().ok()?)?
    } else {
        ENTITIES.iter().find(|(n, _)| *n == name)?.1
    };
    Some((c, end + 1))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn run(filter: &CharFilter, text: &str) -> Filtered {
        filter.apply(&Filtered::new(text))
    }

    fn strip(text: &str) -> String {
        run(&CharFilter::HtmlStrip { ignored_tags: BTreeSet::new() }, text).text
    }

    #[test]
    fn html_strip_removes_markup() {
        assert_eq!(strip("<head><title>x</title></head><b>wo</b>rd"), "\n\nx\n\nword");
        assert_eq!(strip("a<!-- c -->b"), "ab");
        assert_eq!(strip("a<!-- open"), "a");
        assert_eq!(strip("a<script>var x = '<p>';</script>b"), "a\nb");
        assert_eq!(strip("a<SCRIPT>never closed"), "a\n");
        assert_eq!(strip("a<style>x</STYLE b>c"), "a\nc");
        assert_eq!(strip("&amp &ampx &lt"), "& &ampx <");
        assert_eq!(strip("1 < 2 and <3"), "1 < 2 and <3");
        assert_eq!(strip("a<b"), "a<b");
        assert_eq!(strip("a</script>b"), "ab");
        assert_eq!(
            strip("x &amp; y &lt;&#65;&#x42;&#X43;&nbsp;&bogus; &#xZZ; &"),
            "x & y <ABC &bogus; &#xZZ; &"
        );
        assert_eq!(strip("<!DOCTYPE html>t"), "t");
        let kept = CharFilter::HtmlStrip { ignored_tags: BTreeSet::from(["a".to_string()]) };
        assert_eq!(run(&kept, "<p><a href='x'>l</a></p>").text, "\n<a href='x'>l</a>\n");
    }

    #[test]
    fn offsets_point_into_the_original() {
        let f = run(&CharFilter::HtmlStrip { ignored_tags: BTreeSet::new() }, "<b>ab</b>&amp;c");
        assert_eq!(f.text, "ab&c");
        assert_eq!((f.original(0), f.original(2), f.original(3), f.original(4)), (3, 9, 14, 15));
        let chained = CharFilter::IcuNormalize.apply(&run(&CharFilter::Persian, "Ａ\u{200C}b"));
        assert_eq!(chained.text, "a b");
        assert_eq!(chained.original(1), 3);
        assert_eq!(chained.original(3), 7);
    }

    #[test]
    fn icu_normalize_folds() {
        let f = run(&CharFilter::IcuNormalize, "№ E\u{301}TÉ ﬁ");
        assert_eq!(f.text, "no été fi");
        assert_eq!(run(&CharFilter::IcuNormalize, "").text, "");
    }

    #[test]
    fn mapping_takes_the_longest_match() {
        let m = CharFilter::Mapping {
            mappings: vec![
                ("-".into(), "".into()),
                (".".into(), "".into()),
                ("(".into(), "".into()),
                (")".into(), "".into()),
                (" ".into(), "".into()),
                ("ab".into(), "X".into()),
                ("abc".into(), "Y".into()),
                ("".into(), "never".into()),
            ],
        };
        assert_eq!(run(&m, "(123) 456-7890.").text, "1234567890");
        assert_eq!(run(&m, "abcab ñ").text, "YXñ");
    }
}
