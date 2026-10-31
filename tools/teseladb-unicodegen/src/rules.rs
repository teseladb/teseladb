//! ICU collation tailoring rules, read as ICU 57.1's `CollationRuleParser` reads them (UTS #35,
//! part 5, «Collation Tailorings»).
//!
//! At the top level: a rule chain `&reset rel rel…`, a setting `[…]`, a comment `#…`, `@` (French
//! secondary order) or `!` (ignored). A reset is a string or an anchor such as `[last regular]`,
//! with an optional `[before n]`; a relation is `<` `<<` `<<<` `<<<<` `=` (or the old `;` and `,`)
//! with `prefix|string/extension`, and the starred forms `<*abc`, `<*a-d` place one character
//! each. Every ASCII punctuation character is syntax: text containing one is quoted (`'…'`, `''`
//! for an apostrophe) or escaped (`\c` is the character `c`). Spaces are `Pattern_White_Space`.
//! The `\uXXXX` escapes of ICU's data files are already resolved by the resource bundle reader.

/// How much a relation differs from the previous element, or which level a `[before n]` resets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// `<`
    Primary = 1,
    /// `<<`
    Secondary = 2,
    /// `<<<`
    Tertiary = 3,
    /// `<<<<`
    Quaternary = 4,
    /// `=`
    Identical = 5,
}

/// A special reset position (`&[last regular]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Anchor {
    /// `[first tertiary ignorable]`
    FirstTertiaryIgnorable,
    /// `[last tertiary ignorable]`
    LastTertiaryIgnorable,
    /// `[first secondary ignorable]`
    FirstSecondaryIgnorable,
    /// `[last secondary ignorable]`
    LastSecondaryIgnorable,
    /// `[first primary ignorable]`
    FirstPrimaryIgnorable,
    /// `[last primary ignorable]`
    LastPrimaryIgnorable,
    /// `[first variable]`
    FirstVariable,
    /// `[last variable]`, also `[variable top]`
    LastVariable,
    /// `[first regular]`
    FirstRegular,
    /// `[last regular]`, also `[top]`
    LastRegular,
    /// `[first implicit]`
    FirstImplicit,
    /// `[last implicit]`
    LastImplicit,
    /// `[first trailing]`
    FirstTrailing,
    /// `[last trailing]`
    LastTrailing,
}

const ANCHORS: [(&str, Anchor); 16] = [
    ("first tertiary ignorable", Anchor::FirstTertiaryIgnorable),
    ("last tertiary ignorable", Anchor::LastTertiaryIgnorable),
    ("first secondary ignorable", Anchor::FirstSecondaryIgnorable),
    ("last secondary ignorable", Anchor::LastSecondaryIgnorable),
    ("first primary ignorable", Anchor::FirstPrimaryIgnorable),
    ("last primary ignorable", Anchor::LastPrimaryIgnorable),
    ("first variable", Anchor::FirstVariable),
    ("last variable", Anchor::LastVariable),
    ("first regular", Anchor::FirstRegular),
    ("last regular", Anchor::LastRegular),
    ("first implicit", Anchor::FirstImplicit),
    ("last implicit", Anchor::LastImplicit),
    ("first trailing", Anchor::FirstTrailing),
    ("last trailing", Anchor::LastTrailing),
    ("top", Anchor::LastRegular),
    ("variable top", Anchor::LastVariable),
];

/// Where a reset points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// A string.
    Text(String),
    /// A special position.
    Anchor(Anchor),
}

/// `[caseFirst …]`
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseFirst {
    /// `off`
    Off,
    /// `lower`
    Lower,
    /// `upper`
    Upper,
}

/// `[maxVariable …]`: the last group that `shifted` makes ignorable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxVariable {
    /// `space`
    Space,
    /// `punct`
    Punct,
    /// `symbol`
    Symbol,
    /// `currency`
    Currency,
}

/// A setting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    /// `[strength 1|2|3|4|I]`
    Strength(Level),
    /// `[alternate shifted]` is true, `[alternate non-ignorable]` false.
    AlternateShifted(bool),
    /// `[maxVariable …]`
    MaxVariable(MaxVariable),
    /// `[caseFirst …]`
    CaseFirst(CaseFirst),
    /// `[caseLevel on|off]`
    CaseLevel(bool),
    /// `[normalization on|off]`
    Normalization(bool),
    /// `[numericOrdering on|off]`
    NumericOrdering(bool),
    /// `[backwards 2]` or `@`: secondary weights compared from the end.
    BackwardsSecondary,
    /// `[reorder …]`: script codes, `space`, `punct`, `symbol`, `currency`, `digit`, `others`.
    Reorder(Vec<String>),
    /// `[import tag]`: the rules of another collation, ICU locale and type.
    Import {
        /// ICU locale (`root` for `und`).
        locale: String,
        /// Collation type (`standard` if the tag has none).
        collation: String,
    },
    /// `[optimize [set]]`
    Optimize(Vec<(char, char)>),
    /// `[suppressContractions [set]]`
    SuppressContractions(Vec<(char, char)>),
}

/// One rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A setting.
    Setting(Setting),
    /// `&`: where the next relations attach.
    Reset {
        /// The level of `[before n]`.
        before: Option<Level>,
        /// The string or anchor.
        target: Target,
    },
    /// A relation to the previous element.
    Relation {
        /// How much it differs from the previous element.
        level: Level,
        /// Context before it (`prefix|text`), empty if none.
        prefix: String,
        /// The string it places.
        text: String,
        /// What it expands to after (`text/extension`), empty if none.
        extension: String,
    },
}

/// Rules that do not read, with the offset of the character.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleError(pub String);

/// The rules of a tailoring.
pub fn parse(text: &str) -> Result<Vec<Rule>, RuleError> {
    let mut p = Parser { c: text.chars().collect(), pos: 0, rules: Vec::new() };
    loop {
        p.pos = p.skip_space(p.pos);
        let Some(&c) = p.c.get(p.pos) else { return Ok(p.rules) };
        match c {
            '&' => p.rule_chain()?,
            '[' => p.setting()?,
            '#' => p.pos = p.skip_comment(p.pos + 1),
            '@' => {
                p.rules.push(Rule::Setting(Setting::BackwardsSecondary));
                p.pos += 1;
            }
            '!' => p.pos += 1,
            _ => return Err(p.err("a reset, a setting or a comment expected")),
        }
    }
}

/// Unicode's `Pattern_White_Space`, what ICU skips: not `char::is_whitespace`, which takes in
/// U+00A0 and U+3000 (text to ICU) and leaves out the direction marks.
fn space(c: char) -> bool {
    matches!(c, '\t'..='\r' | ' ' | '\u{85}' | '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}')
}

/// ICU's syntax characters: all ASCII punctuation.
fn syntax(c: char) -> bool {
    matches!(c, '!'..='/' | ':'..='@' | '['..='`' | '{'..='~')
}

/// The end of a comment line.
fn line_end(c: char) -> bool {
    matches!(c, '\n' | '\u{C}' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Whether `c` is one of the characters ICU refuses in a rule string.
fn noncharacter(c: char) -> bool {
    ('\u{FFFD}'..='\u{FFFF}').contains(&c)
}

/// A `[import tag]` tag as ICU locale and collation type: `und` is the root, `-u-co-…` the type
/// (BCP 47 spells some types short).
fn import_target(tag: &str) -> Option<(String, String)> {
    let (base, ty) = tag.split_once("-u-co-").unwrap_or((tag, "standard"));
    let well_formed = |s: &str| {
        s.split('-').all(|w| !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric()))
    };
    if base.contains('-') || !well_formed(base) || !well_formed(ty) {
        return None;
    }
    let ty = match ty {
        "phonebk" => "phonebook",
        "trad" => "traditional",
        "dict" => "dictionary",
        "gb2312" => "gb2312han",
        other => other,
    };
    let locale = if base == "und" { "root" } else { base };
    Some((locale.to_ascii_lowercase(), ty.to_ascii_lowercase()))
}

fn on_off(v: &str) -> Option<bool> {
    match v {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

struct Parser {
    c: Vec<char>,
    pos: usize,
    rules: Vec<Rule>,
}

impl Parser {
    fn err(&self, what: &str) -> RuleError {
        RuleError(format!("at {}: {what}", self.pos))
    }

    fn skip_space(&self, mut i: usize) -> usize {
        while self.c.get(i).is_some_and(|c| space(*c)) {
            i += 1;
        }
        i
    }

    fn skip_comment(&self, mut i: usize) -> usize {
        while self.c.get(i).is_some_and(|c| !line_end(*c)) {
            i += 1;
        }
        i
    }

    fn at(&self, i: usize, s: &str) -> bool {
        let n = s.chars().count();
        self.c.get(i..i + n).is_some_and(|w| w.iter().copied().eq(s.chars()))
    }

    /// `&reset` and its relations.
    fn rule_chain(&mut self) -> Result<(), RuleError> {
        let reset = self.reset()?;
        let mut first = true;
        loop {
            let Some((level, starred, i)) = self.relation_operator() else {
                if self.c.get(self.pos) == Some(&'#') {
                    self.pos = self.skip_comment(self.pos + 1);
                    continue;
                }
                if first {
                    return Err(self.err("a reset not followed by a relation"));
                }
                return Ok(());
            };
            if let Some(before) = reset {
                if first && level != before {
                    return Err(self.err("a reset-before level differs from its first relation"));
                }
                if !first && level < before {
                    return Err(self.err("a reset-before chain followed by a stronger relation"));
                }
            }
            if starred {
                self.starred(level, i)?;
            } else {
                self.relation_strings(level, i)?;
            }
            first = false;
        }
    }

    fn reset(&mut self) -> Result<Option<Level>, RuleError> {
        let mut i = self.skip_space(self.pos + 1);
        let mut before = None;
        if self.at(i, "[before") && self.c.get(i + 7).is_some_and(|c| space(*c)) {
            let j = self.skip_space(i + 8);
            let level = match self.c.get(j) {
                Some('1') => Some(Level::Primary),
                Some('2') => Some(Level::Secondary),
                Some('3') => Some(Level::Tertiary),
                _ => None,
            };
            if level.is_some() && self.c.get(j + 1) == Some(&']') {
                before = level;
                i = self.skip_space(j + 2);
            }
        }
        self.pos = i;
        let target = match self.c.get(i) {
            None => return Err(self.err("a reset without a position")),
            Some('[') => Target::Anchor(self.anchor()?),
            Some(_) => Target::Text(self.tailoring_string()?),
        };
        self.rules.push(Rule::Reset { before, target });
        Ok(before)
    }

    /// `[words]` naming a special position.
    fn anchor(&mut self) -> Result<Anchor, RuleError> {
        let (words, j) = self.words(self.pos + 1);
        let found = ANCHORS.iter().find(|(name, _)| *name == words);
        match (found, self.c.get(j)) {
            (Some((_, a)), Some(']')) => {
                self.pos = j + 1;
                Ok(*a)
            }
            _ => Err(self.err("not a valid special reset position")),
        }
    }

    /// Words separated by single spaces, up to a syntax character other than `-` and `_`.
    fn words(&self, i: usize) -> (String, usize) {
        let mut out = String::new();
        let mut i = self.skip_space(i);
        while let Some(&c) = self.c.get(i) {
            if syntax(c) && c != '-' && c != '_' {
                break;
            }
            if space(c) {
                out.push(' ');
                i = self.skip_space(i + 1);
            } else {
                out.push(c);
                i += 1;
            }
        }
        (out.trim_end_matches(' ').to_owned(), i)
    }

    /// The level of the relation operator at the current position, whether it is starred, and
    /// where its strings start.
    fn relation_operator(&mut self) -> Option<(Level, bool, usize)> {
        self.pos = self.skip_space(self.pos);
        let i = self.pos;
        let (level, mut j) = match self.c.get(i)? {
            '<' => {
                let n = (i..i + 4).take_while(|k| self.c.get(*k) == Some(&'<')).count();
                let level =
                    [Level::Primary, Level::Secondary, Level::Tertiary, Level::Quaternary][n - 1];
                (level, i + n)
            }
            ';' => (Level::Secondary, i + 1),
            ',' => (Level::Tertiary, i + 1),
            '=' => (Level::Identical, i + 1),
            _ => return None,
        };
        let starred = matches!(self.c[i], '<' | '=') && self.c.get(j) == Some(&'*');
        if starred {
            j += 1;
        }
        Some((level, starred, j))
    }

    fn relation_strings(&mut self, level: Level, i: usize) -> Result<(), RuleError> {
        self.pos = i;
        let mut text = self.tailoring_string()?;
        let mut prefix = String::new();
        if self.c.get(self.pos) == Some(&'|') {
            prefix = text;
            self.pos += 1;
            text = self.tailoring_string()?;
        }
        let mut extension = String::new();
        if self.c.get(self.pos) == Some(&'/') {
            self.pos += 1;
            extension = self.tailoring_string()?;
        }
        self.rules.push(Rule::Relation { level, prefix, text, extension });
        Ok(())
    }

    /// `<*abc`, `<*a-d`: one relation per character.
    fn starred(&mut self, level: Level, i: usize) -> Result<(), RuleError> {
        self.pos = self.skip_space(i);
        let mut chars: Vec<char> = self.string()?.chars().collect();
        if chars.is_empty() {
            return Err(self.err("a starred relation without characters"));
        }
        while let Some(&prev) = chars.last() {
            for c in chars {
                self.push_one(level, c);
            }
            if self.c.get(self.pos) != Some(&'-') {
                return Ok(());
            }
            self.pos += 1;
            let end: Vec<char> = self.string()?.chars().collect();
            let Some(&last) = end.first() else {
                return Err(self.err("a range without an end in a starred relation"));
            };
            if last < prev {
                return Err(self.err("a range that goes down in a starred relation"));
            }
            for cp in u32::from(prev) + 1..=u32::from(last) {
                match char::from_u32(cp) {
                    Some(c) if !noncharacter(c) => self.push_one(level, c),
                    _ => return Err(self.err("a range through surrogates or U+FFFD..U+FFFF")),
                }
            }
            chars = end[1..].to_vec();
        }
        if self.c.get(self.pos) == Some(&'-') {
            return Err(self.err("a range without a start in a starred relation"));
        }
        Ok(())
    }

    fn push_one(&mut self, level: Level, c: char) {
        let text = c.to_string();
        self.rules.push(Rule::Relation {
            level,
            prefix: String::new(),
            text,
            extension: String::new(),
        });
    }

    /// A non-empty string between spaces.
    fn tailoring_string(&mut self) -> Result<String, RuleError> {
        self.pos = self.skip_space(self.pos);
        let s = self.string()?;
        if s.is_empty() {
            return Err(self.err("a missing relation string"));
        }
        self.pos = self.skip_space(self.pos);
        Ok(s)
    }

    /// Text up to a syntax character or a space; quotes and `\c` take them in.
    fn string(&mut self) -> Result<String, RuleError> {
        let mut out = String::new();
        while let Some(&c) = self.c.get(self.pos) {
            if c == '\'' {
                self.quoted(&mut out)?;
            } else if c == '\\' {
                let Some(&e) = self.c.get(self.pos + 1) else {
                    return Err(self.err("a backslash at the end of the rules"));
                };
                out.push(e);
                self.pos += 2;
            } else if syntax(c) || space(c) {
                break;
            } else {
                out.push(c);
                self.pos += 1;
            }
        }
        if out.chars().any(noncharacter) {
            return Err(self.err("a string with U+FFFD, U+FFFE or U+FFFF"));
        }
        Ok(out)
    }

    /// `''` or `'…'` at the current position.
    fn quoted(&mut self, out: &mut String) -> Result<(), RuleError> {
        self.pos += 1;
        if self.c.get(self.pos) == Some(&'\'') {
            out.push('\'');
            self.pos += 1;
            return Ok(());
        }
        loop {
            let Some(&q) = self.c.get(self.pos) else {
                return Err(self.err("quoted text without its closing apostrophe"));
            };
            self.pos += 1;
            if q == '\'' {
                if self.c.get(self.pos) != Some(&'\'') {
                    return Ok(());
                }
                self.pos += 1;
            }
            out.push(q);
        }
    }

    /// `[words]` or `[words [set]]`.
    fn setting(&mut self) -> Result<(), RuleError> {
        let (words, j) = self.words(self.pos + 1);
        if words.is_empty() {
            return Err(self.err("a setting expected after '['"));
        }
        match self.c.get(j) {
            Some(']') => {
                let setting = self.simple_setting(&words)?;
                self.rules.extend(setting.map(Rule::Setting));
                self.pos = j + 1;
                Ok(())
            }
            Some('[') => {
                self.pos = j;
                let set = self.unicode_set()?;
                let setting = match words.as_str() {
                    "optimize" => Setting::Optimize(set),
                    "suppressContractions" => Setting::SuppressContractions(set),
                    _ => return Err(self.err("not a valid setting")),
                };
                self.pos = self.skip_space(self.pos);
                if self.c.get(self.pos) != Some(&']') {
                    return Err(self.err("a ']' expected after the set"));
                }
                self.pos += 1;
                self.rules.push(Rule::Setting(setting));
                Ok(())
            }
            _ => Err(self.err("not a valid setting")),
        }
    }

    /// The setting `words` names; `None` for one that changes nothing (`[hiraganaQ off]`).
    fn simple_setting(&self, words: &str) -> Result<Option<Setting>, RuleError> {
        if words == "reorder" || words.starts_with("reorder ") {
            let codes = words.split(' ').skip(1).map(str::to_owned).collect();
            return Ok(Some(Setting::Reorder(codes)));
        }
        if words == "backwards 2" {
            return Ok(Some(Setting::BackwardsSecondary));
        }
        let invalid = || self.err("not a valid setting");
        let (name, value) = words.rsplit_once(' ').ok_or_else(invalid)?;
        let setting = match (name, value) {
            ("strength", "1") => Setting::Strength(Level::Primary),
            ("strength", "2") => Setting::Strength(Level::Secondary),
            ("strength", "3") => Setting::Strength(Level::Tertiary),
            ("strength", "4") => Setting::Strength(Level::Quaternary),
            ("strength", "I") => Setting::Strength(Level::Identical),
            ("alternate", "shifted") => Setting::AlternateShifted(true),
            ("alternate", "non-ignorable") => Setting::AlternateShifted(false),
            ("maxVariable", "space") => Setting::MaxVariable(MaxVariable::Space),
            ("maxVariable", "punct") => Setting::MaxVariable(MaxVariable::Punct),
            ("maxVariable", "symbol") => Setting::MaxVariable(MaxVariable::Symbol),
            ("maxVariable", "currency") => Setting::MaxVariable(MaxVariable::Currency),
            ("caseFirst", "off") => Setting::CaseFirst(CaseFirst::Off),
            ("caseFirst", "lower") => Setting::CaseFirst(CaseFirst::Lower),
            ("caseFirst", "upper") => Setting::CaseFirst(CaseFirst::Upper),
            ("caseLevel", v) => Setting::CaseLevel(on_off(v).ok_or_else(invalid)?),
            ("normalization", v) => Setting::Normalization(on_off(v).ok_or_else(invalid)?),
            ("numericOrdering", v) => Setting::NumericOrdering(on_off(v).ok_or_else(invalid)?),
            ("hiraganaQ", "off") => return Ok(None),
            ("hiraganaQ", "on") => return Err(self.err("[hiraganaQ on] is not supported")),
            ("import", tag) => {
                let (locale, collation) = import_target(tag)
                    .ok_or_else(|| self.err("a language tag expected in [import]"))?;
                Setting::Import { locale, collation }
            }
            _ => return Err(invalid()),
        };
        Ok(Some(setting))
    }

    /// A set of characters and ranges, `[…]`, the forms ICU's data uses.
    fn unicode_set(&mut self) -> Result<Vec<(char, char)>, RuleError> {
        self.pos += 1;
        let mut chars = Vec::new();
        let mut ranges = Vec::new();
        loop {
            self.pos = self.skip_space(self.pos);
            match self.c.get(self.pos).copied() {
                None => return Err(self.err("a set without its ']'")),
                Some(']') => break,
                Some('-') if !chars.is_empty() => {
                    self.pos = self.skip_space(self.pos + 1);
                    let hi = self.set_char()?;
                    let lo = chars.pop().expect("a character before '-'");
                    if hi < lo {
                        return Err(self.err("a set range that goes down"));
                    }
                    ranges.push((lo, hi));
                }
                Some('[' | '{' | '$' | '^' | '&' | ':' | '-') => {
                    return Err(self.err("set syntax the collation data does not use"));
                }
                Some(_) => {
                    let c = self.set_char()?;
                    chars.push(c);
                }
            }
        }
        self.pos += 1;
        ranges.extend(chars.into_iter().map(|c| (c, c)));
        ranges.sort_unstable();
        Ok(ranges)
    }

    /// One character of a set, `\uXXXX` or `\c` escaped.
    fn set_char(&mut self) -> Result<char, RuleError> {
        let c = self.c.get(self.pos).copied().ok_or_else(|| self.err("a set without its ']'"))?;
        if c != '\\' {
            self.pos += 1;
            return Ok(c);
        }
        let digits = match self.c.get(self.pos + 1) {
            Some('u') => 4,
            Some('U') => 8,
            Some(&e) => {
                self.pos += 2;
                return Ok(e);
            }
            None => return Err(self.err("a backslash at the end of the rules")),
        };
        let hex: String = self
            .c
            .get(self.pos + 2..self.pos + 2 + digits)
            .ok_or_else(|| self.err("a short escape"))?
            .iter()
            .collect();
        self.pos += 2 + digits;
        u32::from_str_radix(&hex, 16)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(|| self.err("an escape that is not a character"))
    }
}
