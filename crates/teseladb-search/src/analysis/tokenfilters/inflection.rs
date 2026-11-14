//! The stemmers of the Czech, Bulgarian, Latvian and Indonesian analyzers. Each one follows its
//! published description and was measured equal to the reference search server 1.70.6 on the vocabulary of its
//! language (recorded data, `analyzers/vocab/`): the Czech and Bulgarian light stemmers of
//! L. Dolamic and J. Savoy, the Latvian one of K. Kreslins, and the Indonesian one of F. Tala.

use super::light::{ends, replace_end};

/// The case endings of the Czech stemmer by the letters the word must exceed, the longest first.
const CZECH_CASES: &[(usize, &[&str])] = &[
    (7, &["atech"]),
    (6, &["ětem", "etem", "atům"]),
    (
        5,
        &[
            "ech", "ich", "ích", "ého", "ěmi", "emi", "ému", "ěte", "ete", "ěti", "eti", "ího",
            "iho", "ími", "ímu", "imu", "ách", "ata", "aty", "ých", "ama", "ami", "ové", "ovi",
            "ými",
        ],
    ),
    (4, &["em", "es", "ém", "ím", "ům", "at", "ám", "os", "us", "ým", "mi", "ou"]),
    (3, &["a", "e", "i", "o", "u", "ů", "y", "á", "é", "í", "ý", "ě"]),
];

/// The Czech stem: one case ending, then a possessive `ov`, `in` or `ův`, then the stem's last
/// consonant is normalized (`čt` → `ck`, `št` → `sk`, `c`/`č` → `k`, `z`/`ž` → `h`) or the `e`
/// or `ů` before it goes or becomes `o`.
pub(super) fn czech_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    let case = CZECH_CASES.iter().find_map(|&(more_than, list)| {
        let end = list.iter().find(|e| w.len() > more_than && ends(&w, e))?;
        Some(end.chars().count())
    });
    w.truncate(w.len() - case.unwrap_or(0));
    if w.len() > 5 && ["ov", "in", "ův"].iter().any(|e| ends(&w, e)) {
        w.truncate(w.len() - 2);
    }
    let n = w.len();
    if n == 0 {
        return String::new();
    }
    if ends(&w, "čt") {
        replace_end(&mut w, 2, "ck");
    } else if ends(&w, "št") {
        replace_end(&mut w, 2, "sk");
    } else if matches!(w[n - 1], 'c' | 'č') {
        w[n - 1] = 'k';
    } else if matches!(w[n - 1], 'z' | 'ž') {
        w[n - 1] = 'h';
    } else if n > 1 && w[n - 2] == 'e' {
        w.remove(n - 2);
    } else if n > 2 && w[n - 2] == 'ů' {
        w[n - 2] = 'o';
    }
    w.into_iter().collect()
}

/// The Bulgarian article endings by the letters the word must exceed.
const BULGARIAN_ARTICLES: &[(usize, &[&str])] =
    &[(6, &["ият"]), (5, &["ът", "то", "те", "та", "ия"]), (4, &["ят"])];

/// The Bulgarian stem: `ища` goes from a word of six letters or more; otherwise an article, then a
/// plural, then a final `я` and one of `а`, `о`, `е` go, `ен` becomes `н` and an `ъ` before the
/// last letter goes. Words under four letters stay.
pub(super) fn bulgarian_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    if w.len() < 4 {
        return text.to_string();
    }
    if w.len() > 5 && ends(&w, "ища") {
        w.truncate(w.len() - 3);
        return w.into_iter().collect();
    }
    let article = BULGARIAN_ARTICLES.iter().find_map(|&(more_than, list)| {
        let end = list.iter().find(|e| w.len() > more_than && ends(&w, e))?;
        Some(end.chars().count())
    });
    w.truncate(w.len() - article.unwrap_or(0));
    bulgarian_plural(&mut w);
    if w.len() > 3 {
        if ends(&w, "я") {
            w.pop();
        }
        if matches!(w.last(), Some('а' | 'о' | 'е')) {
            w.pop();
        }
    }
    if w.len() > 4 && ends(&w, "ен") {
        replace_end(&mut w, 2, "н");
    }
    let n = w.len();
    if n > 5 && w[n - 2] == 'ъ' {
        w.remove(n - 2);
    }
    w.into_iter().collect()
}

/// `w` without its Bulgarian plural ending.
fn bulgarian_plural(w: &mut Vec<char>) {
    let n = w.len();
    if n > 6 && (ends(w, "овци") || ends(w, "ове")) {
        w.truncate(n - 3);
    } else if n > 6 && ends(w, "еве") {
        replace_end(w, 3, "й");
    } else if n > 5 && ends(w, "ища") {
        w.truncate(n - 3);
    } else if n > 5 && ends(w, "та") {
        w.truncate(n - 2);
    } else if n > 5 && ends(w, "ци") {
        replace_end(w, 2, "к");
    } else if n > 5 && ends(w, "зи") {
        replace_end(w, 2, "г");
    } else if n > 5 && w[n - 3] == 'е' && w[n - 1] == 'и' {
        w[n - 3] = 'я';
        w.pop();
    } else if n > 4 && ends(w, "си") {
        replace_end(w, 2, "х");
    } else if n > 4 && ends(w, "и") {
        w.pop();
    }
}

/// The Latvian endings in the order they are tried: the ending, the vowels the word must have
/// more than, and whether its removal undoes a palatalization of the stem.
const LATVIAN_ENDINGS: &[(&str, usize, bool)] = &[
    ("ajiem", 3, false),
    ("ajai", 3, false),
    ("ajam", 2, false),
    ("ajām", 2, false),
    ("ajos", 2, false),
    ("ajās", 2, false),
    ("iem", 2, true),
    ("ajā", 2, false),
    ("ais", 2, false),
    ("ai", 2, false),
    ("ei", 2, false),
    ("ām", 1, false),
    ("am", 1, false),
    ("ēm", 1, false),
    ("īm", 1, false),
    ("im", 1, false),
    ("um", 1, false),
    ("us", 1, true),
    ("as", 1, false),
    ("ās", 1, false),
    ("es", 1, false),
    ("os", 1, true),
    ("ij", 1, false),
    ("īs", 1, false),
    ("ēs", 1, false),
    ("is", 1, false),
    ("ie", 1, false),
    ("u", 1, true),
    ("a", 1, true),
    ("i", 1, true),
    ("e", 1, false),
    ("ā", 1, false),
    ("ē", 1, false),
    ("ī", 1, false),
    ("ū", 1, false),
    ("o", 1, false),
    ("s", 0, false),
    ("š", 0, false),
];

/// The Latvian stem: the first ending of [`LATVIAN_ENDINGS`] the word has, with three letters
/// left and enough vowels, goes.
pub(super) fn latvian_stem(text: &str) -> String {
    let mut w: Vec<char> = text.chars().collect();
    let vowels = w.iter().filter(|c| "aeiouāīēū".contains(**c)).count();
    for &(ending, more_than, palatal) in LATVIAN_ENDINGS {
        let n = ending.chars().count();
        if vowels > more_than && w.len() >= n + 3 && ends(&w, ending) {
            w.truncate(w.len() - n);
            if palatal {
                latvian_unpalatalize(&mut w, ending.starts_with('u'));
            }
            break;
        }
    }
    w.into_iter().collect()
}

/// The stem's consonants as they are before a palatalizing ending, `after_u` when the ending
/// removed starts with `u`.
fn latvian_unpalatalize(w: &mut Vec<char>, after_u: bool) {
    if after_u && ends(w, "kš") {
        replace_end(w, 1, "st");
        return;
    }
    if after_u && ends(w, "ņņ") {
        replace_end(w, 2, "nn");
        return;
    }
    if ["pj", "bj", "mj", "vj"].iter().any(|e| ends(w, e)) {
        w.pop();
        return;
    }
    let pairs =
        [("šņ", "sn"), ("žņ", "zn"), ("šļ", "sl"), ("žļ", "zl"), ("ļņ", "ln"), ("ļļ", "ll")];
    if let Some((_, to)) = pairs.iter().find(|(from, _)| ends(w, from)) {
        replace_end(w, 2, to);
        return;
    }
    if let Some(last) = w.last_mut() {
        *last = match *last {
            'č' => 'c',
            'ļ' => 'l',
            'ņ' => 'n',
            c => c,
        };
    }
}

/// The Indonesian prefixes removed so far, as flags.
const MENG: u8 = 1;
const PENG: u8 = 2;
const DI: u8 = 4;
const TER: u8 = 8;
const KE: u8 = 16;
const BER: u8 = 32;
const PE: u8 = 64;

/// The first-order Indonesian prefixes in the order they are tried: the prefix, its flag, and the
/// letter its last one becomes when a vowel follows it (the prefix then keeps that letter).
const INDONESIAN_FIRST: &[(&str, u8, Option<char>)] = &[
    ("meng", MENG, None),
    ("meny", MENG, Some('s')),
    ("men", MENG, None),
    ("mem", MENG, None),
    ("me", MENG, None),
    ("peng", PENG, None),
    ("peny", PENG, Some('s')),
    ("peny", PENG, None),
    ("pen", PENG, Some('t')),
    ("pen", PENG, None),
    ("pem", PENG, None),
    ("di", DI, None),
    ("ter", TER, None),
    ("ke", KE, None),
];

/// A word being stemmed by the Indonesian stemmer, with its vowel count and removed prefixes.
struct Indonesian {
    w: Vec<char>,
    syllables: usize,
    flags: u8,
}

impl Indonesian {
    fn starts(&self, prefix: &str) -> bool {
        self.w.iter().copied().take(prefix.chars().count()).eq(prefix.chars())
            && self.w.len() >= prefix.chars().count()
    }

    fn vowel_at(&self, i: usize) -> bool {
        self.w.get(i).is_some_and(|c| "aeiou".contains(*c))
    }

    fn cut_end(&mut self, n: usize) {
        self.w.truncate(self.w.len() - n);
        self.syllables -= 1;
    }

    fn cut_start(&mut self, n: usize, flag: u8) {
        self.w.drain(..n);
        self.syllables -= 1;
        self.flags |= flag;
    }

    /// A particle (`kah`, `lah`, `pun`), then a possessive (`ku`, `mu`, `nya`).
    fn inflections(&mut self) {
        if self.syllables > 2 && ["kah", "lah", "pun"].iter().any(|e| ends(&self.w, e)) {
            self.cut_end(3);
        }
        if self.syllables > 2 && (ends(&self.w, "ku") || ends(&self.w, "mu")) {
            self.cut_end(2);
        } else if self.syllables > 2 && ends(&self.w, "nya") {
            self.cut_end(3);
        }
    }

    fn first_prefix(&mut self) {
        for &(prefix, flag, before_vowel) in INDONESIAN_FIRST {
            let n = prefix.chars().count();
            if !self.starts(prefix) || (before_vowel.is_some() && !self.vowel_at(n)) {
                continue;
            }
            match before_vowel {
                Some(letter) => {
                    self.w[n - 1] = letter;
                    self.cut_start(n - 1, flag);
                }
                None => self.cut_start(n, flag),
            }
            return;
        }
    }

    fn second_prefix(&mut self) {
        let word: String = self.w.iter().collect();
        if self.starts("ber") || word == "belajar" {
            self.cut_start(3, BER);
        } else if self.starts("be")
            && self.w.len() > 4
            && !self.vowel_at(2)
            && self.w[3] == 'e'
            && self.w[4] == 'r'
        {
            self.cut_start(2, BER);
        } else if self.starts("per") || word == "pelajar" {
            self.cut_start(3, 0);
        } else if self.starts("pe") {
            self.cut_start(2, PE);
        }
    }

    fn suffix(&mut self) {
        let f = self.flags;
        if ends(&self.w, "kan") && f & (KE | PENG | PE) == 0 {
            self.cut_end(3);
        } else if ends(&self.w, "an") && f & (DI | MENG | TER) == 0 {
            self.cut_end(2);
        } else if ends(&self.w, "i") && !ends(&self.w, "si") && f & (BER | KE | PENG) == 0 {
            self.cut_end(1);
        }
    }
}

/// The Indonesian stem: inflectional particles and possessives, then the first-order prefix and,
/// when one went, the suffix and the second-order prefix; otherwise the second-order prefix and
/// the suffix. Each removal needs more than two vowels left.
pub(super) fn indonesian_stem(text: &str) -> String {
    let w: Vec<char> = text.chars().collect();
    let syllables = w.iter().filter(|c| "aeiou".contains(**c)).count();
    let mut s = Indonesian { w, syllables, flags: 0 };
    s.inflections();
    let before = s.w.len();
    if s.syllables > 2 {
        s.first_prefix();
    }
    if s.w.len() != before {
        let before = s.w.len();
        if s.syllables > 2 {
            s.suffix();
        }
        if s.w.len() != before && s.syllables > 2 {
            s.second_prefix();
        }
    } else {
        if s.syllables > 2 {
            s.second_prefix();
        }
        if s.syllables > 2 {
            s.suffix();
        }
    }
    s.w.into_iter().collect()
}
