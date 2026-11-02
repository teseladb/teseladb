//! The `queryString` operator (011 FR-002): RefLib's classic query syntax, as `refsearch` 1.70.6
//! parses it (measured 02-11-2026, `scratchpad/lane3j/m3.js`, `m4.js`). Clauses join with `OR`
//! unless `AND`, `+`, `-`, `!` or `NOT` say otherwise, as RefLib's `QueryParser` joins them (an
//! `AND` makes the clause before it required too); a term is analyzed with its field's query
//! analyzer and runs as `text`, `"…"~n` as `phrase` with slop `n`, `t~n` as `text` with `n`
//! edits, `*`, `?` and `/…/` as patterns and `[a TO b]` as a range of terms, those three with
//! a constant score; `^n` boosts a clause or a group. A query of only excluded clauses finds
//! nothing. Syntax errors carry the parser's message.

use teseladb_bson::{Document, Value};

use super::{Operator, Score, SearchError, parse_score, pattern::Pattern, shape};
use crate::index::{Fuzzy, MemIndex};

/// The expansions of a fuzzy term, RefLib's default.
const FUZZY_EXPANSIONS: usize = 50;
/// The edits of `t~` without a number.
const FUZZY_EDITS: u8 = 2;

/// What may start a clause.
const START: &[&str] = &[
    "<NOT>",
    "\"+\"",
    "\"-\"",
    "<BAREOPER>",
    "\"(\"",
    "\"*\"",
    "<QUOTED>",
    "<TERM>",
    "<PREFIXTERM>",
    "<WILDTERM>",
    "<REGEXPTERM>",
    "\"[\"",
    "\"{\"",
    "<NUMBER>",
    "<TERM>",
];
/// What may follow a field name and its colon.
const AFTER_FIELD: &[&str] = &[
    "<BAREOPER>",
    "\"(\"",
    "\"*\"",
    "<QUOTED>",
    "<TERM>",
    "<PREFIXTERM>",
    "<WILDTERM>",
    "<REGEXPTERM>",
    "\"[\"",
    "\"{\"",
    "<NUMBER>",
];
/// What may follow a clause, before the closing parenthesis or the end.
const AFTER_CLAUSE: &[&str] = &[
    "<AND>",
    "<OR>",
    "<NOT>",
    "\"+\"",
    "\"-\"",
    "<BAREOPER>",
    "\"(\"",
    "\")\"",
    "\"*\"",
    "\"^\"",
    "<QUOTED>",
    "<TERM>",
    "<FUZZY_SLOP>",
    "<PREFIXTERM>",
    "<WILDTERM>",
    "<REGEXPTERM>",
    "\"[\"",
    "\"{\"",
    "<NUMBER>",
];
/// What may follow the first bound of a range.
const IN_RANGE: &[&str] = &["\"TO\"", "<RANGE_QUOTED>", "<RANGE_GOOP>"];

/// A token, with the character where it starts.
#[derive(Clone, Debug, PartialEq)]
enum Tok {
    And(String),
    Or(String),
    Not(String),
    Plus,
    Minus,
    Open,
    Close,
    Colon,
    Caret,
    Star,
    Term(String),
    Quoted(String),
    Regex(String),
    /// `~` and its number, if any.
    Fuzzy(Option<f64>),
    Number(f64),
    /// A range: its bounds (`None` for `*`) and whether each is inclusive.
    Range(Option<String>, Option<String>, bool, bool),
}

impl Tok {
    /// How the parser's errors name the token and its text.
    fn named(&self) -> (String, String) {
        let lit = |s: &str| (format!("\"{s}\""), s.to_owned());
        match self {
            Tok::And(s) => ("<AND>".into(), s.clone()),
            Tok::Or(s) => ("<OR>".into(), s.clone()),
            Tok::Not(s) => ("<NOT>".into(), s.clone()),
            Tok::Plus => lit("+"),
            Tok::Minus => lit("-"),
            Tok::Open => lit("("),
            Tok::Close => lit(")"),
            Tok::Colon => lit(":"),
            Tok::Caret => lit("^"),
            Tok::Star => lit("*"),
            Tok::Term(s) => ("<TERM>".into(), s.clone()),
            Tok::Quoted(s) => ("<QUOTED>".into(), format!("\"{s}\"")),
            Tok::Regex(s) => ("<REGEXPTERM>".into(), format!("/{s}/")),
            Tok::Fuzzy(_) => ("<FUZZY_SLOP>".into(), "~".into()),
            Tok::Number(n) => ("<NUMBER>".into(), n.to_string()),
            Tok::Range(..) => lit("["),
        }
    }
}

/// The parser's error for `query`.
struct Fail(String);

fn expected(list: &[&str]) -> String {
    match list {
        [one] => format!("Was expecting:\n    {one} ...\n    "),
        _ => {
            let items: Vec<String> =
                list.iter()
                    .map(|i| {
                        if *i == "<EOF>" { "    <EOF> ".to_owned() } else { format!("    {i} ...") }
                    })
                    .collect();
            format!("Was expecting one of:\n{}\n    ", items.join("\n"))
        }
    }
}

fn at_end(query: &str, list: &[&str]) -> Fail {
    let col = query.chars().count();
    Fail(format!("Encountered \"<EOF>\" at line 1, column {col}.\n{}", expected(list)))
}

fn at_token(tok: &Tok, col: usize, list: &[&str]) -> Fail {
    let (kind, image) = tok.named();
    Fail(format!(
        "Encountered \" {kind} \"{image} \"\" at line 1, column {col}.\n{}",
        expected(list)
    ))
}

fn lexical(col: usize, rest: &str) -> Fail {
    Fail(format!("Lexical error at line 1, column {col}.  Encountered: <EOF>{rest}"))
}

/// Whether `c` ends a term.
fn special(c: char) -> bool {
    c.is_whitespace() || "():^[]{}\"~".contains(c)
}

fn lex(query: &str) -> Result<Vec<(Tok, usize)>, Fail> {
    let chars: Vec<char> = query.chars().collect();
    let len = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < len {
        let c = chars[i];
        let start = i;
        i += 1;
        let tok = match c {
            c if c.is_whitespace() => continue,
            '(' => Tok::Open,
            ')' => Tok::Close,
            ':' => Tok::Colon,
            '+' => Tok::Plus,
            '-' => Tok::Minus,
            '!' => Tok::Not("!".into()),
            '&' if chars.get(i) == Some(&'&') => {
                i += 1;
                Tok::And("&&".into())
            }
            '|' if chars.get(i) == Some(&'|') => {
                i += 1;
                Tok::Or("||".into())
            }
            '^' => {
                out.push((Tok::Caret, start));
                let digits: String =
                    chars[i..].iter().take_while(|c| c.is_ascii_digit() || **c == '.').collect();
                if i == len {
                    continue;
                }
                let Ok(n) = digits.parse::<f64>() else { return Err(lexical(len + 1, "")) };
                i += digits.chars().count();
                Tok::Number(n)
            }
            '~' => {
                let digits: String =
                    chars[i..].iter().take_while(|c| c.is_ascii_digit() || **c == '.').collect();
                i += digits.chars().count();
                Tok::Fuzzy(digits.parse().ok())
            }
            '"' => {
                let mut text = String::new();
                loop {
                    match chars.get(i) {
                        None => {
                            let prefix: String = chars[start..].iter().collect();
                            let prefix = prefix.replace('"', "\\\"");
                            return Err(lexical(
                                len + 1,
                                &format!(" after prefix \"{prefix}\" (in lexical state 2)"),
                            ));
                        }
                        Some('"') => break,
                        Some('\\') if i + 1 < len => {
                            text.push(chars[i + 1]);
                            i += 1;
                        }
                        Some(c) => text.push(*c),
                    }
                    i += 1;
                }
                i += 1;
                Tok::Quoted(text)
            }
            '/' => {
                let end = chars[i..].iter().position(|c| *c == '/').map(|p| p + i);
                let Some(end) = end else { return Err(lexical(len + 1, "")) };
                let re: String = chars[i..end].iter().collect();
                i = end + 1;
                Tok::Regex(re)
            }
            '[' | '{' => {
                let words: Vec<(String, usize)> = {
                    let mut ws = Vec::new();
                    let mut w = String::new();
                    let mut closed = None;
                    while i < len {
                        let ch = chars[i];
                        i += 1;
                        if ch == ']' || ch == '}' {
                            closed = Some(ch);
                            break;
                        }
                        if ch.is_whitespace() {
                            if !w.is_empty() {
                                ws.push((std::mem::take(&mut w), i));
                            }
                        } else {
                            w.push(ch);
                        }
                    }
                    if !w.is_empty() {
                        ws.push((w, i));
                    }
                    let Some(close) = closed else { return Err(at_end(query, IN_RANGE)) };
                    ws.push((close.to_string(), i));
                    ws
                };
                let [(lo, _), (to, _), (hi, _), (close, _)] = words.as_slice() else {
                    return Err(at_end(query, IN_RANGE));
                };
                if to != "TO" {
                    return Err(at_end(query, IN_RANGE));
                }
                let bound = |b: &str| (b != "*").then(|| b.to_owned());
                Tok::Range(bound(lo), bound(hi), c == '[', close == "]")
            }
            _ => {
                let mut text = String::from(c);
                if c == '\\' {
                    text.clear();
                    if let Some(n) = chars.get(i) {
                        text.push('\\');
                        text.push(*n);
                        i += 1;
                    }
                }
                while i < len && !special(chars[i]) {
                    if chars[i] == '\\' && i + 1 < len {
                        text.push('\\');
                        i += 1;
                    }
                    text.push(chars[i]);
                    i += 1;
                }
                match text.as_str() {
                    "AND" => Tok::And(text),
                    "OR" => Tok::Or(text),
                    "NOT" => Tok::Not(text),
                    "*" => Tok::Star,
                    _ => Tok::Term(text),
                }
            }
        };
        out.push((tok, start));
    }
    Ok(out)
}

/// How a clause joins its query.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Occur {
    Must,
    Should,
    MustNot,
}

/// A parsed clause.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    /// A term, phrase, pattern or range on a field, with its boost.
    Leaf { field: String, leaf: Tok, slop: Option<Option<f64>>, boost: f32 },
    /// A parenthesized query with its boost.
    Group { clauses: Vec<(Occur, Node)>, boost: f32 },
}

struct Parser<'a> {
    query: &'a str,
    toks: Vec<(Tok, usize)>,
    at: usize,
    default_field: &'a str,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at).map(|(t, _)| t)
    }

    fn fail_here(&self, list: &[&str]) -> Fail {
        match self.toks.get(self.at) {
            Some((t, col)) => at_token(t, *col, list),
            None => at_end(self.query, list),
        }
    }

    /// A query up to the end, or up to its closing parenthesis.
    fn query(&mut self, nested: bool) -> Result<Vec<(Occur, Node)>, Fail> {
        let mut clauses: Vec<(Occur, Node)> = Vec::new();
        loop {
            let tok = self.peek().cloned();
            match tok {
                None if clauses.is_empty() => return Err(self.fail_here(START)),
                None if nested => return Err(self.fail_here(AFTER_CLAUSE)),
                None => return Ok(clauses),
                Some(Tok::Close) if nested && !clauses.is_empty() => {
                    self.at += 1;
                    return Ok(clauses);
                }
                _ => {}
            }
            let conj = match (clauses.is_empty(), self.peek()) {
                (false, Some(Tok::And(_))) => Some(true),
                (false, Some(Tok::Or(_))) => Some(false),
                _ => None,
            };
            let conj_and = conj == Some(true);
            if conj.is_some() {
                self.at += 1;
                if self.peek().is_none() {
                    let mut after = START.to_vec();
                    after.push("\"*\"");
                    return Err(self.fail_here(&after));
                }
            }
            let occur = match self.peek() {
                Some(Tok::Plus) => {
                    self.at += 1;
                    Occur::Must
                }
                Some(Tok::Minus | Tok::Not(_)) => {
                    self.at += 1;
                    Occur::MustNot
                }
                _ if conj_and => Occur::Must,
                _ => Occur::Should,
            };
            let top_after: Vec<&str> = std::iter::once("<EOF>")
                .chain(AFTER_CLAUSE.iter().copied().filter(|t| *t != "\")\""))
                .collect();
            let list = match (clauses.is_empty(), nested) {
                (true, _) => START,
                (false, true) => AFTER_CLAUSE,
                (false, false) => &top_after,
            };
            let node = self.clause(list)?;
            if conj_and && let Some(last) = clauses.last_mut().filter(|(o, _)| *o != Occur::MustNot)
            {
                last.0 = Occur::Must;
            }
            clauses.push((occur, node));
        }
    }

    fn boost(&mut self) -> Result<f32, Fail> {
        if self.peek() != Some(&Tok::Caret) {
            return Ok(1.0);
        }
        self.at += 1;
        match self.peek() {
            Some(Tok::Number(n)) => {
                let n = *n as f32;
                self.at += 1;
                Ok(n)
            }
            _ => Err(self.fail_here(&["<NUMBER>"])),
        }
    }

    fn clause(&mut self, list: &[&str]) -> Result<Node, Fail> {
        let mut field = self.default_field.to_owned();
        let mut first = list;
        if let (Some((Tok::Term(name), _)), Some((Tok::Colon, _))) =
            (self.toks.get(self.at), self.toks.get(self.at + 1))
        {
            field = unescaped(name);
            self.at += 2;
            first = AFTER_FIELD;
        }
        let Some(tok) = self.peek().cloned() else { return Err(self.fail_here(first)) };
        match tok {
            Tok::Open => {
                self.at += 1;
                let clauses = self.query(true)?;
                let boost = self.boost()?;
                Ok(Node::Group { clauses, boost })
            }
            Tok::Term(_) | Tok::Star | Tok::Quoted(_) | Tok::Regex(_) | Tok::Range(..) => {
                self.at += 1;
                let mut slop = None;
                if let Some(Tok::Fuzzy(n)) = self.peek() {
                    slop = Some(*n);
                    self.at += 1;
                }
                let boost = self.boost()?;
                Ok(Node::Leaf { field, leaf: tok, slop, boost })
            }
            _ => Err(self.fail_here(first)),
        }
    }
}

/// Removes the escapes of a term.
fn unescaped(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.extend(chars.next());
        } else {
            out.push(c);
        }
    }
    out
}

/// Whether `s` holds an unescaped `*` or `?`.
fn wild(s: &str) -> bool {
    let mut escaped = false;
    for c in s.chars() {
        if escaped {
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '*' || c == '?' {
            return true;
        }
    }
    false
}

fn to_operator(node: &Node, outer: f32) -> Operator {
    match node {
        Node::Group { clauses, boost } => compound(clauses, outer * boost),
        Node::Leaf { field, leaf, slop, boost } => {
            let score = Score::Boost(outer * boost);
            let paths = vec![field.clone()];
            match leaf {
                Tok::Quoted(text) => Operator::Phrase {
                    query: vec![text.clone()],
                    paths,
                    slop: slop.flatten().map_or(0, |n| n.max(0.0) as u32),
                    score,
                },
                Tok::Regex(re) => {
                    Operator::Pattern(Pattern::new(true, vec![re.clone()], paths, score))
                }
                Tok::Star => Operator::Pattern(Pattern::new(false, vec!["*".into()], paths, score)),
                Tok::Term(t) if wild(t) => {
                    Operator::Pattern(Pattern::new(false, vec![t.clone()], paths, score))
                }
                Tok::Range(lower, upper, incl_lower, incl_upper) => Operator::TermRange {
                    path: field.clone(),
                    lower: lower.clone().map(|l| (l, *incl_lower)),
                    upper: upper.clone().map(|u| (u, *incl_upper)),
                    score,
                },
                Tok::Term(t) => Operator::Text {
                    query: vec![unescaped(t)],
                    paths,
                    fuzzy: slop.map(|n| Fuzzy {
                        max_edits: n
                            .filter(|n| *n >= 1.0)
                            .map_or(FUZZY_EDITS, |n| n.min(2.0) as u8),
                        prefix_length: 0,
                        max_expansions: FUZZY_EXPANSIONS,
                    }),
                    score,
                },
                _ => Operator::Empty,
            }
        }
    }
}

fn compound(clauses: &[(Occur, Node)], boost: f32) -> Operator {
    let pick = |o: Occur| -> Vec<Operator> {
        clauses.iter().filter(|(c, _)| *c == o).map(|(_, n)| to_operator(n, boost)).collect()
    };
    let (must, should, must_not) = (pick(Occur::Must), pick(Occur::Should), pick(Occur::MustNot));
    if must.is_empty() && should.is_empty() {
        return Operator::Empty;
    }
    Operator::Compound { must, should, filter: Vec::new(), must_not, minimum: 0, weighted: false }
}

/// `queryString` with its options `d` at `path` of the query.
pub(super) fn parse(_index: &MemIndex, d: &Document, path: &str) -> Result<Operator, SearchError> {
    const OPTIONS: [&str; 3] = ["defaultPath", "query", "score"];
    if let Some((k, _)) = d.iter().find(|(k, _)| !OPTIONS.contains(k)) {
        return Err(SearchError(format!("\"{path}\" unrecognized field \"{k}\"")));
    }
    let at = |f: &str| format!("{path}.{f}");
    let string = |f: &str| match d.get(f) {
        None => Err(shape(&at(f), "is required")),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(shape(&at(f), "must be a string")),
    };
    let default_field = string("defaultPath")?;
    let query = string("query")?;
    let score = parse_score(d.get("score"), &at("score"))?;
    let fail = |Fail(m): Fail| SearchError(format!("Cannot parse '{query}': {m}"));
    let toks = lex(&query).map_err(fail)?;
    let mut parser = Parser { query: &query, toks, at: 0, default_field: &default_field };
    let clauses = parser.query(false).map_err(fail)?;
    let (outer, rest) = match score {
        Score::Boost(b) => (b, Score::Plain),
        other => (1.0, other),
    };
    let op = compound(&clauses, outer);
    Ok(match rest {
        Score::Plain => op,
        other => Operator::Scored(Box::new(op), other),
    })
}
