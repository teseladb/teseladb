//! TeselaDB's regular expressions (FR-015): the PCRE2 syntax that the reference server's `$regex`
//! accepts, always in UTF mode, with the options `i`, `m`, `x`, `s` and `u`, compiled to a
//! backtracking machine with a limit of steps.
//!
//! ```
//! use teseladb_mql::regex::Regex;
//!
//! let re = Regex::new(r"^(?<y>\d{4})-(\d\d)$", "").unwrap();
//! assert!(re.is_match("2026-10").unwrap());
//! assert!(Regex::new("ABC", "i").unwrap().is_match("xabcx").unwrap());
//! assert!(Regex::new("(", "").is_err());
//! ```

mod ast;
mod compile;
mod parse;
mod start;
mod unicode;
mod unicode_data;
mod vm;

use crate::error::{MqlError, Result, bad_value, codes};
use ast::{Assertion, Node};
use compile::Program;
use parse::Flags;
use start::StartInfo;
use vm::{Machine, MatchError};

/// Longest pattern the reference server accepts, in bytes.
pub const MAX_PATTERN_LEN: usize = 32 * 1024;

/// The options `$regex` and `$options` accept.
pub const OPTIONS: &str = "imxsu";

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    program: Program,
    groups: usize,
    names: Vec<(String, usize)>,
    anchored: bool,
    start: StartInfo,
    pattern: String,
    options: String,
}

/// A match: the byte range of the whole match and of each capture group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    /// Each group's byte range, the whole match first; `None` for a group that did not take part.
    pub groups: Vec<Option<(usize, usize)>>,
}

impl Regex {
    /// Compiles `pattern` with `options` (any of `imxsu`).
    pub fn new(pattern: &str, options: &str) -> Result<Regex> {
        let mut flags = Flags::default();
        for c in options.chars() {
            match c {
                'i' => flags.caseless = true,
                'm' => flags.multiline = true,
                'x' => flags.extended = true,
                's' => flags.dotall = true,
                'u' => {}
                c => {
                    return Err(MqlError::new(
                        codes::REGEX_INVALID_OPTION,
                        format!("invalid flag in regex options: {c}"),
                    ));
                }
            }
        }
        if pattern.contains('\0') {
            return Err(bad_value("Regular expression cannot contain an embedded null byte"));
        }
        if pattern.len() > MAX_PATTERN_LEN {
            return Err(invalid("regular expression is too large"));
        }
        Regex::build(pattern, options, flags)
    }

    /// Compiles `pattern` without options and without the reference server's limit on its length,
    /// for patterns written by a translation from another syntax (the Java patterns of the search
    /// analyzers, whose classes can become long lists of ranges).
    pub fn without_length_limit(pattern: &str) -> Result<Regex> {
        if pattern.contains('\0') {
            return Err(bad_value("Regular expression cannot contain an embedded null byte"));
        }
        Regex::build(pattern, "", Flags { translated: true, ..Flags::default() })
    }

    fn build(pattern: &str, options: &str, flags: Flags) -> Result<Regex> {
        let parsed = parse::parse(pattern, flags).map_err(|e| invalid(&e))?;
        let anchored = starts_anchored(&parsed.node);
        let start = StartInfo::new(&parsed.node, parsed.options.start_opt);
        let program =
            compile::compile(&parsed.node, parsed.groups, parsed.options, &parsed.reset_groups)
                .map_err(|e| invalid(&e))?;
        Ok(Regex {
            program,
            groups: parsed.groups,
            names: parsed.names,
            anchored,
            start,
            pattern: pattern.to_owned(),
            options: options.to_owned(),
        })
    }

    /// The pattern.
    pub fn pattern(&self) -> &str {
        &self.pattern
    }

    /// The options.
    pub fn options(&self) -> &str {
        &self.options
    }

    /// Number of capture groups.
    pub fn groups(&self) -> usize {
        self.groups
    }

    /// Named groups and their numbers.
    pub fn names(&self) -> &[(String, usize)] {
        &self.names
    }

    /// Whether the expression matches somewhere in `text`.
    pub fn is_match(&self, text: &str) -> Result<bool> {
        Ok(self.find_at(text, 0)?.is_some())
    }

    /// The first match at or after the byte offset `start` (a character boundary of `text`).
    pub fn find_at(&self, text: &str, start: usize) -> Result<Option<Match>> {
        if start > text.len() || !text.is_char_boundary(start) {
            return Ok(None);
        }
        if !self.start.possible(text, start) {
            return Ok(None);
        }
        let mut machine = Machine::new(&self.program, text);
        let found = machine
            .search(start, self.groups, self.anchored && start == 0, |at| {
                self.start.may_start(text, at)
            })
            .map_err(|MatchError::Limit| {
                bad_value("regular expression match exceeded its limit of steps")
            })?;
        Ok(found.map(|slots| Match { groups: slots.chunks(2).map(|p| p[0].zip(p[1])).collect() }))
    }
}

/// `Regular expression is invalid: <reason>`, the reference server's compile error.
fn invalid(reason: &str) -> MqlError {
    MqlError::new(codes::REGEX_INVALID, format!("Regular expression is invalid: {reason}"))
}

/// Whether every match must start at the start of the subject (`^` without `m`, `\A`).
fn starts_anchored(node: &Node) -> bool {
    match node {
        Node::Assert(Assertion::Start { multiline: false } | Assertion::SubjectStart) => true,
        Node::Concat(items) => items.first().is_some_and(starts_anchored),
        Node::Group(n, _) | Node::Atomic(n) => starts_anchored(n),
        Node::Alt(alts) => alts.iter().all(starts_anchored),
        _ => false,
    }
}
