//! Regular expressions in the syntax of `java.util.regex.Pattern`, the syntax of the `pattern`
//! attribute of the `regexCaptureGroup` and `regexSplit` tokenizers and of the `regex` token filter
//! (FR-007). [`parse`] translates the pattern into PCRE2, which the engine of `teseladb-mql`
//! (decision D-049) compiles; matching follows `Matcher.find`, and replacements follow
//! `Matcher.replaceAll` and `replaceFirst` with `$n`, `${name}` and `\` escapes.
//!
//! Choices the Javadoc does not settle, which T005 compares with the reference search server: with
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
    /// pattern does not have, with the message of `Matcher.appendReplacement`.
    Replacement(String),
}

/// A replacement error with Java's message.
fn bad_replacement(message: &str) -> RegexError {
    RegexError::Replacement(message.to_owned())
}

/// The message of the `PatternSyntaxException` Java throws for `error` in `pattern`: the
/// description, ` near index n` when there is an index, the pattern on its own line and a caret
/// under the index when it falls inside the pattern.
pub fn syntax_message(pattern: &str, error: &SyntaxError) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let (description, index) = describe(&chars, error);
    let mut out = description;
    if let Some(i) = index {
        out.push_str(&format!(" near index {i}"));
    }
    out.push('\n');
    out.push_str(pattern);
    if let Some(i) = index.filter(|&i| i < chars.len()) {
        out.push('\n');
        out.push_str(&" ".repeat(i));
        out.push('^');
    }
    out
}

/// The text between `open` and `close` that ends before `at`, searched backwards.
fn enclosed(chars: &[char], at: usize, open: char, close: char) -> String {
    let end = chars[..at.min(chars.len())].iter().rposition(|&c| c == close).unwrap_or(at);
    let start = chars[..end.min(chars.len())].iter().rposition(|&c| c == open).map_or(0, |s| s + 1);
    chars[start..end.max(start)].iter().collect()
}

/// Java's description of `error` and the index it reports.
fn describe(chars: &[char], error: &SyntaxError) -> (String, Option<usize>) {
    let at = error.index;
    let char_at = |i: usize| chars.get(i).copied();
    let text = match error.kind {
        ErrorKind::DanglingMeta => {
            format!("Dangling meta character '{}'", char_at(at).unwrap_or(' '))
        }
        ErrorKind::IllegalRepetition if char_at(at.wrapping_sub(1)) == Some('}') => {
            return ("Illegal repetition range".into(), Some(at - 1));
        }
        ErrorKind::IllegalRepetition => "Illegal repetition".into(),
        ErrorKind::RepetitionTooLarge => {
            return ("Illegal repetition range".into(), Some(overflow_digit(chars, at)));
        }
        ErrorKind::UnclosedGroup => "Unclosed group".into(),
        ErrorKind::UnmatchedClose => {
            return ("Unmatched closing ')'".into(), at.checked_sub(1));
        }
        ErrorKind::UnclosedClass => return ("Unclosed character class".into(), Some(at - 1)),
        ErrorKind::IllegalRange => return ("Illegal character range".into(), Some(at - 1)),
        ErrorKind::IllegalEscape if at >= chars.len() => "Unescaped trailing backslash".into(),
        ErrorKind::IllegalEscape => "Illegal/unsupported escape sequence".into(),
        ErrorKind::IllegalCharacter => {
            let slash = chars[..at.min(chars.len())].iter().rposition(|&c| c == '\\');
            let slash = slash.unwrap_or(0);
            match char_at(slash + 1) {
                Some('x') => {
                    return ("Illegal hexadecimal escape sequence".into(), Some(slash + 2));
                }
                Some('u') => "Illegal Unicode escape sequence".into(),
                Some('c') => return ("Illegal control escape sequence".into(), Some(slash + 1)),
                Some('N') => {
                    let name = enclosed(chars, at + 1, '{', '}');
                    format!("Unknown character name [{name}]")
                }
                _ => "Illegal octal escape sequence".into(),
            }
        }
        ErrorKind::UnknownProperty => {
            let name = enclosed(chars, at, '{', '}');
            return (format!("Unknown character property name {{{name}}}"), Some(at - 1));
        }
        // `(?$` and `(?@` (measured in `m82.js`); any other unknown flag is a modifier.
        ErrorKind::UnknownGroup
            if matches!(char_at(at), Some('$' | '@'))
                && char_at(at.wrapping_sub(1)) == Some('?') =>
        {
            "Unknown group type".into()
        }
        ErrorKind::UnknownGroup => "Unknown inline modifier".into(),
        ErrorKind::BadGroupName => return bad_group_name(chars, at),
        ErrorKind::UnclosedCounted => "Unclosed counted closure".into(),
        ErrorKind::UnclosedGroupName => "named capturing group is missing trailing '>'".into(),
        ErrorKind::BareNamedReference => {
            "\\k is not followed by '<' for named capturing group".into()
        }
        ErrorKind::BadBoundary => "Illegal/unsupported escape sequence".into(),
    };
    (text, Some(at))
}

/// The index of the digit at which the count of the repetition that ends before `at` passes
/// `i32::MAX`, where Java stops.
fn overflow_digit(chars: &[char], at: usize) -> usize {
    let end = at.min(chars.len());
    let open = chars[..end].iter().rposition(|&c| c == '{').unwrap_or(0);
    let mut value: i64 = 0;
    for (i, c) in chars.iter().enumerate().take(end).skip(open + 1) {
        let Some(d) = c.to_digit(10) else {
            value = 0;
            continue;
        };
        value = value * 10 + i64::from(d);
        if value > i64::from(i32::MAX) {
            return i;
        }
    }
    at.saturating_sub(1)
}

/// The three group name errors: a name that does not start with a letter, a name used twice and
/// a back reference to a name that does not exist.
fn bad_group_name(chars: &[char], at: usize) -> (String, Option<usize>) {
    let before: String = chars[..at.min(chars.len())].iter().collect();
    let open = before.rfind('<').map_or(0, |b| before[..b].chars().count());
    let name: String = chars.iter().skip(open + 1).take_while(|&&c| c != '>').collect();
    if !name.starts_with(|c: char| c.is_ascii_alphabetic()) {
        let text = "capturing group name does not start with a Latin letter".into();
        return (text, Some(open + 1));
    }
    let text = if open >= 2 && chars[open - 2] == '\\' && chars[open - 1] == 'k' {
        format!("named capturing group <{name}> does not exist")
    } else {
        format!("Named capturing group <{name}> is already defined")
    };
    (text, Some(at - 1))
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
                    text.push(
                        *chars
                            .get(i + 1)
                            .ok_or_else(|| bad_replacement("character to be escaped is missing"))?,
                    );
                    i += 2;
                }
                '$' => {
                    out.push(Piece::Text(std::mem::take(&mut text)));
                    i += 1;
                    let group = if chars.get(i) == Some(&'{') {
                        let end = chars[i..].iter().position(|&c| c == '}').ok_or_else(|| {
                            bad_replacement("named capturing group is missing trailing '}'")
                        })? + i;
                        let name: String = chars[i + 1..end].iter().collect();
                        if name.is_empty() {
                            return Err(bad_replacement("named capturing group has 0 length name"));
                        }
                        i = end + 1;
                        self.names.iter().find(|(n, _)| *n == name).map(|&(_, g)| g).ok_or_else(
                            || bad_replacement(&format!("No group with name {{{name}}}")),
                        )?
                    } else {
                        let first = match chars.get(i) {
                            None => {
                                return Err(bad_replacement(
                                    "Illegal group reference: group index is missing",
                                ));
                            }
                            Some(c) => c
                                .to_digit(10)
                                .ok_or_else(|| bad_replacement("Illegal group reference"))?,
                        };
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
                        return Err(bad_replacement(&format!("No group {group}")));
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
