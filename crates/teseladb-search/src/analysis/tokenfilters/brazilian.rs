//! The stemmer of the Brazilian Portuguese analyzer (`reflib.brazilian`), deduced from what
//! `refsearch` 1.70.6 gives (D-051): the tables below were found by asking `refsearch` for each ending
//! of the vocabulary after stems that put it inside or outside each region, and the steps and
//! regions were checked on the 40 000 words of the fixture and on 400 000 synthetic words.
//!
//! The word is lower-cased and its accents folded (`á â ã` → `a`, `é ê` → `e`, `í` → `i`,
//! `ó ô õ` → `o`, `ú ü` → `u`, `ç` → `c`, `ñ` → `n`); a word of fewer than 3 or more than
//! 29 letters, or with something other than letters, is not stemmed (and keeps its accents when
//! it is too short or too long). Then RV, R1 and R2 are marked and the steps run: the standard
//! suffixes, the verb suffixes when no standard suffix went, the residual vowels when neither
//! went, and the final `e` always. In each step the longest suffix whose region condition
//! holds is replaced; a suffix whose condition fails lets a shorter one be tried.

use crate::analysis::props;

/// The region a suffix must start in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Region {
    /// RV.
    Rv,
    /// R1.
    R1,
    /// R2.
    R2,
}

/// Suffixes that share a condition and a replacement.
struct Group {
    /// The region the suffix must start in.
    region: Region,
    /// Letters of the suffix allowed before the start of the region.
    offset: usize,
    /// Whether the condition is read on the word as it was before any step (its last letters
    /// must be the suffix) instead of on the word as it is now.
    original: bool,
    /// What replaces the suffix.
    to: &'static str,
    /// The suffixes.
    suffixes: &'static [&'static str],
}

const STANDARD: &[Group] = &[
    Group { region: Region::R1, offset: 0, original: false, to: "", suffixes: &["amente"] },
    Group {
        region: Region::R2,
        offset: 0,
        original: false,
        to: "",
        suffixes: &[
            "eza", "ica", "ico", "iva", "ivo", "osa", "oso", "acao", "ador", "avel", "ezas",
            "icas", "icos", "ismo", "ista", "ivas", "ivel", "ivos", "osas", "osos", "acoes",
            "adora", "idade", "ismos", "istas", "mente", "adoras", "adores", "amento", "idades",
            "imento", "amentos", "imentos",
        ],
    },
    Group {
        region: Region::R2,
        offset: 0,
        original: false,
        to: "ente",
        suffixes: &["encia", "encias"],
    },
    Group { region: Region::R2, offset: 0, original: false, to: "logias", suffixes: &["logias"] },
    Group { region: Region::Rv, offset: 0, original: false, to: "ira", suffixes: &["ira"] },
    Group {
        region: Region::Rv,
        offset: 1,
        original: false,
        to: "eir",
        suffixes: &["eira", "eiras"],
    },
];

const VERB: &[Group] = &[Group {
    region: Region::Rv,
    offset: 0,
    original: false,
    to: "",
    suffixes: &[
        "am", "ar", "as", "ei", "em", "er", "es", "eu", "ia", "ir", "is", "iu", "ou", "ada", "ado",
        "ais", "ara", "ear", "eis", "era", "iam", "ias", "ida", "ido", "adas", "ados", "amos",
        "ando", "aram", "arao", "aras", "arei", "arem", "ares", "aria", "asse", "aste", "avam",
        "avas", "emos", "endo", "eram", "erao", "eras", "erei", "erem", "eres", "eria", "esse",
        "este", "idas", "idos", "ieis", "imos", "indo", "iram", "irao", "iras", "irei", "irem",
        "ires", "iria", "isse", "iste", "ardes", "areis", "ariam", "arias", "armos", "assem",
        "asses", "astes", "aveis", "erdes", "ereis", "eriam", "erias", "ermos", "essem", "esses",
        "estes", "iamos", "irdes", "ireis", "iriam", "irias", "irmos", "issem", "isses", "istes",
        "aramos", "aremos", "arieis", "asseis", "avamos", "eramos", "eremos", "erieis", "esseis",
        "iramos", "iremos", "irieis", "isseis", "ariamos", "assemos", "eriamos", "essemos",
        "iriamos", "issemos",
    ],
}];

const RESIDUAL: &[Group] = &[Group {
    region: Region::Rv,
    offset: 0,
    original: true,
    to: "",
    suffixes: &["a", "i", "o", "os"],
}];

const FINAL_E: &[Group] = &[
    Group { region: Region::Rv, offset: 0, original: true, to: "", suffixes: &["e"] },
    Group { region: Region::Rv, offset: 0, original: true, to: "c", suffixes: &["cie"] },
    Group { region: Region::Rv, offset: 0, original: true, to: "g", suffixes: &["gue"] },
];
/// The longest suffix of the tables.
const LONGEST: usize = 7;

/// Whether `c` is one of the vowels of the regions.
fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u')
}

/// The start of the region after the first non-vowel that follows a vowel, looking from
/// `start`; the last letter never counts.
fn region_after(w: &[char], start: usize) -> usize {
    let last = w.len().saturating_sub(1);
    let mut j = start;
    while j < last && !vowel(w[j]) {
        j += 1;
    }
    if j >= last {
        return w.len();
    }
    j += 1;
    while j < last && vowel(w[j]) {
        j += 1;
    }
    if j >= last { w.len() } else { j + 1 }
}

/// The start of RV.
fn rv(w: &[char]) -> usize {
    let last = w.len().saturating_sub(1);
    if last > 0
        && !vowel(w[1])
        && let Some(j) = (2..last).find(|&j| vowel(w[j]))
    {
        return j + 1;
    }
    if last > 1
        && vowel(w[0])
        && vowel(w[1])
        && let Some(j) = (2..last).find(|&j| !vowel(w[j]))
    {
        return j + 1;
    }
    if last > 2 { 3 } else { w.len() }
}

/// The word before any step and where its regions start.
struct Marks {
    original: Vec<char>,
    rv: usize,
    r1: usize,
    r2: usize,
}

impl Marks {
    fn start(&self, region: Region) -> usize {
        match region {
            Region::Rv => self.rv,
            Region::R1 => self.r1,
            Region::R2 => self.r2,
        }
    }
}

/// Whether `w` ends with `suffix`.
fn ends(w: &[char], suffix: &str) -> bool {
    let n = suffix.chars().count();
    n <= w.len() && w[w.len() - n..].iter().copied().eq(suffix.chars())
}

/// Runs one step on `w`; returns whether it replaced a suffix.
fn step(w: &mut Vec<char>, groups: &[Group], marks: &Marks) -> bool {
    for k in (1..=LONGEST.min(w.len())).rev() {
        let tail: String = w[w.len() - k..].iter().collect();
        let Some(group) = groups.iter().find(|g| g.suffixes.contains(&tail.as_str())) else {
            continue;
        };
        let holds = if group.original {
            ends(&marks.original, &tail)
                && marks.original.len() - k + group.offset >= marks.start(group.region)
        } else {
            w.len() - k + group.offset >= marks.start(group.region)
        };
        if holds {
            w.truncate(w.len() - k);
            w.extend(group.to.chars());
            return true;
        }
    }
    false
}

/// Whether `c` is a letter of the Basic Multilingual Plane (general category `L`): a
/// supplementary character is two UTF-16 units, neither of them a letter.
fn is_letter(c: char) -> bool {
    u32::from(c) <= 0xFFFF && (1..=5).contains(&props::category(c))
}

/// The folded spelling of a lower-case letter.
fn fold(c: char) -> char {
    match c {
        'á' | 'â' | 'ã' => 'a',
        'é' | 'ê' => 'e',
        'í' => 'i',
        'ó' | 'ô' | 'õ' => 'o',
        'ú' | 'ü' => 'u',
        'ç' => 'c',
        'ñ' => 'n',
        other => other,
    }
}

/// The Brazilian stem of a lower-case token.
pub(super) fn brazilian_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().map(fold).collect();
    // The lengths count UTF-16 units, as Java strings do.
    let units = text.encode_utf16().count();
    if !(3..30).contains(&units) {
        return text.to_string();
    }
    if !w.iter().all(|&c| is_letter(c)) {
        return w.into_iter().collect();
    }
    let r1 = region_after(&w, 0);
    let r2 = if r1 < w.len() { region_after(&w, r1) } else { w.len() };
    let marks = Marks { rv: rv(&w), r1, r2, original: w.clone() };
    let mut changed = step(&mut w, STANDARD, &marks);
    if !changed {
        changed = step(&mut w, VERB, &marks);
    }
    if !changed {
        step(&mut w, RESIDUAL, &marks);
    }
    step(&mut w, FINAL_E, &marks);
    w.into_iter().collect()
}
