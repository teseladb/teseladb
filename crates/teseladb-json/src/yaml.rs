//! The YAML subset of `refnode` configuration files, read into a [`Value`]: block
//! mappings and sequences, flow sequences of scalars, plain and quoted scalars with the types of
//! the YAML 1.2 core schema, and comments. Anchors, aliases, tags, block scalars, flow mappings
//! other than `{}`, complex keys, several documents and collections inside a sequence are errors
//! with their position, as are repeated keys and tabs in the indentation.

use std::fmt;

use crate::value::{Number, Object, Value};

/// Largest text read (configuration files are small).
pub const MAX_YAML_SIZE: usize = 1 << 20;

/// Deepest nesting of collections.
pub const MAX_YAML_DEPTH: usize = 32;

/// What failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum YamlErrorKind {
    /// The text is not valid UTF-8.
    Utf8,
    /// Tab in the indentation.
    Tab,
    /// A line indented where no collection can start or continue.
    Indent,
    /// Text that is not a key, a sequence item or a scalar.
    Syntax,
    /// YAML outside the subset (anchors, tags, block scalars, flow mappings…).
    Unsupported,
    /// Quoted scalar without its closing quote.
    Quote,
    /// Invalid escape in a double-quoted scalar.
    Escape,
    /// Repeated key in a mapping.
    DuplicateKey,
    /// Nesting deeper than [`MAX_YAML_DEPTH`].
    Depth,
    /// Text larger than [`MAX_YAML_SIZE`].
    Size,
}

/// Error with its position (line and column in bytes, both from 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct YamlError {
    /// What failed.
    pub kind: YamlErrorKind,
    /// Line.
    pub line: usize,
    /// Column in bytes.
    pub column: usize,
}

impl fmt::Display for YamlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            YamlErrorKind::Utf8 => "invalid UTF-8",
            YamlErrorKind::Tab => "tab in the indentation",
            YamlErrorKind::Indent => "unexpected indentation",
            YamlErrorKind::Syntax => "syntax error",
            YamlErrorKind::Unsupported => "YAML outside the supported subset",
            YamlErrorKind::Quote => "unterminated quoted scalar",
            YamlErrorKind::Escape => "invalid escape",
            YamlErrorKind::DuplicateKey => "duplicate key",
            YamlErrorKind::Depth => "nesting too deep",
            YamlErrorKind::Size => "text too large",
        };
        write!(f, "{what} at line {}, column {}", self.line, self.column)
    }
}

impl std::error::Error for YamlError {}

fn err<T>(kind: YamlErrorKind, line: usize, column: usize) -> Result<T, YamlError> {
    Err(YamlError { kind, line, column })
}

/// A line with content: its number, indentation and text without the indentation or a comment.
struct Line<'a> {
    no: usize,
    indent: usize,
    text: &'a str,
}

/// Whether a quoted scalar may start at byte `i` of `s` (at the start or after a separator).
fn token_start(s: &[u8], i: usize) -> bool {
    i == 0 || matches!(s[i - 1], b' ' | b':' | b'[' | b',' | b'-')
}

/// `line` without a comment: `#` at the start or after a space, outside quoted scalars.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut quote = None;
    let mut at = 0;
    while at < bytes.len() {
        match (quote, bytes[at]) {
            (None, b'#') if at == 0 || bytes[at - 1] == b' ' => return &line[..at],
            (None, open @ (b'\'' | b'"')) if token_start(bytes, at) => quote = Some(open),
            (Some(b'"'), b'\\') => at += 1,
            (Some(b'\''), b'\'') if bytes.get(at + 1) == Some(&b'\'') => at += 1,
            (Some(close), byte) if byte == close => quote = None,
            _ => {}
        }
        at += 1;
    }
    line
}

/// The lines with content; `---` may open the text.
fn lines(text: &str) -> Result<Vec<Line<'_>>, YamlError> {
    let mut out = Vec::new();
    for (n, raw) in text.split('\n').enumerate() {
        let no = n + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let indent = raw.len() - raw.trim_start_matches(' ').len();
        let body = strip_comment(&raw[indent..]).trim_end();
        if body.trim_start().is_empty() {
            continue;
        }
        if body.starts_with('\t') {
            return err(YamlErrorKind::Tab, no, indent + 1);
        }
        if indent == 0 && (body == "---" || body == "...") {
            if body == "---" && out.is_empty() {
                continue;
            }
            return err(YamlErrorKind::Unsupported, no, 1);
        }
        out.push(Line { no, indent, text: body });
    }
    Ok(out)
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Reads YAML text into a [`Value`] (an empty text is an empty mapping).
///
/// # Errors
///
/// A [`YamlError`] with the line and column of the first fault: text over the size limit, invalid
/// UTF-8, a construct outside the subset, or bad syntax.
pub fn parse_yaml(input: &[u8]) -> Result<Value, YamlError> {
    if input.len() > MAX_YAML_SIZE {
        return err(YamlErrorKind::Size, 1, 1);
    }
    let text = std::str::from_utf8(input).or_else(|e| {
        let (line, column) = crate::error::line_column(&input[..e.valid_up_to()]);
        err(YamlErrorKind::Utf8, line, column)
    })?;
    let mut p = Parser { lines: lines(text)?, i: 0 };
    let Some(first) = p.lines.first() else {
        return Ok(Value::Object(Object::new()));
    };
    if p.lines.len() == 1 && first.text == "{}" {
        return Ok(Value::Object(Object::new()));
    }
    let v = p.block(first.indent, 0)?;
    match p.lines.get(p.i) {
        Some(l) => err(YamlErrorKind::Syntax, l.no, l.indent + 1),
        None => Ok(v),
    }
}

struct Parser<'a> {
    lines: Vec<Line<'a>>,
    i: usize,
}

impl Parser<'_> {
    /// The collection whose first line is the current one, at `indent`.
    fn block(&mut self, indent: usize, depth: usize) -> Result<Value, YamlError> {
        let l = &self.lines[self.i];
        if depth >= MAX_YAML_DEPTH {
            return err(YamlErrorKind::Depth, l.no, l.indent + 1);
        }
        if is_item(l.text) { self.sequence(indent) } else { self.mapping(indent, depth) }
    }

    fn mapping(&mut self, indent: usize, depth: usize) -> Result<Value, YamlError> {
        let mut obj = Object::new();
        while let Some(l) = self.lines.get(self.i) {
            let (no, at) = (l.no, l.indent);
            if at < indent {
                break;
            }
            if at > indent {
                return err(YamlErrorKind::Indent, no, at + 1);
            }
            if is_item(l.text) {
                return err(YamlErrorKind::Syntax, no, at + 1);
            }
            let (key, rest, rest_at) = split_key(l.text, no, at)?;
            if obj.get(&key).is_some() {
                return err(YamlErrorKind::DuplicateKey, no, at + 1);
            }
            self.i += 1;
            let value = if rest.is_empty() {
                match self.lines.get(self.i) {
                    Some(n) if n.indent > indent => self.block(n.indent, depth + 1)?,
                    Some(n) if n.indent == indent && is_item(n.text) => self.sequence(indent)?,
                    _ => Value::Null,
                }
            } else {
                inline(rest, no, rest_at)?
            };
            obj.insert(key, value);
        }
        Ok(Value::Object(obj))
    }

    /// A sequence of scalars at `indent`.
    fn sequence(&mut self, indent: usize) -> Result<Value, YamlError> {
        let mut items = Vec::new();
        while let Some(l) = self.lines.get(self.i) {
            if l.indent != indent || !is_item(l.text) {
                break;
            }
            let (no, text) = (l.no, l.text);
            let rest = text[1..].trim_start();
            let rest_at = indent + text.len() - rest.len();
            self.i += 1;
            let nested = self.lines.get(self.i).filter(|n| n.indent > indent);
            if rest.is_empty() {
                if let Some(n) = nested {
                    return err(YamlErrorKind::Unsupported, n.no, n.indent + 1);
                }
                items.push(Value::Null);
                continue;
            }
            // Collections inside a sequence are not in the subset.
            if is_item(rest) || split_key(rest, no, rest_at).is_ok() {
                return err(YamlErrorKind::Unsupported, no, rest_at + 1);
            }
            items.push(inline(rest, no, rest_at)?);
        }
        if let Some(l) = self.lines.get(self.i).filter(|l| l.indent > indent) {
            return err(YamlErrorKind::Indent, l.no, l.indent + 1);
        }
        Ok(Value::Array(items))
    }
}

/// Splits `key: rest`; `at` is the column (from 0) where `text` starts. Returns the key, the rest
/// and the column of the rest.
fn split_key(text: &str, no: usize, at: usize) -> Result<(String, &str, usize), YamlError> {
    if text == "?" || text.starts_with("? ") {
        return err(YamlErrorKind::Unsupported, no, at + 1);
    }
    let (key, after) = if let q @ (b'\'' | b'"') = text.as_bytes()[0] {
        let (k, end) = quoted(text, q, no, at)?;
        (k, &text[end..])
    } else {
        let colon = text
            .match_indices(':')
            .map(|(i, _)| i)
            .find(|&i| text[i + 1..].is_empty() || text[i + 1..].starts_with(' '));
        let Some(colon) = colon else {
            return err(YamlErrorKind::Syntax, no, at + 1);
        };
        let key = text[..colon].trim_end();
        if key.is_empty() {
            return err(YamlErrorKind::Syntax, no, at + 1);
        }
        (key.to_owned(), &text[colon..])
    };
    let after_trim = after.trim_start();
    let Some(rest) = after_trim.strip_prefix(':').filter(|r| r.is_empty() || r.starts_with(' '))
    else {
        return err(YamlErrorKind::Syntax, no, at + 1);
    };
    let rest = rest.trim_start();
    Ok((key, rest, at + text.len() - rest.len()))
}

/// A value on the same line as its key or item.
fn inline(text: &str, no: usize, at: usize) -> Result<Value, YamlError> {
    match text.as_bytes()[0] {
        b'&' | b'*' | b'!' | b'|' | b'>' | b'%' | b'@' | b'`' => {
            err(YamlErrorKind::Unsupported, no, at + 1)
        }
        b'{' if text == "{}" => Ok(Value::Object(Object::new())),
        b'{' => err(YamlErrorKind::Unsupported, no, at + 1),
        b'[' => flow_sequence(text, no, at),
        q @ (b'\'' | b'"') => {
            let (s, end) = quoted(text, q, no, at)?;
            match text[end..].trim_start() {
                "" => Ok(Value::String(s)),
                more => err(YamlErrorKind::Syntax, no, at + text.len() - more.len() + 1),
            }
        }
        _ if text.contains(": ") || text.ends_with(':') => err(YamlErrorKind::Syntax, no, at + 1),
        _ => Ok(plain(text)),
    }
}

/// `[a, 'b', 3]`: scalars only.
fn flow_sequence(text: &str, no: usize, at: usize) -> Result<Value, YamlError> {
    let b = text.as_bytes();
    let mut items = Vec::new();
    let mut i = 1;
    let skip = |i: &mut usize| {
        while *i < b.len() && b[*i] == b' ' {
            *i += 1;
        }
    };
    skip(&mut i);
    if b.get(i) == Some(&b']') {
        i += 1;
    } else {
        loop {
            skip(&mut i);
            match b.get(i) {
                Some(b'[' | b'{') => return err(YamlErrorKind::Unsupported, no, at + i + 1),
                None | Some(b',' | b']') => return err(YamlErrorKind::Syntax, no, at + i + 1),
                Some(&q @ (b'\'' | b'"')) => {
                    let (s, end) = quoted(&text[i..], q, no, at + i)?;
                    items.push(Value::String(s));
                    i += end;
                }
                Some(_) => {
                    let end = text[i..].find([',', ']']).map_or(b.len(), |e| i + e);
                    items.push(plain(text[i..end].trim_end()));
                    i = end;
                }
            }
            skip(&mut i);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b']') => {
                    i += 1;
                    break;
                }
                _ => return err(YamlErrorKind::Syntax, no, at + i + 1),
            }
        }
    }
    match text[i..].trim_start() {
        "" => Ok(Value::Array(items)),
        more => err(YamlErrorKind::Syntax, no, at + text.len() - more.len() + 1),
    }
}

/// The quoted scalar at the start of `text` and the byte after its closing quote.
fn quoted(text: &str, q: u8, no: usize, at: usize) -> Result<(String, usize), YamlError> {
    let mut out = String::new();
    let mut chars = text.char_indices().skip(1).peekable();
    while let Some((i, c)) = chars.next() {
        match (q, c) {
            (b'\'', '\'') if chars.peek().is_some_and(|&(_, n)| n == '\'') => {
                chars.next();
                out.push('\'');
            }
            (b'"', '\\') => {
                let bad = || err(YamlErrorKind::Escape, no, at + i + 1);
                let Some((_, e)) = chars.next() else {
                    return bad();
                };
                let hex = match e {
                    'x' => 2,
                    'u' => 4,
                    'U' => 8,
                    _ => 0,
                };
                if hex > 0 {
                    let digits: String =
                        (0..hex).filter_map(|_| chars.next().map(|(_, d)| d)).collect();
                    let code = (digits.len() == hex)
                        .then(|| u32::from_str_radix(&digits, 16).ok())
                        .flatten()
                        .and_then(char::from_u32);
                    match code {
                        Some(ch) => out.push(ch),
                        None => return bad(),
                    }
                    continue;
                }
                out.push(match e {
                    '\\' => '\\',
                    '"' => '"',
                    '/' => '/',
                    ' ' => ' ',
                    '0' => '\0',
                    'a' => '\u{7}',
                    'b' => '\u{8}',
                    't' | '\t' => '\t',
                    'n' => '\n',
                    'v' => '\u{b}',
                    'f' => '\u{c}',
                    'r' => '\r',
                    'e' => '\u{1b}',
                    _ => return bad(),
                });
            }
            (_, c) if c == char::from(q) => return Ok((out, i + 1)),
            (_, c) => out.push(c),
        }
    }
    err(YamlErrorKind::Quote, no, at + 1)
}

/// A plain scalar with the types of the YAML 1.2 core schema; floats only when finite, since a
/// [`Value`] has no infinities.
fn plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "true" | "True" | "TRUE" => return Value::Bool(true),
        "false" | "False" | "FALSE" => return Value::Bool(false),
        _ => {}
    }
    if let Some(n) = integer(s) {
        return Value::Number(n);
    }
    // A text of the float grammar always parses; it may overflow to infinity, which stays text.
    if is_float(s)
        && let Some(f) = s.parse::<f64>().ok().filter(|f| f.is_finite())
    {
        return Value::Number(Number::Float(f));
    }
    Value::String(s.to_owned())
}

fn integer(s: &str) -> Option<Number> {
    let radix = |digits: &str, r: u32| {
        (!digits.is_empty()).then(|| i64::from_str_radix(digits, r).ok().map(Number::Int)).flatten()
    };
    if let Some(h) = s.strip_prefix("0x") {
        return radix(h, 16);
    }
    if let Some(o) = s.strip_prefix("0o") {
        return radix(o, 8);
    }
    let digits = s.strip_prefix(['-', '+']).unwrap_or(s);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    Some(match (s.parse::<i64>(), s.parse::<u64>()) {
        (Ok(i), _) => Number::Int(i),
        (_, Ok(u)) => Number::UInt(u),
        _ => Number::Big(s.to_owned()),
    })
}

/// `[-+]?(\.[0-9]+|[0-9]+(\.[0-9]*)?)([eE][-+]?[0-9]+)?`.
fn is_float(s: &str) -> bool {
    let s = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (mantissa, exp) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let digits = |t: &str| t.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((a, b)) => digits(a) && digits(b) && !(a.is_empty() && b.is_empty()),
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let exp_ok = exp.is_none_or(|e| {
        let e = e.strip_prefix(['-', '+']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exp_ok
}
