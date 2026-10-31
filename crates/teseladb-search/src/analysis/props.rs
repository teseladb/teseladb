//! Unicode character properties of the regular expressions in custom analyzers (FR-007, FR-019):
//! general categories, binary properties, scripts, blocks, simple uppercase mappings and character
//! names, from the Unicode 15.1.0 character database that
//! `scripts/unicode-search.sha256` pins.
//!
//! The tables live in `data/unicode-props.bin`, embedded in the binary. `tests/unicode_props.rs`
//! rebuilds them from the pinned files and fails if they differ; `TESELADB_WRITE_TABLES=1` writes
//! them.
//!
//! Layout: the magic `TSLPRP01`, then sections. A record section is a `u32` count followed by that
//! many records of little-endian `u32`s; a byte section is a `u32` length followed by that many
//! bytes. In order: general-category ranges `(first, last, category)`, binary-property ranges
//! `(first, last, bits)`, script ranges `(first, last, script + 1)`, block ranges `(first, last,
//! block + 1)`, uppercase mappings `(code point, uppercase)`, the script names and the block names (byte sections of `\n`-terminated
//! lines, each line the `;`-separated aliases of one value), the name words (a byte section of
//! `\n`-terminated words, the most frequent first) and the names: a byte section where each named
//! code point is a LEB128 varint of its distance from the previous one, a varint word count and a
//! varint index of each word.

use std::sync::OnceLock;

/// The general categories, in the order of their codes in the tables (`Cn` first, so that a code
/// point outside every range is unassigned).
pub const CATEGORIES: [&str; 30] = [
    "Cn", "Lu", "Ll", "Lt", "Lm", "Lo", "Mn", "Mc", "Me", "Nd", "Nl", "No", "Pc", "Pd", "Ps", "Pe",
    "Pi", "Pf", "Po", "Sm", "Sc", "Sk", "So", "Zs", "Zl", "Zp", "Cc", "Cf", "Cs", "Co",
];

/// A binary property of the tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binary {
    /// `Alphabetic`.
    Alphabetic,
    /// `Lowercase`.
    Lowercase,
    /// `Uppercase`.
    Uppercase,
    /// `White_Space`.
    WhiteSpace,
    /// `Ideographic`.
    Ideographic,
    /// `Hex_Digit`.
    HexDigit,
    /// `Join_Control`.
    JoinControl,
    /// `Noncharacter_Code_Point`.
    NoncharacterCodePoint,
    /// `Other_ID_Start`.
    OtherIdStart,
    /// `Other_ID_Continue`.
    OtherIdContinue,
    /// `Emoji`.
    Emoji,
    /// `Emoji_Presentation`.
    EmojiPresentation,
    /// `Emoji_Modifier`.
    EmojiModifier,
    /// `Emoji_Modifier_Base`.
    EmojiModifierBase,
    /// `Emoji_Component`.
    EmojiComponent,
    /// `Extended_Pictographic`.
    ExtendedPictographic,
    /// `Bidi_Mirrored`.
    BidiMirrored,
}

impl Binary {
    /// Every property, in the order of their bits.
    pub const ALL: [Binary; 17] = [
        Binary::Alphabetic,
        Binary::Lowercase,
        Binary::Uppercase,
        Binary::WhiteSpace,
        Binary::Ideographic,
        Binary::HexDigit,
        Binary::JoinControl,
        Binary::NoncharacterCodePoint,
        Binary::OtherIdStart,
        Binary::OtherIdContinue,
        Binary::Emoji,
        Binary::EmojiPresentation,
        Binary::EmojiModifier,
        Binary::EmojiModifierBase,
        Binary::EmojiComponent,
        Binary::ExtendedPictographic,
        Binary::BidiMirrored,
    ];

    /// The name the character database uses.
    pub fn ucd_name(self) -> &'static str {
        match self {
            Binary::Alphabetic => "Alphabetic",
            Binary::Lowercase => "Lowercase",
            Binary::Uppercase => "Uppercase",
            Binary::WhiteSpace => "White_Space",
            Binary::Ideographic => "Ideographic",
            Binary::HexDigit => "Hex_Digit",
            Binary::JoinControl => "Join_Control",
            Binary::NoncharacterCodePoint => "Noncharacter_Code_Point",
            Binary::OtherIdStart => "Other_ID_Start",
            Binary::OtherIdContinue => "Other_ID_Continue",
            Binary::Emoji => "Emoji",
            Binary::EmojiPresentation => "Emoji_Presentation",
            Binary::EmojiModifier => "Emoji_Modifier",
            Binary::EmojiModifierBase => "Emoji_Modifier_Base",
            Binary::EmojiComponent => "Emoji_Component",
            Binary::ExtendedPictographic => "Extended_Pictographic",
            Binary::BidiMirrored => "Bidi_Mirrored",
        }
    }

    fn bit(self) -> u32 {
        1 << Binary::ALL.iter().position(|&b| b == self).unwrap_or(0)
    }
}

const MAGIC: &[u8; 8] = b"TSLPRP01";
const DATA: &[u8] = include_bytes!("../../data/unicode-props.bin");

#[derive(Debug, PartialEq, Eq)]
struct Tables {
    categories: Vec<[u32; 3]>,
    binary: Vec<[u32; 3]>,
    scripts: Vec<[u32; 3]>,
    blocks: Vec<[u32; 3]>,
    uppercase: Vec<[u32; 2]>,
    script_names: Vec<String>,
    block_names: Vec<String>,
    words: Vec<String>,
    names: Vec<u8>,
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn word(&mut self) -> Option<u32> {
        let w = u32::from_le_bytes(self.bytes.get(self.at..self.at + 4)?.try_into().ok()?);
        self.at += 4;
        Some(w)
    }

    fn records<const N: usize>(&mut self) -> Option<Vec<[u32; N]>> {
        let count = usize::try_from(self.word()?).ok()?;
        let mut out = Vec::with_capacity(count.min(self.bytes.len() / (4 * N)));
        for _ in 0..count {
            let mut record = [0_u32; N];
            for slot in &mut record {
                *slot = self.word()?;
            }
            out.push(record);
        }
        Some(out)
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let len = usize::try_from(self.word()?).ok()?;
        let out = self.bytes.get(self.at..self.at + len)?.to_vec();
        self.at += len;
        Some(out)
    }

    fn lines(&mut self) -> Option<Vec<String>> {
        let text = String::from_utf8(self.bytes()?).ok()?;
        Some(text.lines().map(str::to_string).collect())
    }
}

fn parse(bytes: &[u8]) -> Option<Tables> {
    if bytes.get(..8)? != MAGIC {
        return None;
    }
    let mut r = Reader { bytes, at: 8 };
    let tables = Tables {
        categories: r.records()?,
        binary: r.records()?,
        scripts: r.records()?,
        blocks: r.records()?,
        uppercase: r.records()?,
        script_names: r.lines()?,
        block_names: r.lines()?,
        words: r.lines()?,
        names: r.bytes()?,
    };
    (r.at == bytes.len()).then_some(tables)
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| parse(DATA).expect("data/unicode-props.bin is checked by its test"))
}

fn in_ranges(ranges: &[[u32; 3]], c: char) -> u32 {
    let c = u32::from(c);
    let i = ranges.partition_point(|r| r[1] < c);
    ranges.get(i).filter(|r| r[0] <= c).map_or(0, |r| r[2])
}

/// The general category of `c`, as an index of [`CATEGORIES`].
pub fn category(c: char) -> usize {
    in_ranges(&tables().categories, c) as usize
}

/// Whether `c` has the binary property `p`.
pub fn has(c: char, p: Binary) -> bool {
    in_ranges(&tables().binary, c) & p.bit() != 0
}

/// The script of `c`, as an index of [`script_names`], or `None` for `Unknown`.
pub fn script(c: char) -> Option<usize> {
    (in_ranges(&tables().scripts, c) as usize).checked_sub(1)
}

/// The block of `c`, as an index of [`block_names`], or `None` for `No_Block`.
pub fn block(c: char) -> Option<usize> {
    (in_ranges(&tables().blocks, c) as usize).checked_sub(1)
}

/// The aliases of each script, the long name first.
pub fn script_names() -> &'static [String] {
    &tables().script_names
}

/// The aliases of each block, the long name first.
pub fn block_names() -> &'static [String] {
    &tables().block_names
}

/// The simple uppercase mapping of `c`.
pub fn uppercase(c: char) -> char {
    let t = &tables().uppercase;
    t.binary_search_by_key(&u32::from(c), |r| r[0])
        .ok()
        .and_then(|i| char::from_u32(t[i][1]))
        .unwrap_or(c)
}

fn varint(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let mut value = 0_u32;
    for shift in (0..35).step_by(7) {
        let b = *bytes.get(*at)?;
        *at += 1;
        value |= u32::from(b & 0x7F).checked_shl(shift)?;
        if b & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Decodes the names section into `(code point, name)` in code point order.
fn decode_names(t: &Tables) -> Option<Vec<(u32, String)>> {
    let mut out = Vec::new();
    let (mut at, mut cp) = (0, 0_u32);
    while at < t.names.len() {
        cp = cp.checked_add(varint(&t.names, &mut at)?)?;
        let count = varint(&t.names, &mut at)?;
        let mut name = String::new();
        for i in 0..count {
            if i > 0 {
                name.push(' ');
            }
            name.push_str(t.words.get(varint(&t.names, &mut at)? as usize)?);
        }
        out.push((cp, name));
    }
    Some(out)
}

fn names() -> &'static [(u32, String)] {
    static NAMES: OnceLock<Vec<(u32, String)>> = OnceLock::new();
    NAMES.get_or_init(|| {
        decode_names(tables()).expect("data/unicode-props.bin is checked by its test")
    })
}

/// The name the character database gives `c`, if it has one in `UnicodeData.txt`.
pub fn name(c: char) -> Option<&'static str> {
    let all = names();
    let i = all.binary_search_by_key(&u32::from(c), |(cp, _)| *cp).ok()?;
    Some(&all[i].1)
}

/// The code point named `wanted` in `UnicodeData.txt`. Names are compared without case.
pub fn by_name(wanted: &str) -> Option<char> {
    static INDEX: OnceLock<Vec<(&'static str, u32)>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let mut all: Vec<(&'static str, u32)> =
            names().iter().map(|(cp, n)| (n.as_str(), *cp)).collect();
        all.sort_unstable();
        all
    });
    let wanted = wanted.to_ascii_uppercase();
    let i = index.binary_search_by(|(n, _)| (*n).cmp(wanted.as_str())).ok()?;
    char::from_u32(index[i].1)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn damaged_tables_are_rejected() {
        assert_eq!(parse(b"TSLPRP00"), None);
        assert_eq!(parse(b"TSL"), None);
        assert_eq!(parse(&DATA[..DATA.len() - 1]), None);
        let mut longer = DATA.to_vec();
        longer.push(0);
        assert_eq!(parse(&longer), None);
    }

    #[test]
    fn lookups() {
        assert_eq!(CATEGORIES[category('A')], "Lu");
        assert_eq!(CATEGORIES[category('\u{378}')], "Cn");
        assert_eq!(CATEGORIES[category('中')], "Lo");
        assert!(has('ª', Binary::Lowercase) && !has('a', Binary::Uppercase));
        assert!(has('(', Binary::BidiMirrored) && has('😀', Binary::ExtendedPictographic));
        let latin = script('a').unwrap();
        assert!(script_names()[latin].starts_with("Latin;"));
        assert_eq!(script('\u{378}'), None);
        assert!(block_names()[block('α').unwrap()].starts_with("Greek_And_Coptic"));
        assert_eq!(block('\u{2FE0}'), None);
        assert_eq!(uppercase('ß'), 'ß');
        assert_eq!(uppercase('ä'), 'Ä');
        assert_eq!(name('A'), Some("LATIN CAPITAL LETTER A"));
        let mut damaged = Tables { names: vec![0x80], ..parse(DATA).unwrap() };
        assert_eq!(decode_names(&damaged), None);
        damaged.names = vec![1, 1, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01];
        assert_eq!(decode_names(&damaged), None);
        damaged.names = vec![1, 1, 0x7F, 0x7F];
        assert_eq!(decode_names(&damaged), None);
        assert_eq!(name('\u{378}'), None);
        assert_eq!(by_name("latin small letter a"), Some('a'));
        assert_eq!(by_name("HYPHEN-MINUS"), Some('-'));
        assert_eq!(by_name("no such name"), None);
        let names: Vec<&str> = Binary::ALL.iter().map(|b| b.ucd_name()).collect();
        assert_eq!(names.len(), 17);
    }
}
