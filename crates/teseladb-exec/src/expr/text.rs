//! String expressions with named arguments (measured on a `refrouter` 9.0.4): `$trim`, `$ltrim`,
//! `$rtrim`, `$regexMatch`, `$regexFind`, `$regexFindAll`, `$replaceOne` and `$replaceAll`.
//!
//! The trims remove, by default, ASCII whitespace, the null character and the Unicode spaces;
//! `chars` names other characters. The regular expressions use TeselaDB's PCRE engine; positions
//! (`idx`) count code points, and after an empty match `$regexFindAll` moves one character on. A
//! `null` or missing input gives `null` (`false` for `$regexMatch`, `[]` for `$regexFindAll`).
//! `$replaceAll` with a regular expression puts the replacement in as it is (no `$1`).

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::regex::Regex;
use teseladb_mql::{ErrorCode, MqlError};

use super::arith::type_name_of;
use super::values::display;
use super::{Ctx, Eval, Expr, Scope, error};

macro_rules! code {
    ($n:literal) => {
        ErrorCode { code: $n, name: concat!("Location", $n) }
    };
}

const TRIM_UNKNOWN: ErrorCode = code!(50694);
const TRIM_INPUT: ErrorCode = code!(50695);
const TRIM_OBJECT: ErrorCode = code!(50696);
const TRIM_INPUT_STRING: ErrorCode = code!(50699);
const TRIM_CHARS_STRING: ErrorCode = code!(50700);
const REGEX_INPUT_PARAM: ErrorCode = code!(31022);
const REGEX_REGEX_PARAM: ErrorCode = code!(31023);
const REGEX_UNKNOWN: ErrorCode = code!(31024);
const REGEX_OBJECT: ErrorCode = code!(51103);
const REGEX_INPUT_STRING: ErrorCode = code!(51104);
const REGEX_REGEX_TYPE: ErrorCode = code!(51105);
const REGEX_OPTIONS_STRING: ErrorCode = code!(51106);
const REGEX_OPTIONS_TWICE: ErrorCode = code!(51107);
const REGEX_FLAG: ErrorCode = code!(51108);
const REGEX_NULL_BYTE: ErrorCode = code!(51109);
const REGEX_INVALID: ErrorCode = code!(51111);
const REPLACE_REPLACEMENT: ErrorCode = code!(51747);
const REPLACE_FIND: ErrorCode = code!(51748);
const REPLACE_INPUT: ErrorCode = code!(51749);
const REPLACE_UNKNOWN: ErrorCode = code!(51750);
const REPLACE_OBJECT: ErrorCode = code!(51751);
const REPLACE_REPLACEMENT_STRING: ErrorCode = code!(10503902);
const REPLACE_FIND_TYPE: ErrorCode = code!(10503903);
const REPLACE_INPUT_STRING: ErrorCode = code!(10503904);

/// The characters the trims remove by default.
const WHITESPACE: [char; 25] = [
    '\0', ' ', '\t', '\n', '\u{b}', '\u{c}', '\r', '\u{a0}', '\u{1680}', '\u{2000}', '\u{2001}',
    '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}',
    '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}', '\u{3000}',
];

/// Which end a trim works on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ends {
    Both,
    Start,
    End,
}

/// Which regular expression operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegexOp {
    Match,
    Find,
    FindAll,
}

/// A string expression with named arguments.
#[derive(Clone, Debug, PartialEq)]
pub enum Text {
    Trim { ends: Ends, input: Expr, chars: Option<Expr> },
    Regex { op: RegexOp, input: Expr, regex: Expr, options: Option<Expr> },
    Replace { all: bool, input: Expr, find: Expr, replacement: Expr },
}

impl Ends {
    fn name(self) -> &'static str {
        match self {
            Ends::Both => "$trim",
            Ends::Start => "$ltrim",
            Ends::End => "$rtrim",
        }
    }
}

impl RegexOp {
    fn name(self) -> &'static str {
        match self {
            RegexOp::Match => "$regexMatch",
            RegexOp::Find => "$regexFind",
            RegexOp::FindAll => "$regexFindAll",
        }
    }
}

/// The named arguments of `arg`, or `bad` when it is not a document.
fn named<'a>(
    arg: RawValue<'a>,
    bad: impl FnOnce(&str) -> MqlError,
) -> Result<teseladb_bson::RawDoc<'a>, MqlError> {
    match arg {
        RawValue::Document(d) => Ok(d),
        other => Err(bad(super::arith::raw_type_name(other))),
    }
}

/// Parses one of these operators; `None` for any other.
pub(super) fn parse(name: &str, arg: RawValue<'_>, ctx: &Ctx) -> Result<Option<Text>, MqlError> {
    let ends = match name {
        "$trim" => Some(Ends::Both),
        "$ltrim" => Some(Ends::Start),
        "$rtrim" => Some(Ends::End),
        _ => None,
    };
    if let Some(ends) = ends {
        return parse_trim(ends, arg, ctx).map(Some);
    }
    let op = match name {
        "$regexMatch" => Some(RegexOp::Match),
        "$regexFind" => Some(RegexOp::Find),
        "$regexFindAll" => Some(RegexOp::FindAll),
        _ => None,
    };
    if let Some(op) = op {
        return parse_regex(op, arg, ctx).map(Some);
    }
    match name {
        "$replaceOne" => parse_replace(false, arg, ctx).map(Some),
        "$replaceAll" => parse_replace(true, arg, ctx).map(Some),
        _ => Ok(None),
    }
}

fn parse_trim(ends: Ends, arg: RawValue<'_>, ctx: &Ctx) -> Result<Text, MqlError> {
    let op = ends.name();
    let d = named(arg, |t| {
        error(TRIM_OBJECT, format!("{op} only supports an object as an argument, found {t}"))
    })?;
    let (mut input, mut chars) = (None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "chars" => chars = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    TRIM_UNKNOWN,
                    format!("{op} found an unknown argument: {other}"),
                ));
            }
        }
    }
    let input =
        input.ok_or_else(|| error(TRIM_INPUT, format!("{op} requires an 'input' field")))?;
    Ok(Text::Trim { ends, input, chars })
}

fn parse_regex(op: RegexOp, arg: RawValue<'_>, ctx: &Ctx) -> Result<Text, MqlError> {
    let name = op.name();
    let d = named(arg, |t| {
        error(REGEX_OBJECT, format!("{name} expects an object of named arguments but found: {t}"))
    })?;
    let (mut input, mut regex, mut options) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "regex" => regex = Some(Expr::parse_in(v, ctx)?),
            "options" => options = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    REGEX_UNKNOWN,
                    format!("{name} found an unknown argument: {other}"),
                ));
            }
        }
    }
    let input = input
        .ok_or_else(|| error(REGEX_INPUT_PARAM, format!("{name} requires 'input' parameter")))?;
    let regex = regex
        .ok_or_else(|| error(REGEX_REGEX_PARAM, format!("{name} requires 'regex' parameter")))?;
    Ok(Text::Regex { op, input, regex, options })
}

fn parse_replace(all: bool, arg: RawValue<'_>, ctx: &Ctx) -> Result<Text, MqlError> {
    let name = if all { "$replaceAll" } else { "$replaceOne" };
    let d = named(arg, |t| {
        error(REPLACE_OBJECT, format!("{name} requires an object as an argument, found: {t}"))
    })?;
    let (mut input, mut find, mut replacement) = (None, None, None);
    for (k, v) in d.iter() {
        match k {
            "input" => input = Some(Expr::parse_in(v, ctx)?),
            "find" => find = Some(Expr::parse_in(v, ctx)?),
            "replacement" => replacement = Some(Expr::parse_in(v, ctx)?),
            other => {
                return Err(error(
                    REPLACE_UNKNOWN,
                    format!("{name} found an unknown argument: {other}"),
                ));
            }
        }
    }
    let input = input
        .ok_or_else(|| error(REPLACE_INPUT, format!("{name} requires 'input' to be specified")))?;
    let find =
        find.ok_or_else(|| error(REPLACE_FIND, format!("{name} requires 'find' to be specified")))?;
    let replacement = replacement.ok_or_else(|| {
        error(REPLACE_REPLACEMENT, format!("{name} requires 'replacement' to be specified"))
    })?;
    Ok(Text::Replace { all, input, find, replacement })
}

fn nullish(v: &Eval) -> bool {
    matches!(v, None | Some(Value::Null | Value::Undefined))
}

/// `v` and its type as a message names them: `1 (of type int)`.
fn shown(v: &Value) -> String {
    format!("{} (of type {})", plain(v), type_name_of(v))
}

/// A value as these messages write it, strings without quotes.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => display(other),
    }
}

impl Text {
    /// Evaluates the expression.
    pub fn eval(&self, doc: &Document, scope: &Scope) -> Result<Eval, MqlError> {
        match self {
            Text::Trim { ends, input, chars } => {
                eval_trim(*ends, input, chars.as_ref(), doc, scope)
            }
            Text::Regex { op, input, regex, options } => {
                eval_regex(*op, input, regex, options.as_ref(), doc, scope)
            }
            Text::Replace { all, input, find, replacement } => {
                eval_replace(*all, input, find, replacement, doc, scope)
            }
        }
    }
}

fn eval_trim(
    ends: Ends,
    input: &Expr,
    chars: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let op = ends.name();
    let input = input.eval(doc, scope)?;
    let set: Option<Vec<char>> = match chars {
        None => None,
        Some(e) => match e.eval(doc, scope)? {
            None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
            Some(Value::String(s)) => Some(s.chars().collect()),
            Some(other) => {
                return Err(error(
                    TRIM_CHARS_STRING,
                    format!("{op} requires 'chars' to be a string, got {} instead.", shown(&other)),
                ));
            }
        },
    };
    let s = match input {
        None | Some(Value::Null | Value::Undefined) => return Ok(Some(Value::Null)),
        Some(Value::String(s)) => s,
        Some(other) => {
            return Err(error(
                TRIM_INPUT_STRING,
                format!("{op} requires its input to be a string, got {} instead.", shown(&other)),
            ));
        }
    };
    let strip = |c: char| match &set {
        Some(chars) => chars.contains(&c),
        None => WHITESPACE.contains(&c),
    };
    let out = match ends {
        Ends::Both => s.trim_matches(strip),
        Ends::Start => s.trim_start_matches(strip),
        Ends::End => s.trim_end_matches(strip),
    };
    Ok(Some(Value::String(out.to_owned())))
}

/// The compiled expression of `$regex*`, `None` when the regex is `null` or missing.
fn compile(op: RegexOp, regex: &Eval, options: &Eval) -> Result<Option<Regex>, MqlError> {
    let name = op.name();
    let (pattern, mut flags) = match regex {
        None | Some(Value::Null | Value::Undefined) => return Ok(None),
        Some(Value::String(p)) => (p.clone(), String::new()),
        Some(Value::Regex { pattern, options }) => (pattern.clone(), options.clone()),
        Some(_) => {
            return Err(error(
                REGEX_REGEX_TYPE,
                format!("{name} needs 'regex' to be of type string or regex"),
            ));
        }
    };
    match options {
        None | Some(Value::Null | Value::Undefined) => {}
        Some(Value::String(o)) => {
            if !flags.is_empty() {
                return Err(error(
                    REGEX_OPTIONS_TWICE,
                    format!(
                        "{name}: found regex option(s) specified in both 'regex' and 'option' fields"
                    ),
                ));
            }
            flags = o.clone();
        }
        Some(_) => {
            return Err(error(
                REGEX_OPTIONS_STRING,
                format!("{name} needs 'options' to be of type string"),
            ));
        }
    }
    if let Some(c) = flags.chars().find(|c| !teseladb_mql::regex::OPTIONS.contains(*c)) {
        return Err(error(REGEX_FLAG, format!("{name} invalid flag in regex options: {c}")));
    }
    if pattern.contains('\0') {
        return Err(error(
            REGEX_NULL_BYTE,
            format!("{name}: regular expression cannot contain an embedded null byte"),
        ));
    }
    let re = Regex::new(&pattern, &flags).map_err(|e| {
        let reason =
            e.message.strip_prefix("Regular expression is invalid: ").unwrap_or(&e.message);
        error(REGEX_INVALID, format!("Invalid Regex in {name}: {reason}"))
    })?;
    Ok(Some(re))
}

/// One match as `$regexFind` gives it.
fn match_doc(text: &str, m: &teseladb_mql::regex::Match) -> Value {
    let (start, end) = m.groups[0].unwrap_or((0, 0));
    let captures = m.groups[1..]
        .iter()
        .map(|g| g.map_or(Value::Null, |(s, e)| Value::String(text[s..e].to_owned())))
        .collect();
    let idx = text[..start].chars().count();
    Value::Document(
        Document::new()
            .with("match", text[start..end].to_owned())
            .with("idx", Value::Int32(i32::try_from(idx).unwrap_or(i32::MAX)))
            .with("captures", Value::Array(captures)),
    )
}

fn eval_regex(
    op: RegexOp,
    input: &Expr,
    regex: &Expr,
    options: Option<&Expr>,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let input = input.eval(doc, scope)?;
    let regex = regex.eval(doc, scope)?;
    let options = match options {
        Some(e) => e.eval(doc, scope)?,
        None => None,
    };
    let re = compile(op, &regex, &options)?;
    let empty = || match op {
        RegexOp::Match => Value::Boolean(false),
        RegexOp::Find => Value::Null,
        RegexOp::FindAll => Value::Array(Vec::new()),
    };
    let text = match input {
        None | Some(Value::Null | Value::Undefined) => return Ok(Some(empty())),
        Some(Value::String(s)) => s,
        Some(_) => {
            return Err(error(
                REGEX_INPUT_STRING,
                format!("{} needs 'input' to be of type string", op.name()),
            ));
        }
    };
    let Some(re) = re else { return Ok(Some(empty())) };
    match op {
        RegexOp::Match => Ok(Some(Value::Boolean(re.is_match(&text)?))),
        RegexOp::Find => {
            Ok(Some(re.find_at(&text, 0)?.map_or(Value::Null, |m| match_doc(&text, &m))))
        }
        RegexOp::FindAll => {
            // A match that would start at the end of a non-empty text is not looked for.
            let mut out = Vec::new();
            let mut at = 0;
            while at < text.len() || at == 0 {
                let Some(m) = re.find_at(&text, at)? else { break };
                let (start, end) = m.groups[0].unwrap_or((at, at));
                out.push(match_doc(&text, &m));
                at = if end > start {
                    end
                } else {
                    end + text[end..].chars().next().map_or(1, char::len_utf8)
                };
            }
            Ok(Some(Value::Array(out)))
        }
    }
}

fn eval_replace(
    all: bool,
    input: &Expr,
    find: &Expr,
    replacement: &Expr,
    doc: &Document,
    scope: &Scope,
) -> Result<Eval, MqlError> {
    let name = if all { "$replaceAll" } else { "$replaceOne" };
    let (input, find, replacement) =
        (input.eval(doc, scope)?, find.eval(doc, scope)?, replacement.eval(doc, scope)?);
    let wrong = |code, what: &str, v: &Value| {
        error(
            code,
            format!(
                "{name} requires that '{what}' be {}, found: {}",
                if what == "find" { "a string or regular expression" } else { "a string" },
                display(v)
            ),
        )
    };
    if let Some(v) =
        input.as_ref().filter(|v| !nullish(&Some((*v).clone())) && !matches!(v, Value::String(_)))
    {
        return Err(wrong(REPLACE_INPUT_STRING, "input", v));
    }
    if let Some(v) = find.as_ref().filter(|v| {
        !nullish(&Some((*v).clone())) && !matches!(v, Value::String(_) | Value::Regex { .. })
    }) {
        return Err(wrong(REPLACE_FIND_TYPE, "find", v));
    }
    if let Some(v) = replacement
        .as_ref()
        .filter(|v| !nullish(&Some((*v).clone())) && !matches!(v, Value::String(_)))
    {
        return Err(wrong(REPLACE_REPLACEMENT_STRING, "replacement", v));
    }
    let (Some(Value::String(text)), Some(find), Some(Value::String(with))) =
        (input, find, replacement)
    else {
        return Ok(Some(Value::Null));
    };
    let spans: Vec<(usize, usize)> = match find {
        Value::String(f) if f.is_empty() => {
            let mut at: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
            at.push(text.len());
            at.into_iter().map(|i| (i, i)).collect()
        }
        Value::String(f) => text.match_indices(f.as_str()).map(|(i, m)| (i, i + m.len())).collect(),
        Value::Regex { pattern, options } => {
            let re = Regex::new(&pattern, &options)?;
            let mut spans = Vec::new();
            let mut at = 0;
            while at <= text.len() {
                let Some(m) = re.find_at(&text, at)? else { break };
                let (s, e) = m.groups[0].unwrap_or((at, at));
                spans.push((s, e));
                at = if e > s { e } else { e + text[e..].chars().next().map_or(1, char::len_utf8) };
            }
            spans
        }
        _ => return Ok(Some(Value::Null)),
    };
    let spans = if all { spans } else { spans.into_iter().take(1).collect() };
    let mut out = String::new();
    let mut last = 0;
    for (s, e) in spans {
        out.push_str(&text[last..s]);
        out.push_str(&with);
        last = e;
    }
    out.push_str(&text[last..]);
    Ok(Some(Value::String(out)))
}
