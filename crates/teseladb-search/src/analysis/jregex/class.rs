//! Character classes of the `java.util.regex.Pattern` syntax: ranges, unions, intersections,
//! negation, the predefined classes (`\d`, `\s`, `\w`, `\h`, `\v`) and the `\p{…}` properties
//! (POSIX, `java.lang.Character`, general categories, binary properties, scripts and blocks), as
//! the `Pattern` Javadoc of Java 21 describes them.

use crate::analysis::props::{self, Binary, CATEGORIES};

/// A named set of characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Prop {
    /// Every character.
    All,
    /// One general category, as an index of [`CATEGORIES`].
    Category(usize),
    /// A group of categories: the categories whose name starts with this letter.
    Major(char),
    /// `LC`: `Lu`, `Ll` and `Lt`.
    CasedLetter,
    /// `LD`: letters and decimal digits.
    LetterOrDigit,
    /// A binary property of the character database.
    Binary(Binary),
    /// `Assigned`: any category but `Cn`.
    Assigned,
    /// A script, as an index of [`props::script_names`].
    Script(usize),
    /// A block, as an index of [`props::block_names`].
    Block(usize),
    /// `[\x00-\x7F]`.
    Ascii,
    /// `[\x00-\xFF]` (`L1`).
    Latin1,
    /// A class with an ASCII and a Unicode definition (`UNICODE_CHARACTER_CLASS`).
    Posix(Posix, bool),
    /// A class of `java.lang.Character`.
    Java(JavaClass),
    /// `\h`: horizontal white space.
    Horizontal,
    /// `\v`: vertical white space.
    Vertical,
}

/// The POSIX classes, and `\d`, `\s` and `\w`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Posix {
    /// `Lower`.
    Lower,
    /// `Upper`.
    Upper,
    /// `Alpha`.
    Alpha,
    /// `Digit` and `\d`.
    Digit,
    /// `Alnum`.
    Alnum,
    /// `Punct`.
    Punct,
    /// `Graph`.
    Graph,
    /// `Print`.
    Print,
    /// `Blank`.
    Blank,
    /// `Cntrl`.
    Cntrl,
    /// `XDigit`.
    XDigit,
    /// `Space` and `\s`.
    Space,
    /// `\w`.
    Word,
}

/// The classes of `java.lang.Character` (`\p{javaLowerCase}` …).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JavaClass {
    /// `isLowerCase`.
    LowerCase,
    /// `isUpperCase`.
    UpperCase,
    /// `isTitleCase`.
    TitleCase,
    /// `isWhitespace`.
    Whitespace,
    /// `isMirrored`.
    Mirrored,
    /// `isAlphabetic`.
    Alphabetic,
    /// `isDefined`.
    Defined,
    /// `isDigit`.
    Digit,
    /// `isIdentifierIgnorable`.
    IdentifierIgnorable,
    /// `isIdeographic`.
    Ideographic,
    /// `isISOControl`.
    IsoControl,
    /// `isJavaIdentifierPart`.
    JavaIdentifierPart,
    /// `isJavaIdentifierStart`.
    JavaIdentifierStart,
    /// `isLetter`.
    Letter,
    /// `isLetterOrDigit`.
    LetterOrDigit,
    /// `isSpaceChar`.
    SpaceChar,
    /// `isUnicodeIdentifierPart`.
    UnicodeIdentifierPart,
    /// `isUnicodeIdentifierStart`.
    UnicodeIdentifierStart,
}

const JAVA_CLASSES: [(&str, JavaClass); 18] = [
    ("javaLowerCase", JavaClass::LowerCase),
    ("javaUpperCase", JavaClass::UpperCase),
    ("javaTitleCase", JavaClass::TitleCase),
    ("javaWhitespace", JavaClass::Whitespace),
    ("javaMirrored", JavaClass::Mirrored),
    ("javaAlphabetic", JavaClass::Alphabetic),
    ("javaDefined", JavaClass::Defined),
    ("javaDigit", JavaClass::Digit),
    ("javaIdentifierIgnorable", JavaClass::IdentifierIgnorable),
    ("javaIdeographic", JavaClass::Ideographic),
    ("javaISOControl", JavaClass::IsoControl),
    ("javaJavaIdentifierPart", JavaClass::JavaIdentifierPart),
    ("javaJavaIdentifierStart", JavaClass::JavaIdentifierStart),
    ("javaLetter", JavaClass::Letter),
    ("javaLetterOrDigit", JavaClass::LetterOrDigit),
    ("javaSpaceChar", JavaClass::SpaceChar),
    ("javaUnicodeIdentifierPart", JavaClass::UnicodeIdentifierPart),
    ("javaUnicodeIdentifierStart", JavaClass::UnicodeIdentifierStart),
];

const POSIX_CLASSES: [(&str, Posix); 12] = [
    ("Lower", Posix::Lower),
    ("Upper", Posix::Upper),
    ("Alpha", Posix::Alpha),
    ("Digit", Posix::Digit),
    ("Alnum", Posix::Alnum),
    ("Punct", Posix::Punct),
    ("Graph", Posix::Graph),
    ("Print", Posix::Print),
    ("Blank", Posix::Blank),
    ("Cntrl", Posix::Cntrl),
    ("XDigit", Posix::XDigit),
    ("Space", Posix::Space),
];

fn gc(c: char) -> &'static str {
    CATEGORIES[props::category(c)]
}

fn is_major(c: char, major: char) -> bool {
    gc(c).starts_with(major)
}

const ASCII_PUNCT: &str = "!\"#$%&'()*+,-./:;<=>?@[\\]^_`{|}~";

fn posix(c: char, class: Posix, unicode: bool) -> bool {
    if !unicode {
        return match class {
            Posix::Lower => c.is_ascii_lowercase(),
            Posix::Upper => c.is_ascii_uppercase(),
            Posix::Alpha => c.is_ascii_alphabetic(),
            Posix::Digit => c.is_ascii_digit(),
            Posix::Alnum => c.is_ascii_alphanumeric(),
            Posix::Punct => ASCII_PUNCT.contains(c),
            Posix::Graph => c.is_ascii_alphanumeric() || ASCII_PUNCT.contains(c),
            Posix::Print => c == ' ' || c.is_ascii_alphanumeric() || ASCII_PUNCT.contains(c),
            Posix::Blank => c == ' ' || c == '\t',
            Posix::Cntrl => c.is_ascii_control(),
            Posix::XDigit => c.is_ascii_hexdigit(),
            Posix::Space => matches!(c, ' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r'),
            Posix::Word => c.is_ascii_alphanumeric() || c == '_',
        };
    }
    let white = props::has(c, Binary::WhiteSpace);
    match class {
        Posix::Lower => props::has(c, Binary::Lowercase),
        Posix::Upper => props::has(c, Binary::Uppercase),
        Posix::Alpha => props::has(c, Binary::Alphabetic),
        Posix::Digit => gc(c) == "Nd",
        Posix::Alnum => props::has(c, Binary::Alphabetic) || gc(c) == "Nd",
        Posix::Punct => is_major(c, 'P'),
        Posix::Graph => !white && !matches!(gc(c), "Cc" | "Cs" | "Cn"),
        Posix::Print => {
            (posix(c, Posix::Graph, true) || posix(c, Posix::Blank, true)) && gc(c) != "Cc"
        }
        Posix::Blank => {
            white && !matches!(gc(c), "Zl" | "Zp") && !matches!(c, '\n'..='\r' | '\u{85}')
        }
        Posix::Cntrl => gc(c) == "Cc",
        Posix::XDigit => gc(c) == "Nd" || props::has(c, Binary::HexDigit),
        Posix::Space => white,
        Posix::Word => {
            props::has(c, Binary::Alphabetic)
                || matches!(gc(c), "Mn" | "Me" | "Mc" | "Nd" | "Pc")
                || props::has(c, Binary::JoinControl)
        }
    }
}

fn identifier_ignorable(c: char) -> bool {
    matches!(c, '\0'..='\x08' | '\x0E'..='\x1B' | '\x7F'..='\u{9F}') || gc(c) == "Cf"
}

fn java(c: char, class: JavaClass) -> bool {
    let g = gc(c);
    let letter = is_major(c, 'L');
    match class {
        JavaClass::LowerCase => props::has(c, Binary::Lowercase),
        JavaClass::UpperCase => props::has(c, Binary::Uppercase),
        JavaClass::TitleCase => g == "Lt",
        JavaClass::Whitespace => {
            (matches!(g, "Zs" | "Zl" | "Zp") && !matches!(c, '\u{A0}' | '\u{2007}' | '\u{202F}'))
                || matches!(c, '\t'..='\r' | '\x1C'..='\x1F')
        }
        JavaClass::Mirrored => props::has(c, Binary::BidiMirrored),
        JavaClass::Alphabetic => props::has(c, Binary::Alphabetic),
        JavaClass::Defined => g != "Cn",
        JavaClass::Digit => g == "Nd",
        JavaClass::IdentifierIgnorable => identifier_ignorable(c),
        JavaClass::Ideographic => props::has(c, Binary::Ideographic),
        JavaClass::IsoControl => matches!(c, '\0'..='\x1F' | '\x7F'..='\u{9F}'),
        JavaClass::JavaIdentifierStart => letter || matches!(g, "Nl" | "Sc" | "Pc"),
        JavaClass::JavaIdentifierPart => {
            letter
                || matches!(g, "Sc" | "Pc" | "Nd" | "Nl" | "Mc" | "Mn")
                || identifier_ignorable(c)
        }
        JavaClass::Letter => letter,
        JavaClass::LetterOrDigit => letter || g == "Nd",
        JavaClass::SpaceChar => matches!(g, "Zs" | "Zl" | "Zp"),
        JavaClass::UnicodeIdentifierStart => {
            letter || g == "Nl" || props::has(c, Binary::OtherIdStart) || c == '\u{2E2F}'
        }
        JavaClass::UnicodeIdentifierPart => {
            java(c, JavaClass::UnicodeIdentifierStart)
                || matches!(g, "Pc" | "Nd" | "Mc" | "Mn")
                || identifier_ignorable(c)
                || props::has(c, Binary::OtherIdContinue)
        }
    }
}

impl Prop {
    /// Whether `c` is in the set.
    pub fn contains(self, c: char) -> bool {
        match self {
            Prop::All => true,
            Prop::Category(i) => props::category(c) == i,
            Prop::Major(m) => is_major(c, m),
            Prop::CasedLetter => matches!(gc(c), "Lu" | "Ll" | "Lt"),
            Prop::LetterOrDigit => is_major(c, 'L') || gc(c) == "Nd",
            Prop::Binary(b) => props::has(c, b),
            Prop::Assigned => props::category(c) != 0,
            Prop::Script(s) => props::script(c) == Some(s),
            Prop::Block(b) => props::block(c) == Some(b),
            Prop::Ascii => c.is_ascii(),
            Prop::Latin1 => u32::from(c) <= 0xFF,
            Prop::Posix(p, unicode) => posix(c, p, unicode),
            Prop::Java(j) => java(c, j),
            Prop::Horizontal => matches!(
                c,
                ' ' | '\t' | '\u{A0}' | '\u{1680}' | '\u{180E}' | '\u{2000}'
                    ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
            ),
            Prop::Vertical => {
                matches!(c, '\n'..='\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
            }
        }
    }
}

/// Loose matching of names (UAX #44, LM3): case, spaces, `_` and `-` ignored.
fn loose(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, ' ' | '_' | '-')).flat_map(char::to_lowercase).collect()
}

fn find_alias(names: &[String], wanted: &str) -> Option<usize> {
    let wanted = loose(wanted);
    names.iter().position(|line| line.split(';').any(|alias| loose(alias) == wanted))
}

fn category(name: &str) -> Option<Prop> {
    if let Some(i) = CATEGORIES.iter().position(|&c| c == name) {
        return Some(Prop::Category(i));
    }
    match name {
        "L" | "M" | "N" | "P" | "S" | "Z" | "C" => name.chars().next().map(Prop::Major),
        "LC" => Some(Prop::CasedLetter),
        "LD" => Some(Prop::LetterOrDigit),
        "L1" => Some(Prop::Latin1),
        "all" => Some(Prop::All),
        _ => None,
    }
}

/// The binary properties `\p{IsXxx}` accepts, by loose name.
fn binary(name: &str) -> Option<Prop> {
    let wanted = loose(name);
    let named = |n: &str| loose(n) == wanted;
    if let Some(&b) = Binary::ALL.iter().find(|b| named(b.ucd_name())) {
        let java_visible =
            !matches!(b, Binary::OtherIdStart | Binary::OtherIdContinue | Binary::BidiMirrored);
        return java_visible.then_some(Prop::Binary(b));
    }
    match wanted.as_str() {
        "letter" => Some(Prop::Major('L')),
        "titlecase" => Some(Prop::Category(3)),
        "punctuation" => Some(Prop::Major('P')),
        "control" => Some(Prop::Category(26)),
        "digit" => Some(Prop::Category(9)),
        "assigned" => Some(Prop::Assigned),
        _ => None,
    }
}

/// Resolves the name inside `\p{…}`. `unicode` is the `UNICODE_CHARACTER_CLASS` flag.
pub fn property(name: &str, unicode: bool) -> Option<Prop> {
    if let Some((key, value)) = name.split_once('=') {
        return match key {
            "script" | "sc" => find_alias(props::script_names(), value).map(Prop::Script),
            "block" | "blk" => find_alias(props::block_names(), value).map(Prop::Block),
            "general_category" | "gc" => category(value),
            _ => None,
        };
    }
    if let Some(rest) = name.strip_prefix("In") {
        return find_alias(props::block_names(), rest).map(Prop::Block);
    }
    if let Some(rest) = name.strip_prefix("Is") {
        return category(rest)
            .or_else(|| binary(rest))
            .or_else(|| find_alias(props::script_names(), rest).map(Prop::Script));
    }
    if let Some(&(_, p)) = POSIX_CLASSES.iter().find(|(n, _)| *n == name) {
        return Some(Prop::Posix(p, unicode));
    }
    if name == "ASCII" {
        return Some(Prop::Ascii);
    }
    if let Some(&(_, j)) = JAVA_CLASSES.iter().find(|(n, _)| *n == name) {
        return Some(Prop::Java(j));
    }
    category(name)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn properties_resolve() {
        assert_eq!(property("Lu", false), Some(Prop::Category(1)));
        assert_eq!(property("IsL", false), Some(Prop::Major('L')));
        assert_eq!(property("gc=Nd", false), Some(Prop::Category(9)));
        assert!(matches!(property("IsLatin", false), Some(Prop::Script(_))));
        assert!(matches!(property("sc=Latn", false), Some(Prop::Script(_))));
        assert!(matches!(property("InGreek", false), Some(Prop::Block(_))));
        assert!(matches!(property("blk=Greek and Coptic", false), Some(Prop::Block(_))));
        assert_eq!(property("IsAlphabetic", false), Some(Prop::Binary(Binary::Alphabetic)));
        assert_eq!(property("IsWhiteSpace", false), Some(Prop::Binary(Binary::WhiteSpace)));
        assert_eq!(property("IsBidi_Mirrored", false), None);
        assert_eq!(property("Lower", true), Some(Prop::Posix(Posix::Lower, true)));
        assert_eq!(property("javaLowerCase", false), Some(Prop::Java(JavaClass::LowerCase)));
        assert_eq!(property("ASCII", false), Some(Prop::Ascii));
        assert_eq!(property("nope", false), None);
        assert_eq!(property("foo=bar", false), None);
    }

    #[test]
    fn ascii_and_unicode_posix() {
        assert!(Prop::Posix(Posix::Word, false).contains('_'));
        assert!(!Prop::Posix(Posix::Word, false).contains('é'));
        assert!(Prop::Posix(Posix::Word, true).contains('é'));
        assert!(Prop::Posix(Posix::Digit, true).contains('٣'));
        assert!(Prop::Posix(Posix::Blank, true).contains('\u{3000}'));
        assert!(!Prop::Posix(Posix::Blank, true).contains('\n'));
        assert!(Prop::Posix(Posix::Print, true).contains(' '));
        assert!(!Prop::Posix(Posix::Graph, true).contains(' '));
        assert!(Prop::Java(JavaClass::Whitespace).contains('\x1C'));
        assert!(!Prop::Java(JavaClass::Whitespace).contains('\u{A0}'));
        assert!(Prop::Java(JavaClass::JavaIdentifierStart).contains('$'));
        assert!(Prop::Horizontal.contains('\u{3000}') && Prop::Vertical.contains('\u{2029}'));
    }

    /// Every class over characters that take each side of its conditions: `(class, inside,
    /// outside)` with the characters Java's definitions put in and out.
    #[test]
    fn every_class_over_probes() {
        let posix_cases: [(Posix, &str, &str, &str, &str); 13] = [
            (Posix::Lower, "a", "Aé", "aªé", "A1"),
            (Posix::Upper, "A", "aÉ", "AÉ", "a1"),
            (Posix::Alpha, "aZ", "1é", "aé中", "1_"),
            (Posix::Digit, "7", "a٣", "7٣", "a"),
            (Posix::Alnum, "a7", "_é", "é٣", "_"),
            (Posix::Punct, "!~_", "a", "!«_", "a+"),
            (Posix::Graph, "a!", " \u{7F}", "aé!", " \u{378}\u{7}"),
            (Posix::Print, " a!", "\n", " a\u{3000}", "\n\u{7}"),
            (Posix::Blank, " \t", "\na", " \u{3000}", "\n\u{2028}\u{85}a"),
            (Posix::Cntrl, "\u{7}\u{7F}", "a", "\u{7}\u{85}", "a"),
            (Posix::XDigit, "fA9", "g", "f٣\u{FF21}", "g"),
            (Posix::Space, " \r", "a", " \u{3000}", "a"),
            (Posix::Word, "a_9", "-é", "é_٣\u{301}\u{200D}", "-"),
        ];
        let unescape = |s: &str| -> Vec<char> {
            let mut out = Vec::new();
            let mut rest = s;
            while !rest.is_empty() {
                if let Some(r) = rest.strip_prefix("\\u{") {
                    let end = r.find('}').unwrap();
                    out.push(char::from_u32(u32::from_str_radix(&r[..end], 16).unwrap()).unwrap());
                    rest = &r[end + 1..];
                } else if let Some(r) = rest.strip_prefix("\\n") {
                    out.push('\n');
                    rest = r;
                } else if let Some(r) = rest.strip_prefix("\\t") {
                    out.push('\t');
                    rest = r;
                } else if let Some(r) = rest.strip_prefix("\\r") {
                    out.push('\r');
                    rest = r;
                } else {
                    let c = rest.chars().next().unwrap();
                    out.push(c);
                    rest = &rest[c.len_utf8()..];
                }
            }
            out
        };
        for (class, ascii_in, ascii_out, uni_in, uni_out) in posix_cases {
            for c in unescape(ascii_in) {
                assert!(posix(c, class, false), "{class:?} ascii {c:?}");
            }
            for c in unescape(ascii_out) {
                assert!(!posix(c, class, false), "{class:?} ascii not {c:?}");
            }
            for c in unescape(uni_in) {
                assert!(posix(c, class, true), "{class:?} unicode {c:?}");
            }
            for c in unescape(uni_out) {
                assert!(!posix(c, class, true), "{class:?} unicode not {c:?}");
            }
        }
        let java_cases: [(JavaClass, &str, &str); 18] = [
            (JavaClass::LowerCase, "aª", "A"),
            (JavaClass::UpperCase, "AⅠ", "a"),
            (JavaClass::TitleCase, "ǅ", "A"),
            (JavaClass::Whitespace, " \u{2028}\u{1F}\t", "\u{A0}\u{2007}a"),
            (JavaClass::Mirrored, "(«", "a"),
            (JavaClass::Alphabetic, "aⅠ", "1"),
            (JavaClass::Defined, "a", "\u{378}"),
            (JavaClass::Digit, "٣", "a"),
            (JavaClass::IdentifierIgnorable, "\u{0}\u{AD}\u{7F}", "a\n"),
            (JavaClass::Ideographic, "中", "a"),
            (JavaClass::IsoControl, "\u{0}\u{9F}", "a"),
            (JavaClass::JavaIdentifierPart, "a$_1\u{301}\u{AD}Ⅰ", "-"),
            (JavaClass::JavaIdentifierStart, "a$_Ⅰ", "1"),
            (JavaClass::Letter, "a", "1"),
            (JavaClass::LetterOrDigit, "a1", "_"),
            (JavaClass::SpaceChar, "\u{A0}\u{2029}", "\t"),
            (JavaClass::UnicodeIdentifierPart, "a_1\u{301}\u{AD}\u{B7}", "-$"),
            (JavaClass::UnicodeIdentifierStart, "aⅠ\u{2E2F}\u{2118}", "1_"),
        ];
        for (class, inside, outside) in java_cases {
            for c in unescape(inside) {
                assert!(java(c, class), "{class:?} {c:?}");
            }
            for c in unescape(outside) {
                assert!(!java(c, class), "{class:?} not {c:?}");
            }
        }
        let props: [(Prop, char, char); 12] = [
            (Prop::All, 'a', 'a'),
            (Prop::Category(1), 'A', 'a'),
            (Prop::Major('N'), '1', 'a'),
            (Prop::CasedLetter, 'ǅ', '中'),
            (Prop::LetterOrDigit, '1', '_'),
            (Prop::Assigned, 'a', '\u{378}'),
            (Prop::Ascii, 'a', 'é'),
            (Prop::Latin1, 'é', 'ā'),
            (Prop::Horizontal, '\u{3000}', '\n'),
            (Prop::Vertical, '\u{85}', ' '),
            (Prop::Java(JavaClass::Letter), 'a', '1'),
            (Prop::Posix(Posix::Word, false), 'a', 'é'),
        ];
        for (p, inside, outside) in props {
            assert!(p.contains(inside), "{p:?} {inside:?}");
            assert!(p == Prop::All || !p.contains(outside), "{p:?} not {outside:?}");
        }
        let script = property("IsGreek", false).unwrap();
        assert!(script.contains('α') && !script.contains('a'));
        let block = property("InBasicLatin", false).unwrap();
        assert!(block.contains('a') && !block.contains('é'));
        let binary = property("IsEmoji", false).unwrap();
        assert!(binary.contains('😀') && !binary.contains('a'));
        for name in
            ["IsLetter", "IsTitlecase", "IsPunctuation", "IsControl", "IsDigit", "IsAssigned"]
        {
            assert!(property(name, false).is_some(), "{name}");
        }
        for name in ["LC", "LD", "L1", "all", "IsLC"] {
            assert!(property(name, false).is_some(), "{name}");
        }
        assert_eq!(property("block=Nope", false), None);
        assert_eq!(property("InNope", false), None);
    }
}
