//! Regular expressions in the syntax of `java.util.regex.Pattern`, the syntax of the `pattern`
//! attribute of the `regexCaptureGroup` and `regexSplit` tokenizers and of the `regex` token filter
//! (FR-007). [`parse`] translates the pattern into PCRE2, which the engine of `teseladb-mql`
//! (decision D-049) compiles; matching follows `Matcher.find`, and replacements follow
//! `Matcher.replaceAll` and `replaceFirst` with `$n`, `${name}` and `\` escapes.
//!
//! Choices the Javadoc does not settle, which T005 compares with `refsearch`: with
//! `CASE_INSENSITIVE` only characters and ranges compare without case, not `\p{…}` nor the
//! predefined classes; `\b` uses the word characters of `\w` (Java 19 and later); a back reference
//! to a group that does not exist yet never matches, and one under `CASE_INSENSITIVE` uses the
//! engine's case folding. General categories and scripts use the engine's Unicode 16.0.0 tables
//! and the other properties the Unicode 15.1.0 tables of [`crate::analysis::props`]. `\b{g}` passes
//! through as the engine's extended grapheme cluster boundary, which it reads only in patterns
//! compiled with `Regex::without_length_limit`.

pub mod class;
pub mod names;
pub mod parse;
pub mod set;

use teseladb_mql::regex::Regex;

pub use parse::{ErrorKind, SyntaxError};

/// Why a pattern does not compile or a match or a replacement fails.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexError {
    /// The Java syntax is wrong.
    Syntax(SyntaxError),
    /// The engine rejects the translated pattern (for instance a lookbehind without a longest
    /// length), with its message.
    Engine(String),
    /// A match ran out of the engine's steps.
    Limit,
    /// A replacement has a `\` or `$` at its end, a `$` without a group, or names a group the
    /// pattern does not have.
    Replacement,
}

/// A compiled Java pattern.
#[derive(Debug)]
pub struct JavaRegex {
    regex: Regex,
    groups: usize,
    names: Vec<(String, usize)>,
}

/// One match: the byte range of the whole match and of each group (`None` when the group did not
/// take part).
pub type Groups = Vec<Option<(usize, usize)>>;

enum Piece {
    Text(String),
    Group(usize),
}

impl JavaRegex {
    /// Compiles `pattern`.
    pub fn new(pattern: &str) -> Result<JavaRegex, RegexError> {
        let t = parse::translate(pattern).map_err(RegexError::Syntax)?;
        let regex =
            Regex::without_length_limit(&t.pcre).map_err(|e| RegexError::Engine(e.message))?;
        Ok(JavaRegex { regex, groups: t.groups, names: t.names })
    }

    /// Number of capturing groups.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// Every match of `text` in order, as successive `Matcher.find` calls find them: the next
    /// search starts where the previous match ended, one character later after an empty match.
    pub fn find_all(&self, text: &str) -> Result<Vec<Groups>, RegexError> {
        let mut out = Vec::new();
        let mut at = 0;
        while at <= text.len() {
            let Some(m) = self.regex.find_at(text, at).map_err(|_| RegexError::Limit)? else {
                break;
            };
            let (start, end) = m.groups.first().copied().flatten().unwrap_or((at, at));
            at = if end > start {
                end
            } else {
                end + text[end..].chars().next().map_or(1, char::len_utf8)
            };
            out.push(m.groups);
        }
        Ok(out)
    }

    fn pieces(&self, replacement: &str) -> Result<Vec<Piece>, RegexError> {
        let chars: Vec<char> = replacement.chars().collect();
        let mut out = Vec::new();
        let mut text = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '\\' => {
                    text.push(*chars.get(i + 1).ok_or(RegexError::Replacement)?);
                    i += 2;
                }
                '$' => {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                    i += 1;
                    let group = if chars.get(i) == Some(&'{') {
                        let end = chars[i..]
                            .iter()
                            .position(|&c| c == '}')
                            .ok_or(RegexError::Replacement)?
                            + i;
                        let name: String = chars[i + 1..end].iter().collect();
                        i = end + 1;
                        self.names
                            .iter()
                            .find(|(n, _)| *n == name)
                            .map(|&(_, g)| g)
                            .ok_or(RegexError::Replacement)?
                    } else {
                        let first = chars
                            .get(i)
                            .and_then(|c| c.to_digit(10))
                            .ok_or(RegexError::Replacement)?;
                        let mut group = first as usize;
                        i += 1;
                        while let Some(d) = chars.get(i).and_then(|c| c.to_digit(10)) {
                            if group * 10 + d as usize > self.groups {
                                break;
                            }
                            group = group * 10 + d as usize;
                            i += 1;
                        }
                        group
                    };
                    if group > self.groups {
                        return Err(RegexError::Replacement);
                    }
                    out.push(Piece::Group(group));
                }
                c => {
                    text.push(c);
                    i += 1;
                }
            }
        }
        out.push(Piece::Text(text));
        Ok(out)
    }

    /// Checks a replacement before any match: a `\` or `$` at its end, a `$` without a group or
    /// a group the pattern does not have.
    pub fn check_replacement(&self, replacement: &str) -> Result<(), RegexError> {
        self.pieces(replacement).map(|_| ())
    }

    /// `Matcher.replaceAll` (`all`) or `Matcher.replaceFirst`. The replacement is checked only
    /// when there is a match, as Java does.
    pub fn replace(&self, text: &str, replacement: &str, all: bool) -> Result<String, RegexError> {
        let mut matches = self.find_all(text)?;
        if matches.is_empty() {
            return Ok(text.to_string());
        }
        if !all {
            matches.truncate(1);
        }
        let pieces = self.pieces(replacement)?;
        let mut out = String::with_capacity(text.len());
        let mut copied = 0;
        for groups in &matches {
            let Some((start, end)) = groups.first().copied().flatten() else { continue };
            out.push_str(&text[copied..start]);
            for piece in &pieces {
                match piece {
                    Piece::Text(t) => out.push_str(t),
                    Piece::Group(g) => {
                        if let Some(&Some((s, e))) = groups.get(*g) {
                            out.push_str(&text[s..e]);
                        }
                    }
                }
            }
            copied = end;
        }
        out.push_str(&text[copied..]);
        Ok(out)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests;
