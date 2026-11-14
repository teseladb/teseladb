//! The syntax tree of a pattern and its character classes. Options that apply lexically (`i`,
//! `m`, `s`, UCP) are resolved by the parser and carried by each node.

use super::unicode::{
    bidi_class, case_variants, category_bit, has_binary, has_script_extension, in_categories,
    script,
};

/// A node of the tree.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Node {
    /// Matches the empty string.
    Empty,
    /// One character; `caseless` matches its case variants too.
    Char {
        /// The character.
        c: char,
        /// How case is matched.
        caseless: Case,
    },
    /// `.`: any character, the newline only with `dotall`.
    Dot {
        /// `s` option.
        dotall: bool,
    },
    /// A character class or a class escape.
    Class(Class),
    /// A zero-width assertion.
    Assert(Assertion),
    /// A group, capturing when it has an index.
    Group(Box<Node>, Option<usize>),
    /// A sequence.
    Concat(Vec<Node>),
    /// Alternatives in order.
    Alt(Vec<Node>),
    /// A repetition.
    Repeat {
        /// What repeats.
        node: Box<Node>,
        /// Minimum count.
        min: u32,
        /// Maximum count, `None` without limit.
        max: Option<u32>,
        /// Greedy, lazy or possessive.
        kind: RepeatKind,
    },
    /// A back reference to the groups listed (several with duplicate names): the first one set.
    Backref {
        /// Group numbers.
        groups: Vec<usize>,
        /// How case is matched.
        caseless: Case,
    },
    /// A back reference by name, before the parser resolves it to its groups.
    NamedBackref {
        /// Group name.
        name: String,
        /// How case is matched.
        caseless: Case,
    },
    /// A lookahead or lookbehind.
    Look {
        /// Lookbehind.
        behind: bool,
        /// Negative.
        negate: bool,
        /// Atomic (Perl's); a non-atomic one can be backtracked into (`(*napla:`, `(?*`).
        atomic: bool,
        /// The assertion's pattern.
        node: Box<Node>,
    },
    /// A call to a group as a subroutine, 0 being the whole pattern (`(?R)`, `(?1)`, `(?&n)`).
    Call(usize),
    /// A call by name, before the parser resolves it.
    NamedCall(String),
    /// A backtracking control verb.
    Verb(Verb),
    /// `(*script_run:...)`: what the pattern matches must be a script run.
    ScriptRun(Box<Node>),
    /// An atomic group.
    Atomic(Box<Node>),
    /// A conditional group.
    Cond {
        /// The condition.
        cond: Condition,
        /// Pattern when it holds.
        yes: Box<Node>,
        /// Pattern when it does not.
        no: Box<Node>,
    },
    /// `\K`: the reported match starts here.
    KeepOut,
    /// `(*FAIL)`.
    Fail,
    /// `\X`: an extended grapheme cluster.
    Grapheme,
    /// `\R`: a newline sequence.
    NewlineSeq,
}

/// How letters match their other cases.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Case {
    /// Only themselves.
    #[default]
    Sensitive,
    /// Every case variant (`i`).
    Insensitive,
    /// Case variants on the same side of ASCII only (`i` with `(?r)`).
    Restricted,
}

impl Case {
    /// The characters that match `c`, `c` included.
    pub(super) fn variants(self, c: char) -> Vec<char> {
        match self {
            Case::Sensitive => vec![c],
            Case::Insensitive => case_variants(c),
            Case::Restricted => {
                case_variants(c).into_iter().filter(|v| v.is_ascii() == c.is_ascii()).collect()
            }
        }
    }
}

/// How a repetition chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RepeatKind {
    /// As many as possible, giving back on failure.
    Greedy,
    /// As few as possible.
    Lazy,
    /// As many as possible, never giving back.
    Possessive,
}

/// The condition of a conditional group.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Condition {
    /// One of these groups has matched.
    Group(Vec<usize>),
    /// A group by name, before the parser resolves it; bare, `(?(R)` is a recursion test.
    Name(String),
    /// A group by name in `<>` or `''`, before the parser resolves it.
    QuotedName(String),
    /// An assertion holds.
    Look {
        /// Lookbehind.
        behind: bool,
        /// Negative.
        negate: bool,
        /// The assertion's pattern.
        node: Box<Node>,
    },
    /// `(?(DEFINE)...)`: never holds.
    Define,
    /// `(?(R)`: inside any call; `(?(Rn)` and `(?(R&name)`: the latest call is to one of the groups.
    Recursion(Option<Vec<usize>>),
    /// `(?(R&name)`, before the parser resolves it.
    RecursionName(String),
    /// `(?(VERSION>=n.m)`: decided when the pattern is compiled.
    Fixed(bool),
}

/// Backtracking control verbs. The names of `(*COMMIT:n)`, `(*PRUNE:n)` and `(*THEN:n)` are only
/// reported to the caller, which the reference server never asks for, so they are dropped.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Verb {
    /// `(*ACCEPT)`: the match, or the called group, ends successfully here.
    Accept,
    /// `(*COMMIT)`: backtracking onto it fails the whole match.
    Commit,
    /// `(*PRUNE)`: backtracking onto it fails the match at this start.
    Prune,
    /// `(*SKIP)`: like `PRUNE`, and the next start is where it was passed.
    Skip,
    /// `(*SKIP:name)`: the next start is the latest `(*MARK:name)` on the path, if any.
    SkipTo(String),
    /// `(*THEN)`: backtracking onto it goes to the next alternative of the innermost alternation.
    Then,
    /// `(*MARK:name)`.
    Mark(String),
}

/// Zero-width assertions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Assertion {
    /// `^`: start of the subject, or of a line with `multiline`.
    Start {
        /// `m` option.
        multiline: bool,
    },
    /// `$`: end of the subject or before its final newline, or end of a line with `multiline`.
    End {
        /// `m` option.
        multiline: bool,
    },
    /// `\A`.
    SubjectStart,
    /// `\Z`: end or before a final newline.
    SubjectEndNewline,
    /// `\z`.
    SubjectEnd,
    /// `\G`: the offset where the search started.
    MatchStart,
    /// A boundary of extended grapheme clusters (Java's `\b{g}`, only in translated patterns).
    GraphemeBoundary,
    /// `\b`, or `\B` when `negate`.
    WordBoundary {
        /// `\B`.
        negate: bool,
        /// Unicode word characters.
        ucp: bool,
    },
}

/// A character class: its items, matched caselessly or not, possibly negated.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Class {
    /// `[^...]`.
    pub negated: bool,
    /// Items; a character matches the class if it matches one of them.
    pub items: Vec<Item>,
    /// How case is matched.
    pub caseless: Case,
}

/// An item of a class.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Item {
    /// Characters from the first to the last.
    Range(char, char),
    /// A class escape, a POSIX class or a Unicode property, possibly negated.
    Set {
        /// Which set.
        set: Set,
        /// Negated (`\D`, `[:^alpha:]`, `\P`).
        negate: bool,
    },
}

/// Named sets of characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Set {
    /// `\d`.
    Digit {
        /// Unicode decimal digits.
        ucp: bool,
    },
    /// `\w`.
    Word {
        /// Unicode letters and digits.
        ucp: bool,
    },
    /// `\s`.
    Space {
        /// Unicode spaces.
        ucp: bool,
    },
    /// `\h`.
    HSpace,
    /// `\v`.
    VSpace,
    /// A POSIX class.
    Posix {
        /// Which one.
        class: Posix,
        /// With UCP.
        ucp: bool,
    },
    /// A Unicode property.
    Prop(Prop),
}

/// POSIX classes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Posix {
    /// `alnum`.
    Alnum,
    /// `alpha`.
    Alpha,
    /// `ascii`.
    Ascii,
    /// `blank`.
    Blank,
    /// `cntrl`.
    Cntrl,
    /// `digit`.
    Digit,
    /// `graph`.
    Graph,
    /// `lower`.
    Lower,
    /// `print`.
    Print,
    /// `punct`.
    Punct,
    /// `space`.
    Space,
    /// `upper`.
    Upper,
    /// `word`.
    Word,
    /// `xdigit`.
    Xdigit,
}

/// Unicode properties of `\p`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Prop {
    /// Any character.
    Any,
    /// General categories, a mask of bits.
    Categories(u32),
    /// A script (`sc:`).
    Script(u8),
    /// A script among the script extensions (the default and `scx:`).
    ScriptExtension(u8),
    /// Letters and numbers.
    Xan,
    /// POSIX spaces: separators and the ASCII spaces.
    Xps,
    /// Perl spaces (the same set since PCRE2).
    Xsp,
    /// Word characters.
    Xwd,
    /// Characters of a universal character name.
    Xuc,
    /// A binary property, by its index in the generated tables.
    Binary(u8),
    /// A bidi class, by its index in the generated tables.
    Bidi(u8),
}

impl Class {
    /// Whether `c` belongs to the class.
    pub(super) fn matches(&self, c: char) -> bool {
        let hit = if self.caseless != Case::Sensitive {
            self.caseless
                .variants(c)
                .into_iter()
                .any(|v| self.items.iter().any(|i| self.item(i, v)))
        } else {
            self.items.iter().any(|i| self.item(i, c))
        };
        hit != self.negated
    }

    /// Whether item `i` matches `c`. In PCRE2 10.43 a negated class with a Unicode property
    /// tests characters above U+00FF only against its properties and ranges: `\D`, `\W` and `\S`
    /// without UCP do not take part (`[^\D\P{Nd}]` matches U+1D7CF, testoutput5).
    fn item(&self, i: &Item, c: char) -> bool {
        let ascii_negation = matches!(
            i,
            Item::Set {
                set: Set::Digit { ucp: false }
                    | Set::Word { ucp: false }
                    | Set::Space { ucp: false },
                negate: true,
            }
        );
        if ascii_negation && self.negated && u32::from(c) > 0xFF && self.has_property() {
            return false;
        }
        i.matches(c)
    }

    fn has_property(&self) -> bool {
        self.items.iter().any(|i| matches!(i, Item::Set { set: Set::Prop(_), .. }))
    }
}

impl Item {
    fn matches(&self, c: char) -> bool {
        match self {
            Item::Range(lo, hi) => (*lo..=*hi).contains(&c),
            Item::Set { set, negate } => set.matches(c) != *negate,
        }
    }
}

/// `\h`: horizontal white space.
pub(super) fn is_hspace(c: char) -> bool {
    matches!(
        c,
        '\t' | ' ' | '\u{A0}' | '\u{1680}' | '\u{180E}' | '\u{2000}'
            ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// `\v`: vertical white space.
pub(super) fn is_vspace(c: char) -> bool {
    matches!(c, '\n'..='\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `\w` without UCP.
pub(super) fn is_ascii_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// `\w` with UCP: letters, numbers, non-spacing marks and connector punctuation.
pub(super) fn is_unicode_word(c: char) -> bool {
    const MASK: &[&str] = &["Mn", "Pc"];
    is_letter_or_number(c) || MASK.iter().any(|m| in_categories(c, category_bit(m)))
}

fn is_letter_or_number(c: char) -> bool {
    const LN: &[&str] = &["Lu", "Ll", "Lt", "Lm", "Lo", "Nd", "Nl", "No"];
    LN.iter().any(|m| in_categories(c, category_bit(m)))
}

/// Separators (`Z`) and the ASCII white space.
fn is_unicode_space(c: char) -> bool {
    const Z: &[&str] = &["Zs", "Zl", "Zp"];
    matches!(c, '\t'..='\r') || Z.iter().any(|m| in_categories(c, category_bit(m)))
}

impl Set {
    pub(super) fn matches(&self, c: char) -> bool {
        match *self {
            Set::Digit { ucp: false } => c.is_ascii_digit(),
            Set::Digit { ucp: true } => in_categories(c, category_bit("Nd")),
            Set::Word { ucp: false } => is_ascii_word(c),
            Set::Word { ucp: true } => is_unicode_word(c),
            Set::Space { ucp: false } => matches!(c, '\t'..='\r' | ' '),
            Set::Space { ucp: true } => is_unicode_space(c),
            Set::HSpace => is_hspace(c),
            Set::VSpace => is_vspace(c),
            Set::Posix { class, ucp } => posix(class, ucp, c),
            Set::Prop(p) => prop(p, c),
        }
    }
}

fn posix(class: Posix, ucp: bool, c: char) -> bool {
    if ucp {
        let cat = |names: &[&str]| names.iter().any(|m| in_categories(c, category_bit(m)));
        match class {
            Posix::Alnum => return is_letter_or_number(c),
            Posix::Alpha => return cat(&["Lu", "Ll", "Lt", "Lm", "Lo"]),
            Posix::Blank => return is_hspace(c),
            Posix::Cntrl => return cat(&["Cc"]),
            Posix::Digit => return cat(&["Nd"]),
            Posix::Lower => return cat(&["Ll"]),
            Posix::Space => return is_unicode_space(c),
            Posix::Upper => return cat(&["Lu"]),
            Posix::Word => return is_unicode_word(c),
            Posix::Graph => return ucp_graph(c),
            Posix::Print => return ucp_graph(c) || cat(&["Zs"]),
            Posix::Punct => {
                return cat(&["Pc", "Pd", "Pe", "Pf", "Pi", "Po", "Ps"])
                    || (u32::from(c) < 0x100 && cat(&["Sc", "Sk", "Sm", "So"]));
            }
            Posix::Ascii | Posix::Xdigit => {}
        }
    }
    match class {
        Posix::Alnum => c.is_ascii_alphanumeric(),
        Posix::Alpha => c.is_ascii_alphabetic(),
        Posix::Ascii => c.is_ascii(),
        Posix::Blank => matches!(c, ' ' | '\t'),
        Posix::Cntrl => c.is_ascii_control(),
        Posix::Digit => c.is_ascii_digit(),
        Posix::Graph => c.is_ascii_graphic(),
        Posix::Lower => c.is_ascii_lowercase(),
        Posix::Print => c.is_ascii_graphic() || c == ' ',
        Posix::Punct => c.is_ascii_punctuation(),
        Posix::Space => matches!(c, '\t'..='\r' | ' '),
        Posix::Upper => c.is_ascii_uppercase(),
        Posix::Word => is_ascii_word(c),
        Posix::Xdigit => c.is_ascii_hexdigit(),
    }
}

/// `[:graph:]` with UCP: everything but separators and other characters, except the format
/// characters that have a glyph.
fn ucp_graph(c: char) -> bool {
    let cat = |names: &[&str]| names.iter().any(|m| in_categories(c, category_bit(m)));
    let invisible_format = matches!(c, '\u{61C}' | '\u{180E}' | '\u{2066}'..='\u{2069}');
    !cat(&["Zs", "Zl", "Zp", "Cc", "Cf", "Cn", "Co", "Cs"]) || (cat(&["Cf"]) && !invisible_format)
}

fn prop(p: Prop, c: char) -> bool {
    match p {
        Prop::Any => true,
        Prop::Categories(mask) => in_categories(c, mask),
        Prop::Script(s) => script(c) == s,
        Prop::ScriptExtension(s) => has_script_extension(c, s),
        Prop::Xan => is_letter_or_number(c),
        Prop::Xps | Prop::Xsp => is_unicode_space(c),
        Prop::Xwd => is_unicode_word(c),
        Prop::Xuc => matches!(c, '$' | '@' | '`') || u32::from(c) >= 0xA0,
        Prop::Binary(p) => has_binary(c, p),
        Prop::Bidi(b) => bidi_class(c) == b,
    }
}
