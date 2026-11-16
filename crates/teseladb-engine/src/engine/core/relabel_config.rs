//! Metric relabelling and family configuration, with the regular-expression engine they use.
//!
//! [`metrics::RelabelConfig`] rules rename, relabel, enable or disable metric series at run
//! time, in the style of Prometheus `metric_relabel_configs`; [`metrics::MetricFamilyConfig`]
//! sets the labels a family is aggregated by. Both select series with regular expressions.
//!
//! [`regex::Regex`] implements the ECMAScript regular-expression grammar for what the metrics
//! layer needs: whole-subject matching, leftmost search and `$`-substitution of the groups. The
//! accepted syntax is: literals and escapes (`\0 \f \n \r \t \v`, `\xHH`, `\uHHHH`, `\cX`),
//! `.`, bracket expressions (ranges, `\d \w \s \D \W \S`, `[\b]`, `[:class:]`, `[.c.]`,
//! `[=c=]`, the empty `[]` and `[^]`), capturing and non-capturing groups, back-references,
//! alternation, the assertions `^ $ \b \B`, the lookaheads `(?=…)` and `(?!…)`, and greedy and
//! lazy `* + ? {m} {m,} {m,n}`.
//!
//! The matcher is a depth-first backtracking machine over an explicit stack (choice points and
//! an undo log of the capture slots): it does not recurse per subject byte and does not copy
//! the captures at each choice. An iteration of an unbounded repetition that consumes nothing
//! ends the repetition (as ECMAScript requires), so `(a*)*` terminates. The worst case of the
//! depth-first search is exponential (`(a|a)*b` over `aaa…c`); its stack is on the heap, so it
//! costs time, not the thread's stack.
//!
//! The expressions come from outside (the configuration and the `/metrics` query), so their
//! size is bounded: a program of more than [`STATE_LIMIT`](regex::STATE_LIMIT) (100000) states
//! is `error_space`, checked before each state is added (a `{m,n}` that copies its atom fails
//! without reserving the whole copy). The parser, the compiler and nested lookaheads recurse
//! once per nesting level, so a nesting deeper than [`MAX_NESTING`](regex::MAX_NESTING) is
//! `error_stack` instead of a stack overflow.

use crate::engine::core::future::ExceptionPtr;

/// Regular expressions with the ECMAScript grammar.
pub mod regex {
    use std::fmt;

    use crate::engine::core::future::ExceptionPtr;

    /// The kind of a regular-expression error (the classic `error_*` codes).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ErrorType {
        /// `error_collate`: an invalid collating element name.
        Collate,
        /// `error_ctype`: an invalid character class name.
        Ctype,
        /// `error_escape`: an invalid escape or a trailing escape.
        Escape,
        /// `error_backref`: a back-reference to a group that does not exist (yet).
        Backref,
        /// `error_brack`: unbalanced or malformed `[ ]`.
        Brack,
        /// `error_paren`: unbalanced or malformed `( )`.
        Paren,
        /// `error_brace`: unbalanced `{ }`.
        Brace,
        /// `error_badbrace`: an invalid range inside `{ }`.
        Badbrace,
        /// `error_range`: an invalid character range.
        Range,
        /// `error_badrepeat`: a repeat with nothing to repeat.
        Badrepeat,
        /// `error_space`: the expression needs more states than the limit.
        Space,
        /// `error_stack`: the expression nests deeper than the limit.
        Stack,
    }

    /// The most states (instructions) a compiled expression may have.
    pub const STATE_LIMIT: usize = 100000;

    /// The deepest nesting of groups and lookaheads an expression may have (each level is a
    /// few frames of recursion in the parser, the compiler and the matcher).
    pub const MAX_NESTING: usize = 256;

    #[cold]
    fn space_error() -> ExceptionPtr {
        error(
            ErrorType::Space,
            "Number of NFA states exceeds limit. Please use shorter regex string, or use smaller brace expression.",
        )
    }

    /// An error compiling a regular expression: its kind and a message.
    #[derive(Debug, Clone)]
    pub struct RegexError {
        code: ErrorType,
        what: &'static str,
    }

    impl RegexError {
        /// The kind of the error.
        #[inline]
        pub fn code(&self) -> ErrorType {
            self.code
        }
    }

    impl fmt::Display for RegexError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.what)
        }
    }

    impl std::error::Error for RegexError {}

    #[cold]
    fn error(code: ErrorType, what: &'static str) -> ExceptionPtr {
        ExceptionPtr::new(RegexError { code, what })
    }

    /// A set of bytes, one bit per byte value.
    type ByteSet = [u64; 4];

    #[inline]
    fn set_add(set: &mut ByteSet, c: u8) {
        set[(c >> 6) as usize] |= 1u64 << (c & 63);
    }

    #[inline]
    fn set_contains(set: &ByteSet, c: u8) -> bool {
        set[(c >> 6) as usize] & (1u64 << (c & 63)) != 0
    }

    fn set_add_if(set: &mut ByteSet, pred: fn(u8) -> bool, positive: bool) {
        for c in 0..=u8::MAX {
            if pred(c) == positive {
                set_add(set, c);
            }
        }
    }

    fn is_word(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'_'
    }

    fn is_space(c: u8) -> bool {
        matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
    }

    fn is_digit(c: u8) -> bool {
        c.is_ascii_digit()
    }

    /// The named character classes (`[:alpha:]` and the others) in the "C" locale.
    fn posix_class(name: &[u8]) -> Option<fn(u8) -> bool> {
        Some(match name {
            b"alnum" => |c: u8| c.is_ascii_alphanumeric(),
            b"alpha" => |c: u8| c.is_ascii_alphabetic(),
            b"blank" => |c: u8| c == b' ' || c == b'\t',
            b"cntrl" => |c: u8| c.is_ascii_control(),
            b"digit" | b"d" => is_digit,
            b"graph" => |c: u8| c.is_ascii_graphic(),
            b"lower" => |c: u8| c.is_ascii_lowercase(),
            b"print" => |c: u8| c.is_ascii_graphic() || c == b' ',
            b"punct" => |c: u8| c.is_ascii_punctuation(),
            b"space" | b"s" => is_space,
            b"upper" => |c: u8| c.is_ascii_uppercase(),
            b"xdigit" => |c: u8| c.is_ascii_hexdigit(),
            b"w" => is_word,
            _ => return None,
        })
    }

    /// A character class: its predicate and whether it matches (`\d`) or not (`\D`).
    type QuotedClass = (fn(u8) -> bool, bool);

    /// The predicate and sense of `\d \D \s \S \w \W`.
    fn quoted_class(c: u8) -> Option<QuotedClass> {
        Some(match c {
            b'd' => (is_digit, true),
            b'D' => (is_digit, false),
            b's' => (is_space, true),
            b'S' => (is_space, false),
            b'w' => (is_word, true),
            b'W' => (is_word, false),
            _ => return None,
        })
    }

    /// The single-character escapes of ECMAScript (`\f \n \r \t \v \0` and the like).
    fn ecma_escape(c: u8) -> Option<u8> {
        Some(match c {
            b'0' => 0,
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 0x0b,
            _ => return None,
        })
    }

    #[derive(Debug, Clone)]
    enum Node {
        Empty,
        Byte(u8),
        Set(Box<ByteSet>),
        Group(Box<Node>, Option<usize>),
        Concat(Vec<Node>),
        Alt(Vec<Node>),
        Repeat(Box<Node>, u32, Option<u32>, bool),
        Bol,
        Eol,
        WordBoundary(bool),
        Backref(usize),
        Lookahead(Box<Node>, bool),
    }

    /// One element of a bracket expression.
    enum BracketItem {
        Char(u8),
        Class(fn(u8) -> bool, bool),
        Dash,
    }

    struct Parser<'a> {
        p: &'a [u8],
        i: usize,
        ngroups: usize,
        /// The groups whose `)` has not been read yet (a back-reference to them is an error).
        open: Vec<usize>,
        /// The groups and lookaheads open at the current position.
        depth: usize,
    }

    impl Parser<'_> {
        #[inline]
        fn peek(&self) -> Option<u8> {
            self.p.get(self.i).copied()
        }

        #[inline]
        fn next(&mut self, code: ErrorType, what: &'static str) -> Result<u8, ExceptionPtr> {
            let c = self.peek().ok_or_else(|| error(code, what))?;
            self.i += 1;
            Ok(c)
        }

        fn alt(&mut self) -> Result<Node, ExceptionPtr> {
            let mut alts = vec![self.concat()?];
            while self.peek() == Some(b'|') {
                self.i += 1;
                alts.push(self.concat()?);
            }
            Ok(if alts.len() == 1 { alts.swap_remove(0) } else { Node::Alt(alts) })
        }

        fn concat(&mut self) -> Result<Node, ExceptionPtr> {
            let mut items = Vec::new();
            while let Some(c) = self.peek() {
                if c == b'|' || c == b')' {
                    break;
                }
                let (atom, is_assertion) = self.atom()?;
                // An assertion takes no quantifier: a quantifier after it is read as an atom
                // and rejected.
                items.push(if is_assertion { atom } else { self.quantified(atom)? });
            }
            Ok(match items.len() {
                0 => Node::Empty,
                1 => items.swap_remove(0),
                _ => Node::Concat(items),
            })
        }

        fn number(&mut self) -> Option<u32> {
            let s = self.i;
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
            if s == self.i {
                return None;
            }
            self.p[s..self.i]
                .iter()
                .try_fold(0u32, |n, &d| n.checked_mul(10)?.checked_add(u32::from(d - b'0')))
        }

        fn quantified(&mut self, atom: Node) -> Result<Node, ExceptionPtr> {
            let (min, max) = match self.peek() {
                Some(b'*') => {
                    self.i += 1;
                    (0, None)
                }
                Some(b'+') => {
                    self.i += 1;
                    (1, None)
                }
                Some(b'?') => {
                    self.i += 1;
                    (0, Some(1))
                }
                Some(b'{') => {
                    self.i += 1;
                    let m = self.number().ok_or_else(|| {
                        error(ErrorType::Badbrace, "Unexpected token in brace expression.")
                    })?;
                    let max = if self.peek() == Some(b',') {
                        self.i += 1;
                        if self.peek() == Some(b'}') {
                            None
                        } else {
                            Some(self.number().ok_or_else(|| {
                                error(ErrorType::Badbrace, "Invalid range in brace expression.")
                            })?)
                        }
                    } else {
                        Some(m)
                    };
                    if self.peek() != Some(b'}') {
                        return Err(error(
                            ErrorType::Brace,
                            "Unexpected end of regex when in brace expression.",
                        ));
                    }
                    self.i += 1;
                    if max.is_some_and(|mx| mx < m) {
                        return Err(error(
                            ErrorType::Badbrace,
                            "Invalid range in brace expression.",
                        ));
                    }
                    (m, max)
                }
                _ => return Ok(atom),
            };
            let lazy = self.peek() == Some(b'?');
            if lazy {
                self.i += 1;
            }
            Ok(Node::Repeat(Box::new(atom), min, max, !lazy))
        }

        /// `\xHH` and `\uHHHH`: the value converted to a byte (`\u` above 0xff keeps the low
        /// byte).
        fn hex(&mut self, digits: usize) -> Result<u8, ExceptionPtr> {
            let mut v: u32 = 0;
            for _ in 0..digits {
                let d = self.peek().and_then(|c| (c as char).to_digit(16)).ok_or_else(|| {
                    error(
                        ErrorType::Escape,
                        "Invalid '\\xNN' or '\\uNNNN' control character in regular expression",
                    )
                })?;
                self.i += 1;
                v = v * 16 + d;
            }
            Ok(v as u8)
        }

        /// The escapes that read the same inside and outside brackets: `None` for the ones the
        /// caller handles (`\b \B`, the classes, the digits).
        fn common_escape(&mut self, e: u8) -> Result<Option<u8>, ExceptionPtr> {
            Ok(Some(match e {
                // `\cX` is the ordinary character X (no control-character mapping).
                b'c' => self.next(
                    ErrorType::Escape,
                    "invalid '\\cX' control character in regular expression",
                )?,
                b'x' => self.hex(2)?,
                b'u' => self.hex(4)?,
                _ => return Ok(None),
            }))
        }

        fn atom(&mut self) -> Result<(Node, bool), ExceptionPtr> {
            let c = self.next(ErrorType::Paren, "Unexpected end of regex.")?;
            Ok(match c {
                b'.' => {
                    let mut set = [u64::MAX; 4];
                    set[0] &= !((1u64 << b'\n') | (1u64 << b'\r'));
                    (Node::Set(Box::new(set)), false)
                }
                b'^' => (Node::Bol, true),
                b'$' => (Node::Eol, true),
                b'(' => self.group()?,
                b'[' => (self.bracket()?, false),
                b'\\' => self.escape()?,
                b'*' | b'+' | b'?' | b'{' => {
                    return Err(error(
                        ErrorType::Badrepeat,
                        "Nothing to repeat before a quantifier.",
                    ));
                }
                b')' => return Err(error(ErrorType::Paren, "Unexpected ')'.")),
                other => (Node::Byte(other), false),
            })
        }

        fn group(&mut self) -> Result<(Node, bool), ExceptionPtr> {
            if self.depth >= MAX_NESTING {
                return Err(error(
                    ErrorType::Stack,
                    "There was insufficient memory to determine whether the regular expression could match the specified character sequence.",
                ));
            }
            self.depth += 1;
            let r = self.group_body();
            self.depth -= 1;
            r
        }

        fn group_body(&mut self) -> Result<(Node, bool), ExceptionPtr> {
            let mut lookahead = None;
            let capture = if self.peek() == Some(b'?') {
                self.i += 1;
                match self.next(ErrorType::Paren, "Unexpected end of regex after '(?'.")? {
                    b':' => None,
                    b'=' => {
                        lookahead = Some(false);
                        None
                    }
                    b'!' => {
                        lookahead = Some(true);
                        None
                    }
                    _ => {
                        return Err(error(
                            ErrorType::Paren,
                            "Invalid '(?...)' zero-width assertion in regular expression",
                        ));
                    }
                }
            } else {
                self.ngroups += 1;
                self.open.push(self.ngroups);
                Some(self.ngroups)
            };
            let inner = self.alt()?;
            if self.peek() != Some(b')') {
                return Err(error(ErrorType::Paren, "Parenthesis is not closed."));
            }
            self.i += 1;
            if capture.is_some() {
                self.open.pop();
            }
            Ok(match lookahead {
                Some(neg) => (Node::Lookahead(Box::new(inner), neg), true),
                None => (Node::Group(Box::new(inner), capture), false),
            })
        }

        fn escape(&mut self) -> Result<(Node, bool), ExceptionPtr> {
            let e = self.next(ErrorType::Escape, "Unexpected end of regex when escaping.")?;
            if e == b'b' {
                return Ok((Node::WordBoundary(true), true));
            }
            if e == b'B' {
                return Ok((Node::WordBoundary(false), true));
            }
            if let Some(b) = ecma_escape(e) {
                return Ok((Node::Byte(b), false));
            }
            if let Some((pred, positive)) = quoted_class(e) {
                let mut set = [0u64; 4];
                set_add_if(&mut set, pred, positive);
                return Ok((Node::Set(Box::new(set)), false));
            }
            if e.is_ascii_digit() {
                // ECMAScript recognizes multi-digit back-references.
                self.i -= 1;
                let n = self
                    .number()
                    .ok_or_else(|| error(ErrorType::Backref, "Invalid back reference."))?
                    as usize;
                if n > self.ngroups || self.open.contains(&n) {
                    return Err(error(
                        ErrorType::Backref,
                        "Back-reference index exceeds current sub-expression count.",
                    ));
                }
                return Ok((Node::Backref(n), false));
            }
            Ok((Node::Byte(self.common_escape(e)?.unwrap_or(e)), false))
        }

        /// The element after `[` inside a bracket: `[:name:]`, `[.c.]` or `[=c=]`.
        fn bracket_name(&mut self, kind: u8) -> Result<BracketItem, ExceptionPtr> {
            let start = self.i;
            while !(self.peek() == Some(kind) && self.p.get(self.i + 1) == Some(&b']')) {
                if self.peek().is_none() {
                    return Err(error(
                        ErrorType::Brack,
                        "Unexpected character class open bracket.",
                    ));
                }
                self.i += 1;
            }
            let name = &self.p[start..self.i];
            self.i += 2;
            if kind == b':' {
                let pred = posix_class(name)
                    .ok_or_else(|| error(ErrorType::Ctype, "Invalid character class."))?;
                return Ok(BracketItem::Class(pred, true));
            }
            // Collating elements and equivalence classes of one character, as the "C" locale has.
            match name {
                [c] => Ok(BracketItem::Char(*c)),
                _ => Err(error(ErrorType::Collate, "Invalid collate element.")),
            }
        }

        fn bracket_item(&mut self, c: u8) -> Result<BracketItem, ExceptionPtr> {
            Ok(match c {
                b'[' if matches!(self.peek(), Some(b':' | b'.' | b'=')) => {
                    let kind = self.p[self.i];
                    self.i += 1;
                    self.bracket_name(kind)?
                }
                b'\\' => {
                    let e =
                        self.next(ErrorType::Escape, "Unexpected end of regex when escaping.")?;
                    if let Some(b) = ecma_escape(e) {
                        // `[\b]` is the backspace.
                        BracketItem::Char(b)
                    } else if let Some((pred, positive)) = quoted_class(e) {
                        BracketItem::Class(pred, positive)
                    } else if e.is_ascii_digit() || e == b'B' {
                        return Err(error(
                            ErrorType::Brack,
                            "Unexpected character within '[...]' in regular expression",
                        ));
                    } else {
                        BracketItem::Char(self.common_escape(e)?.unwrap_or(e))
                    }
                }
                b'-' => BracketItem::Dash,
                other => BracketItem::Char(other),
            })
        }

        /// A bracket expression, after its `[`.
        fn bracket(&mut self) -> Result<Node, ExceptionPtr> {
            const UNCLOSED: &str = "Unexpected end of regex when in bracket expression.";
            let negated = self.peek() == Some(b'^');
            if negated {
                self.i += 1;
            }
            let mut set = [0u64; 4];
            // The previous element: a character that may start a range, or a class.
            let mut prev_char: Option<u8> = None;
            let mut prev_class = false;
            loop {
                let c = self.next(ErrorType::Brack, UNCLOSED)?;
                // In ECMAScript `]` closes the bracket also right after `[` (`[]`, `[^]`).
                if c == b']' {
                    break;
                }
                match self.bracket_item(c)? {
                    BracketItem::Char(ch) => {
                        set_add(&mut set, ch);
                        prev_char = Some(ch);
                        prev_class = false;
                    }
                    BracketItem::Class(pred, positive) => {
                        set_add_if(&mut set, pred, positive);
                        prev_char = None;
                        prev_class = true;
                    }
                    BracketItem::Dash => {
                        if self.peek() == Some(b']') {
                            set_add(&mut set, b'-');
                            continue;
                        }
                        if prev_class {
                            return Err(error(
                                ErrorType::Range,
                                "Invalid start of '[x-x]' range in regular expression",
                            ));
                        }
                        let Some(lo) = prev_char else {
                            // A dash that does not end a range is an ordinary '-' in ECMAScript.
                            set_add(&mut set, b'-');
                            prev_char = Some(b'-');
                            continue;
                        };
                        let n = self.next(ErrorType::Brack, UNCLOSED)?;
                        let hi = match self.bracket_item(n)? {
                            BracketItem::Char(h) => h,
                            BracketItem::Dash => b'-',
                            BracketItem::Class(..) => {
                                return Err(error(
                                    ErrorType::Range,
                                    "Invalid end of '[x-x]' range in regular expression",
                                ));
                            }
                        };
                        if hi < lo {
                            return Err(error(
                                ErrorType::Range,
                                "Invalid range in bracket expression.",
                            ));
                        }
                        for ch in lo..=hi {
                            set_add(&mut set, ch);
                        }
                        prev_char = None;
                    }
                }
            }
            if negated {
                for w in &mut set {
                    *w = !*w;
                }
            }
            Ok(Node::Set(Box::new(set)))
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Inst {
        Byte(u8),
        /// Index into `Regex::sets`.
        Set(usize),
        Split(usize, usize),
        Jmp(usize),
        Save(usize),
        /// Records the position at the start of an iteration of an unbounded repetition
        /// (a capture-like slot, restored on backtracking).
        Mark(usize),
        /// Fails the iteration if it matched nothing since its `Mark`: ECMAScript rejects an
        /// empty iteration, so `(a*)*` ends instead of looping.
        Progress(usize),
        Bol,
        Eol,
        WordBoundary(bool),
        Backref(usize),
        /// A lookahead whose body follows and ends with `LookEnd`; `end` is the pc after it.
        Look {
            neg: bool,
            end: usize,
        },
        LookEnd,
        Match,
        /// Matches nothing (the program of [`Regex::empty`]).
        Fail,
    }

    struct Compiler {
        prog: Vec<Inst>,
        sets: Vec<ByteSet>,
        next_slot: usize,
    }

    impl Compiler {
        /// Adds a state, or fails with `error_space` past the limit.
        #[inline]
        fn push(&mut self, inst: Inst) -> Result<(), ExceptionPtr> {
            if self.prog.len() >= STATE_LIMIT {
                return Err(space_error());
            }
            self.prog.push(inst);
            Ok(())
        }

        /// One optional iteration of a repetition past its minimum, rejected if it matches
        /// nothing.
        fn emit_optional(&mut self, inner: &Node, slot: usize) -> Result<(), ExceptionPtr> {
            self.push(Inst::Mark(slot))?;
            self.emit(inner)?;
            self.push(Inst::Progress(slot))
        }

        fn emit(&mut self, node: &Node) -> Result<(), ExceptionPtr> {
            match node {
                Node::Empty => {}
                Node::Byte(b) => self.push(Inst::Byte(*b))?,
                Node::Set(set) => {
                    self.push(Inst::Set(self.sets.len()))?;
                    self.sets.push(**set);
                }
                Node::Bol => self.push(Inst::Bol)?,
                Node::Eol => self.push(Inst::Eol)?,
                Node::WordBoundary(b) => self.push(Inst::WordBoundary(*b))?,
                Node::Backref(n) => self.push(Inst::Backref(*n))?,
                Node::Group(inner, cap) => match cap {
                    Some(n) => {
                        self.push(Inst::Save(2 * n))?;
                        self.emit(inner)?;
                        self.push(Inst::Save(2 * n + 1))?;
                    }
                    None => self.emit(inner)?,
                },
                Node::Lookahead(inner, neg) => {
                    let at = self.prog.len();
                    self.push(Inst::Look { neg: *neg, end: 0 })?;
                    self.emit(inner)?;
                    self.push(Inst::LookEnd)?;
                    self.prog[at] = Inst::Look { neg: *neg, end: self.prog.len() };
                }
                Node::Concat(items) => {
                    for n in items {
                        self.emit(n)?;
                    }
                }
                Node::Alt(alts) => {
                    let mut jumps = Vec::with_capacity(alts.len());
                    for (k, a) in alts.iter().enumerate() {
                        if k + 1 < alts.len() {
                            let split = self.prog.len();
                            self.push(Inst::Split(0, 0))?;
                            self.emit(a)?;
                            jumps.push(self.prog.len());
                            self.push(Inst::Jmp(0))?;
                            self.prog[split] = Inst::Split(split + 1, self.prog.len());
                        } else {
                            self.emit(a)?;
                        }
                    }
                    let end = self.prog.len();
                    for j in jumps {
                        self.prog[j] = Inst::Jmp(end);
                    }
                }
                Node::Repeat(inner, min, max, greedy) => {
                    // The atom's states are copied for each repetition, which needs at least one
                    // state per copy: a count past the limit fails at once, before any copy.
                    if max.unwrap_or(*min) as usize > STATE_LIMIT {
                        return Err(space_error());
                    }
                    for k in 0..*min {
                        let before = self.prog.len();
                        self.emit(inner)?;
                        // An atom that compiles to no state (`(?:)`) gives the same nothing on
                        // every copy: stop instead of looping over the count.
                        if k == 0 && self.prog.len() == before {
                            break;
                        }
                    }
                    let fork = |s: usize, end: usize| {
                        if *greedy { Inst::Split(s + 1, end) } else { Inst::Split(end, s + 1) }
                    };
                    let slot = self.next_slot;
                    self.next_slot += 1;
                    match max {
                        None => {
                            let split = self.prog.len();
                            self.push(Inst::Split(0, 0))?;
                            self.emit_optional(inner, slot)?;
                            self.push(Inst::Jmp(split))?;
                            self.prog[split] = fork(split, self.prog.len());
                        }
                        Some(mx) => {
                            let mut splits = Vec::with_capacity((*mx - *min) as usize);
                            for _ in *min..*mx {
                                splits.push(self.prog.len());
                                self.push(Inst::Split(0, 0))?;
                                self.emit_optional(inner, slot)?;
                            }
                            let end = self.prog.len();
                            for s in splits {
                                self.prog[s] = fork(s, end);
                            }
                        }
                    }
                }
            }
            Ok(())
        }
    }

    /// A slot that holds no position.
    const NONE: usize = usize::MAX;

    /// An entry of the backtracking stack.
    #[derive(Clone, Copy)]
    enum Frame {
        /// Where to resume when the current path fails.
        Choice { pc: usize, sp: usize },
        /// The previous value of a slot, restored when backtracking past this point.
        Undo { slot: usize, old: usize },
    }

    /// A compiled regular expression.
    #[derive(Debug, Clone)]
    pub struct Regex {
        prog: Vec<Inst>,
        sets: Vec<ByteSet>,
        ngroups: usize,
        /// The capture slots plus one mark slot per unbounded repetition.
        nslots: usize,
    }

    /// The groups of a successful match: byte ranges into the subject.
    #[derive(Debug, Clone, Default)]
    pub struct Match {
        groups: Vec<Option<(usize, usize)>>,
    }

    impl Match {
        /// The range of group `n` (0 is the whole match), if it participated.
        #[inline]
        pub fn group(&self, n: usize) -> Option<(usize, usize)> {
            self.groups.get(n).copied().flatten()
        }

        /// Expands the replacement pattern `fmt` with the ECMAScript format rules: `$&`, `` $` ``, `$'`,
        /// `$$`, `$n` and `$nn`; any other `$` is copied as is.
        pub fn format<S: AsRef<[u8]> + ?Sized>(&self, subject: &S, fmt: &str) -> Vec<u8> {
            let s = subject.as_ref();
            let f = fmt.as_bytes();
            let mut out = Vec::with_capacity(fmt.len());
            let whole = self.group(0).unwrap_or((0, 0));
            let mut i = 0;
            while i < f.len() {
                if f[i] != b'$' || i + 1 >= f.len() {
                    out.push(f[i]);
                    i += 1;
                    continue;
                }
                let n = f[i + 1];
                match n {
                    b'$' => {
                        out.push(b'$');
                        i += 2;
                    }
                    b'&' => {
                        out.extend_from_slice(&s[whole.0..whole.1]);
                        i += 2;
                    }
                    b'`' => {
                        out.extend_from_slice(&s[..whole.0]);
                        i += 2;
                    }
                    b'\'' => {
                        out.extend_from_slice(&s[whole.1..]);
                        i += 2;
                    }
                    b'0'..=b'9' => {
                        let mut num = (n - b'0') as usize;
                        i += 2;
                        if i < f.len() && f[i].is_ascii_digit() {
                            num = num * 10 + (f[i] - b'0') as usize;
                            i += 1;
                        }
                        if let Some((a, b)) = self.group(num) {
                            out.extend_from_slice(&s[a..b]);
                        }
                    }
                    _ => {
                        out.push(b'$');
                        i += 1;
                    }
                }
            }
            out
        }
    }

    /// Drops the frames above `base` restoring the slots they saved.
    fn unwind(stack: &mut Vec<Frame>, base: usize, caps: &mut [usize]) {
        while stack.len() > base {
            if let Some(Frame::Undo { slot, old }) = stack.pop() {
                caps[slot] = old;
            }
        }
    }

    /// Drops the choice points above `base` and keeps their undo entries: a lookahead that
    /// matched is not re-entered on backtracking, but its captures are still undone.
    fn commit(stack: &mut Vec<Frame>, base: usize) {
        let mut w = base;
        for r in base..stack.len() {
            if let Frame::Undo { .. } = stack[r] {
                stack[w] = stack[r];
                w += 1;
            }
        }
        stack.truncate(w);
    }

    impl Regex {
        /// Compiles `expr`.
        ///
        /// # Errors
        ///
        /// Returns a [`RegexError`] for a malformed or too large expression.
        pub fn new(expr: &str) -> Result<Self, ExceptionPtr> {
            let mut parser =
                Parser { p: expr.as_bytes(), i: 0, ngroups: 0, open: Vec::new(), depth: 0 };
            let ast = parser.alt()?;
            if parser.i != parser.p.len() {
                return Err(error(ErrorType::Paren, "Unexpected ')'."));
            }
            let mut c = Compiler {
                prog: Vec::new(),
                sets: Vec::new(),
                next_slot: 2 * (parser.ngroups + 1),
            };
            c.emit(&ast)?;
            c.push(Inst::Match)?;
            Ok(Self { prog: c.prog, sets: c.sets, ngroups: parser.ngroups, nslots: c.next_slot })
        }

        /// An expression that matches nothing.
        pub fn empty() -> Self {
            Self { prog: vec![Inst::Fail], sets: Vec::new(), ngroups: 0, nslots: 2 }
        }

        /// Runs the program from `pc` at `sp`; frames below the stack's current length belong to
        /// an enclosing run and are left alone. Returns the end of the match.
        fn exec(
            &self,
            s: &[u8],
            mut pc: usize,
            mut sp: usize,
            caps: &mut [usize],
            stack: &mut Vec<Frame>,
            full: bool,
        ) -> Option<usize> {
            let base = stack.len();
            loop {
                let ok = match self.prog[pc] {
                    Inst::Match => {
                        if !full || sp == s.len() {
                            return Some(sp);
                        }
                        false
                    }
                    Inst::LookEnd => return Some(sp),
                    Inst::Fail => false,
                    Inst::Byte(b) => {
                        let ok = s.get(sp) == Some(&b);
                        sp += ok as usize;
                        ok
                    }
                    Inst::Set(k) => {
                        let ok = s.get(sp).is_some_and(|&c| set_contains(&self.sets[k], c));
                        sp += ok as usize;
                        ok
                    }
                    Inst::Bol => sp == 0,
                    Inst::Eol => sp == s.len(),
                    Inst::WordBoundary(want) => {
                        let before = sp > 0 && is_word(s[sp - 1]);
                        let after = sp < s.len() && is_word(s[sp]);
                        (before != after) == want
                    }
                    Inst::Backref(n) => {
                        let (a, b) = (caps[2 * n], caps[2 * n + 1]);
                        // A group that did not participate matches the empty string.
                        let len = if a == NONE || b == NONE || b < a { 0 } else { b - a };
                        let ok =
                            s.len() - sp >= len && s[a.min(s.len())..][..len] == s[sp..sp + len];
                        sp += if ok { len } else { 0 };
                        ok
                    }
                    Inst::Jmp(t) => {
                        pc = t;
                        continue;
                    }
                    Inst::Split(a, b) => {
                        stack.push(Frame::Choice { pc: b, sp });
                        pc = a;
                        continue;
                    }
                    Inst::Save(slot) | Inst::Mark(slot) => {
                        stack.push(Frame::Undo { slot, old: caps[slot] });
                        caps[slot] = sp;
                        true
                    }
                    Inst::Progress(slot) => caps[slot] != sp,
                    Inst::Look { neg, end } => {
                        let inner = stack.len();
                        let found = self.exec(s, pc + 1, sp, caps, stack, false).is_some();
                        match (found, neg) {
                            (true, false) => {
                                commit(stack, inner);
                                pc = end;
                                continue;
                            }
                            (true, true) => {
                                unwind(stack, inner, caps);
                                false
                            }
                            (false, false) => false,
                            (false, true) => {
                                pc = end;
                                continue;
                            }
                        }
                    }
                };
                if ok {
                    pc += 1;
                    continue;
                }
                // Backtrack to the last choice point of this run.
                loop {
                    if stack.len() == base {
                        return None;
                    }
                    match stack.pop() {
                        Some(Frame::Undo { slot, old }) => caps[slot] = old,
                        Some(Frame::Choice { pc: p, sp: q }) => {
                            pc = p;
                            sp = q;
                            break;
                        }
                        None => return None,
                    }
                }
            }
        }

        fn groups(&self, caps: &[usize], start: usize, end: usize) -> Match {
            let mut groups = Vec::with_capacity(self.ngroups + 1);
            groups.push(Some((start, end)));
            for n in 1..=self.ngroups {
                groups.push(match (caps[2 * n], caps[2 * n + 1]) {
                    (a, b) if a != NONE && b != NONE => Some((a, b)),
                    _ => None,
                });
            }
            Match { groups }
        }

        /// Whether the whole subject matches.
        pub fn is_full_match<S: AsRef<[u8]> + ?Sized>(&self, s: &S) -> bool {
            let mut caps = vec![NONE; self.nslots];
            let mut stack = Vec::with_capacity(self.prog.len());
            self.exec(s.as_ref(), 0, 0, &mut caps, &mut stack, true).is_some()
        }

        /// The leftmost match anywhere in the subject, if any.
        pub fn search<S: AsRef<[u8]> + ?Sized>(&self, s: &S) -> Option<Match> {
            let b = s.as_ref();
            let mut caps = vec![NONE; self.nslots];
            let mut stack = Vec::with_capacity(self.prog.len());
            for start in 0..=b.len() {
                caps.fill(NONE);
                stack.clear();
                if let Some(end) = self.exec(b, 0, start, &mut caps, &mut stack, false) {
                    return Some(self.groups(&caps, start, end));
                }
            }
            None
        }
    }

    impl Default for Regex {
        /// An expression that matches nothing.
        fn default() -> Self {
            Self::empty()
        }
    }
}

/// The relabelling and family configuration of the metrics layer.
pub mod metrics {
    use super::regex::Regex;
    use super::*;

    /// A compiled regular expression together with its source text, which [`Regex`] does not
    /// keep; it acts both as a string and as a regex.
    #[derive(Debug, Clone, Default)]
    pub struct RelabelConfigRegex {
        _regex_str: String,
        _regex: Regex,
    }

    impl RelabelConfigRegex {
        /// Compiles `expr`.
        ///
        /// # Errors
        ///
        /// Returns the compilation error of [`Regex::new`].
        pub fn new(expr: &str) -> Result<Self, ExceptionPtr> {
            Ok(Self { _regex_str: expr.to_string(), _regex: Regex::new(expr)? })
        }

        /// The source text.
        #[inline]
        pub fn str(&self) -> &str {
            &self._regex_str
        }

        /// The compiled expression.
        #[inline]
        pub fn regex(&self) -> &Regex {
            &self._regex
        }

        /// Replaces the expression. The text changes before the expression compiles, so a
        /// failed compilation leaves the new text and the old regex.
        ///
        /// # Errors
        ///
        /// Returns the compilation error of [`Regex::new`].
        pub fn assign(&mut self, expr: &str) -> Result<(), ExceptionPtr> {
            self._regex_str = expr.to_string();
            self._regex = Regex::new(&self._regex_str)?;
            Ok(())
        }

        /// Whether no expression was given.
        #[inline]
        pub fn empty(&self) -> bool {
            self._regex_str.is_empty()
        }

        /// Whether the expression is set and matches the whole of `str`.
        #[inline]
        pub fn match_(&self, str: &str) -> bool {
            !self.empty() && self._regex.is_full_match(str)
        }
    }

    /// What a relabel config does on a match.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum RelabelAction {
        /// Stop reporting a series (histogram, summary or counter) while it was never used.
        SkipWhenEmpty,
        /// Report the series even when unused (undoes `SkipWhenEmpty`).
        ReportWhenEmpty,
        /// Set the target label to the replacement.
        Replace,
        /// Enable the series.
        Keep,
        /// Disable the series.
        Drop,
        /// Remove the target label.
        DropLabel,
    }

    /// A rule that changes metric labels at run time.
    ///
    /// The logic follows the Prometheus relabelling configuration, where an entry looks like
    /// `source_labels: [version]`, `regex: '([0-9]+\.[0-9]+)(\.?[0-9]*).*'`,
    /// `replacement: '$1$2'`, `target_label: svr`. The source labels are joined with the
    /// separator; when `expr` matches the joined value, `action` is applied.
    ///
    /// The default rule replaces the target label with the first group of `(.*)`, joining the
    /// source labels with `;`.
    #[derive(Debug, Clone)]
    pub struct RelabelConfig {
        /// The labels whose values are joined and matched (`__name__` is the metric name).
        pub source_labels: Vec<String>,
        /// The label to set or remove.
        pub target_label: String,
        /// The new value of the target label; `$n` refers to the groups of the match.
        pub replacement: String,
        /// The expression the joined source values must match.
        pub expr: RelabelConfigRegex,
        /// What to do on a match.
        pub action: RelabelAction,
        /// The separator used to join the source label values.
        pub separator: String,
    }

    impl Default for RelabelConfig {
        fn default() -> Self {
            Self {
                source_labels: Vec::new(),
                target_label: String::new(),
                replacement: "${1}".to_string(),
                expr: RelabelConfigRegex {
                    _regex_str: "(.*)".to_string(),
                    // "(.*)" always compiles.
                    _regex: Regex::new("(.*)").unwrap_or_default(),
                },
                action: RelabelAction::Replace,
                separator: ";".to_string(),
            }
        }
    }

    /// Parses an action name into a [`RelabelAction`].
    pub use crate::engine::core::metrics::relabel_config_action;

    /// A metric family configuration: the labels a family is aggregated by.
    ///
    /// The family is selected by an exact name or by a regular expression; name matching is
    /// cheaper and should be used for a single family.
    #[derive(Debug, Clone)]
    pub struct MetricFamilyConfig {
        /// The exact family name, or empty.
        pub name: String,
        /// If set, applies to every family whose name matches.
        pub regex_name: RelabelConfigRegex,
        /// The labels to aggregate the series by.
        pub aggregate_labels: Vec<String>,
    }

    impl Default for MetricFamilyConfig {
        fn default() -> Self {
            Self {
                name: String::new(),
                // The empty expression, compiled (it matches the empty string), not
                // `Regex::empty()`, which matches nothing.
                regex_name: RelabelConfigRegex {
                    _regex_str: String::new(),
                    // "" always compiles.
                    _regex: Regex::new("").unwrap_or_default(),
                },
                aggregate_labels: Vec::new(),
            }
        }
    }
}
