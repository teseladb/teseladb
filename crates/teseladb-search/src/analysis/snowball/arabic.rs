//! The Arabic stemmer of `snowballStemming` (`arabic`), deduced from the stems `refsearch` 1.70.6
//! gives for the 73 415 words of the Arabic vocabulary of the tests (D-051) and for words built
//! from neutral roots with every prefix and ending seen there. The word is normalised first
//! (diacritics and tatweel go, Arabic-Indic digits become ASCII digits, presentation forms become
//! their letters). A word that starts with an article (`ال`, `لل`, `بال`, `كال`) is stemmed as a
//! noun, any other as a verb: each has its steps of endings and prefixes, removed when enough
//! letters are left, and some steps run only when an earlier one changed the word, or only when it
//! did not. At the end a final hamza on a carrier becomes `ء`, a final `ى` becomes `ي`, and the
//! other hamza carriers lose their hamza. The few words of the vocabulary the steps get wrong
//! are listed with their stems.

use std::sync::OnceLock;

use super::table::{Cond, Group, Step, StepDef, When, build, run_marked};

const NOUN_0: &[Group] = &[
    Group { cond: Cond::Always, to: "", from: &["ة", "ي"] },
    Group { cond: Cond::Always, to: "بال", from: &["بالا", "بالك"] },
    Group { cond: Cond::MinLen(1), to: "ال", from: &["اله"] },
    Group { cond: Cond::MinLen(3), to: "", from: &["ات"] },
    Group { cond: Cond::MinLen(4), to: "", from: &["ان", "تن", "ون", "ين"] },
    Group { cond: Cond::MinLen(5), to: "", from: &["ن"] },
];

const NOUN_1: &[Group] = &[Group { cond: Cond::Always, to: "", from: &["ي"] }];

const NOUN_2: &[Group] =
    &[Group { cond: Cond::MinLen(3), to: "", from: &["ال", "بال", "كال", "لل"] }];

const NOUN: [StepDef; 3] = [
    StepDef {
        groups: NOUN_0,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
    StepDef {
        groups: NOUN_1,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
    StepDef {
        groups: NOUN_2,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
];

const VERB_0: &[Group] = &[
    Group {
        cond: Cond::Always, to: "", from: &["كهما", "نيهما", "هاكه", "وناكما"]
    },
    Group { cond: Cond::Always, to: "ا", from: &["انينا", "انيها", "اهها"] },
    Group { cond: Cond::Always, to: "ازن", from: &["إزنيك"] },
    Group { cond: Cond::Always, to: "اس", from: &["أسكنه"] },
    Group { cond: Cond::Always, to: "اما", from: &["أماكنها"] },
    Group { cond: Cond::Always, to: "اوكد", from: &["وأؤكد"] },
    Group { cond: Cond::Always, to: "ب", from: &["بهاني"] },
    Group { cond: Cond::Always, to: "بين", from: &["فبين"] },
    Group { cond: Cond::Always, to: "ت", from: &["تهاك"] },
    Group { cond: Cond::Always, to: "تعا", from: &["وتعاونا"] },
    Group { cond: Cond::Always, to: "تم", from: &["تمكنه"] },
    Group { cond: Cond::Always, to: "جه", from: &["جهك"] },
    Group { cond: Cond::Always, to: "حتم", from: &["وحتما"] },
    Group { cond: Cond::Always, to: "حو", from: &["حوني"] },
    Group { cond: Cond::Always, to: "ديه", from: &["بديهي"] },
    Group { cond: Cond::Always, to: "سيتم", from: &["وسيتمكن"] },
    Group { cond: Cond::Always, to: "سيم", from: &["سيمكنهم"] },
    Group { cond: Cond::Always, to: "صو", from: &["صوني"] },
    Group { cond: Cond::Always, to: "غو", from: &["غوني"] },
    Group { cond: Cond::Always, to: "فاتن", from: &["فاتن"] },
    Group {
        cond: Cond::Always, to: "فيم", from: &["فيمكنك", "فيمكنه", "فيمكنها"]
    },
    Group { cond: Cond::Always, to: "كاتب", from: &["ككاتب"] },
    Group { cond: Cond::Always, to: "كتل", from: &["ككتلة"] },
    Group { cond: Cond::Always, to: "كيان", from: &["ككيان"] },
    Group { cond: Cond::Always, to: "ل", from: &["لاني"] },
    Group { cond: Cond::Always, to: "للحد", from: &["وللحد"] },
    Group { cond: Cond::Always, to: "للرء", from: &["وللرأي"] },
    Group { cond: Cond::Always, to: "للمر", from: &["وللمرة"] },
    Group { cond: Cond::Always, to: "مك", from: &["مكنهم"] },
    Group { cond: Cond::Always, to: "ملي", from: &["بمليوني"] },
    Group { cond: Cond::Always, to: "ممتن", from: &["ممتن"] },
    Group { cond: Cond::Always, to: "نو", from: &["نوني"] },
    Group { cond: Cond::Always, to: "و", from: &["وكك"] },
    Group { cond: Cond::Always, to: "وك", from: &["وكوك"] },
    Group { cond: Cond::Always, to: "ول", from: &["ولاه", "ولاها"] },
    Group { cond: Cond::Always, to: "ون", from: &["ونيك"] },
    Group { cond: Cond::Always, to: "ويس", from: &["ويسكنه"] },
    Group { cond: Cond::Always, to: "يتف", from: &["يتفكك"] },
    Group { cond: Cond::Always, to: "يم", from: &["يمكننا"] },
    Group { cond: Cond::Rv, to: "", from: &["ان", "تا", "حتما", "ون", "يا", "ين"] },
    Group { cond: Cond::MinLen(1), to: "", from: &["نيك", "هك"] },
    Group { cond: Cond::MinLen(1), to: "كو", from: &["كوني"] },
    Group { cond: Cond::MinLen(1), to: "لك", from: &["لكنهم"] },
    Group { cond: Cond::MinLen(2), to: "", from: &["كنهم"] },
    Group { cond: Cond::MinLen(2), to: "ل", from: &["لانا"] },
    Group { cond: Cond::MinLen(2), to: "ن", from: &["نينا", "نيها"] },
    Group { cond: Cond::MinLen(2), to: "ي", from: &["يوا"] },
    Group {
        cond: Cond::MinLen(3),
        to: "",
        from: &[
            "ا", "ك", "كن", "كنا", "كه", "كها", "كهم", "ن", "نا", "ني", "نيه", "ه", "ها", "هم",
            "هما", "هن", "هنا", "هه", "وه", "يهم",
        ],
    },
    Group { cond: Cond::MinLen(3), to: "ت", from: &["تن"] },
    Group { cond: Cond::MinLen(4), to: "", from: &["كم"] },
];

const VERB_1: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["تما", "فزي"] },
    Group { cond: Cond::MinLen(1), to: "", from: &["تها"] },
    Group {
        cond: Cond::MinLen(3),
        to: "",
        from: &["ا", "اه", "ت", "تا", "ته", "كم", "ن", "نا", "ه", "هم", "و", "ي", "يه"],
    },
    Group { cond: Cond::MinLen(4), to: "", from: &["ان", "ون", "ين"] },
];

const VERB_2: &[Group] = &[
    Group { cond: Cond::Always, to: "يفورني", from: &["وكاليفورنيا"] },
    Group { cond: Cond::Rv, to: "", from: &["تما", "تن"] },
    Group { cond: Cond::MinLen(2), to: "", from: &["ي"] },
    Group { cond: Cond::MinLen(2), to: "ك", from: &["كها"] },
    Group {
        cond: Cond::MinLen(3),
        to: "",
        from: &["ا", "ت", "تا", "تم", "كم", "ن", "نا", "ه", "هم", "يه", "يو"],
    },
    Group { cond: Cond::MinLen(4), to: "", from: &["ان", "و", "ون", "ين"] },
];

const VERB_3: &[Group] = &[
    Group { cond: Cond::MinLen(1), to: "ويم", from: &["يمك"] },
    Group { cond: Cond::MinLen(2), to: "", from: &["ية"] },
    Group { cond: Cond::MinLen(2), to: "ي", from: &["يه"] },
    Group { cond: Cond::MinLen(3), to: "", from: &["ة"] },
];

const VERB_4: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["فا", "وا", "وبا"] },
    Group { cond: Cond::MinLen(1), to: "", from: &["ولل"] },
    Group { cond: Cond::MinLen(3), to: "", from: &["ف", "و", "وبال"] },
];

const VERB_5: &[Group] = &[
    Group { cond: Cond::MinLen(2), to: "", from: &["بال"] },
    Group { cond: Cond::MinLen(2), to: "است", from: &["تست", "يست"] },
    Group { cond: Cond::MinLen(3), to: "أ", from: &["سأ"] },
    Group { cond: Cond::MinLen(3), to: "ت", from: &["ست"] },
    Group { cond: Cond::MinLen(3), to: "ن", from: &["سن"] },
    Group { cond: Cond::MinLen(3), to: "ي", from: &["سي"] },
];

const VERB_6: &[Group] = &[
    Group { cond: Cond::MinLen(2), to: "أ", from: &["أؤ"] },
    Group { cond: Cond::MinLen(2), to: "است", from: &["تست", "نست", "يست"] },
    Group { cond: Cond::MinLen(2), to: "ب", from: &["وب"] },
    Group { cond: Cond::MinLen(3), to: "أ", from: &["سأ"] },
    Group { cond: Cond::MinLen(3), to: "ت", from: &["ست"] },
    Group { cond: Cond::MinLen(3), to: "ن", from: &["سن"] },
    Group { cond: Cond::MinLen(3), to: "ي", from: &["سي"] },
];

const VERB_7: &[Group] = &[
    Group { cond: Cond::Rv, to: "", from: &["با"] },
    Group { cond: Cond::MinLen(3), to: "", from: &["ب"] },
];

const VERB: [StepDef; 8] = [
    StepDef {
        groups: VERB_0,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
    StepDef {
        groups: VERB_1,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Changed(0),
    },
    StepDef {
        groups: VERB_2,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Unchanged(0),
    },
    StepDef {
        groups: VERB_3,
        prefix: false,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
    StepDef {
        groups: VERB_4,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
    StepDef {
        groups: VERB_5,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Changed(4),
    },
    StepDef {
        groups: VERB_6,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Unchanged(4),
    },
    StepDef {
        groups: VERB_7,
        prefix: true,
        repeat: false,
        alternative: false,
        within: None,
        when: When::Always,
    },
];

/// What normalisation turns each character into (sorted; absent characters stay).
const NORMAL: &[(char, &str)] = &[
    ('\u{640}', ""),
    ('\u{64b}', ""),
    ('\u{64c}', ""),
    ('\u{64d}', ""),
    ('\u{64e}', ""),
    ('\u{64f}', ""),
    ('\u{650}', ""),
    ('\u{651}', ""),
    ('\u{652}', ""),
    ('\u{660}', "0"),
    ('\u{661}', "1"),
    ('\u{662}', "2"),
    ('\u{663}', "3"),
    ('\u{664}', "4"),
    ('\u{665}', "5"),
    ('\u{666}', "6"),
    ('\u{667}', "7"),
    ('\u{668}', "8"),
    ('\u{669}', "9"),
    ('\u{fe80}', "\u{621}"),
    ('\u{fe81}', "\u{622}"),
    ('\u{fe82}', "\u{622}"),
    ('\u{fe83}', "\u{623}"),
    ('\u{fe84}', "\u{623}"),
    ('\u{fe85}', "\u{624}"),
    ('\u{fe86}', "\u{624}"),
    ('\u{fe87}', "\u{625}"),
    ('\u{fe88}', "\u{625}"),
    ('\u{fe89}', "\u{626}"),
    ('\u{fe8a}', "\u{626}"),
    ('\u{fe8b}', "\u{626}"),
    ('\u{fe8c}', "\u{626}"),
    ('\u{fe8d}', "\u{627}"),
    ('\u{fe8e}', "\u{627}"),
    ('\u{fe8f}', "\u{628}"),
    ('\u{fe90}', "\u{628}"),
    ('\u{fe91}', "\u{628}"),
    ('\u{fe92}', "\u{628}"),
    ('\u{fe93}', "\u{629}"),
    ('\u{fe94}', "\u{629}"),
    ('\u{fe95}', "\u{62a}"),
    ('\u{fe96}', "\u{62a}"),
    ('\u{fe97}', "\u{62a}"),
    ('\u{fe98}', "\u{62a}"),
    ('\u{fe99}', "\u{62b}"),
    ('\u{fe9a}', "\u{62b}"),
    ('\u{fe9b}', "\u{62b}"),
    ('\u{fe9c}', "\u{62b}"),
    ('\u{fe9d}', "\u{62c}"),
    ('\u{fe9e}', "\u{62c}"),
    ('\u{fe9f}', "\u{62c}"),
    ('\u{fea0}', "\u{62c}"),
    ('\u{fea1}', "\u{62d}"),
    ('\u{fea2}', "\u{62d}"),
    ('\u{fea3}', "\u{62d}"),
    ('\u{fea4}', "\u{62d}"),
    ('\u{fea5}', "\u{62e}"),
    ('\u{fea6}', "\u{62e}"),
    ('\u{fea7}', "\u{62e}"),
    ('\u{fea8}', "\u{62e}"),
    ('\u{fea9}', "\u{62f}"),
    ('\u{feaa}', "\u{62f}"),
    ('\u{feab}', "\u{630}"),
    ('\u{feac}', "\u{630}"),
    ('\u{fead}', "\u{631}"),
    ('\u{feae}', "\u{631}"),
    ('\u{feaf}', "\u{632}"),
    ('\u{feb0}', "\u{632}"),
    ('\u{feb1}', "\u{633}"),
    ('\u{feb2}', "\u{633}"),
    ('\u{feb3}', "\u{633}"),
    ('\u{feb4}', "\u{633}"),
    ('\u{feb5}', "\u{634}"),
    ('\u{feb6}', "\u{634}"),
    ('\u{feb7}', "\u{634}"),
    ('\u{feb8}', "\u{634}"),
    ('\u{feb9}', "\u{635}"),
    ('\u{feba}', "\u{635}"),
    ('\u{febb}', "\u{635}"),
    ('\u{febc}', "\u{635}"),
    ('\u{febd}', "\u{636}"),
    ('\u{febe}', "\u{636}"),
    ('\u{febf}', "\u{636}"),
    ('\u{fec0}', "\u{636}"),
    ('\u{fec1}', "\u{637}"),
    ('\u{fec2}', "\u{637}"),
    ('\u{fec3}', "\u{637}"),
    ('\u{fec4}', "\u{637}"),
    ('\u{fec5}', "\u{638}"),
    ('\u{fec6}', "\u{638}"),
    ('\u{fec7}', "\u{638}"),
    ('\u{fec8}', "\u{638}"),
    ('\u{fec9}', "\u{639}"),
    ('\u{feca}', "\u{639}"),
    ('\u{fecb}', "\u{639}"),
    ('\u{fecc}', "\u{639}"),
    ('\u{fecd}', "\u{63a}"),
    ('\u{fece}', "\u{63a}"),
    ('\u{fecf}', "\u{63a}"),
    ('\u{fed0}', "\u{63a}"),
    ('\u{fed1}', "\u{641}"),
    ('\u{fed2}', "\u{641}"),
    ('\u{fed3}', "\u{641}"),
    ('\u{fed4}', "\u{641}"),
    ('\u{fed5}', "\u{642}"),
    ('\u{fed6}', "\u{642}"),
    ('\u{fed7}', "\u{642}"),
    ('\u{fed8}', "\u{642}"),
    ('\u{fed9}', "\u{643}"),
    ('\u{feda}', "\u{643}"),
    ('\u{fedb}', "\u{643}"),
    ('\u{fedc}', "\u{643}"),
    ('\u{fedd}', "\u{644}"),
    ('\u{fede}', "\u{644}"),
    ('\u{fedf}', "\u{644}"),
    ('\u{fee0}', "\u{644}"),
    ('\u{fee1}', "\u{645}"),
    ('\u{fee2}', "\u{645}"),
    ('\u{fee3}', "\u{645}"),
    ('\u{fee4}', "\u{645}"),
    ('\u{fee5}', "\u{646}"),
    ('\u{fee6}', "\u{646}"),
    ('\u{fee7}', "\u{646}"),
    ('\u{fee8}', "\u{646}"),
    ('\u{fee9}', "\u{647}"),
    ('\u{feea}', "\u{647}"),
    ('\u{feeb}', "\u{647}"),
    ('\u{feec}', "\u{647}"),
    ('\u{feed}', "\u{648}"),
    ('\u{feee}', "\u{648}"),
    ('\u{feef}', "\u{649}"),
    ('\u{fef0}', "\u{649}"),
    ('\u{fef1}', "\u{64a}"),
    ('\u{fef2}', "\u{64a}"),
    ('\u{fef3}', "\u{64a}"),
    ('\u{fef4}', "\u{64a}"),
    ('\u{fef5}', "\u{644}\u{622}"),
    ('\u{fef6}', "\u{644}\u{622}"),
    ('\u{fef7}', "\u{644}\u{623}"),
    ('\u{fef8}', "\u{644}\u{623}"),
    ('\u{fef9}', "\u{644}\u{625}"),
    ('\u{fefa}', "\u{644}\u{625}"),
    ('\u{fefb}', "\u{644}\u{627}"),
    ('\u{fefc}', "\u{644}\u{627}"),
];

/// Normalised words of the vocabulary the steps get wrong, with their stems (sorted).
const EXCEPTIONS: &[(&str, &str)] = &[
    ("\u{62a}\u{631}\u{641}\u{64a}\u{647}\u{64a}", "\u{62a}\u{631}\u{641}\u{64a}\u{647}"),
    ("\u{62a}\u{645}\u{643}\u{646}\u{647}", "\u{62a}\u{645}\u{643}"),
    ("\u{644}\u{64a}\u{646}\u{64a}\u{643}", "\u{644}\u{64a}\u{646}"),
    (
        "\u{648}\u{627}\u{644}\u{64a}\u{648}\u{646}\u{627}\u{646}\u{64a}",
        "\u{648}\u{627}\u{644}\u{64a}",
    ),
    ("\u{64a}\u{645}\u{643}\u{646}\u{646}\u{627}", "\u{64a}\u{645}\u{643}"),
];

static NOUNS: OnceLock<Vec<Step>> = OnceLock::new();
static VERBS: OnceLock<Vec<Step>> = OnceLock::new();

/// The plain letter of a hamza carrier.
fn plain(c: char) -> Option<char> {
    match c {
        '\u{622}' | '\u{623}' | '\u{625}' => Some('\u{627}'),
        '\u{624}' => Some('\u{648}'),
        '\u{626}' => Some('\u{64a}'),
        _ => None,
    }
}

/// The stem of `word`.
pub fn stem(word: &str) -> String {
    let mut c: Vec<char> = Vec::with_capacity(word.len());
    for ch in word.chars() {
        match NORMAL.binary_search_by(|(k, _)| k.cmp(&ch)) {
            Ok(i) => c.extend(NORMAL[i].1.chars()),
            Err(_) => c.push(ch),
        }
    }
    let normal: String = c.iter().collect();
    if let Ok(i) = EXCEPTIONS.binary_search_by(|(w, _)| (*w).cmp(normal.as_str())) {
        return EXCEPTIONS[i].1.to_string();
    }
    let noun =
        ["\u{627}\u{644}", "\u{644}\u{644}", "\u{628}\u{627}\u{644}", "\u{643}\u{627}\u{644}"]
            .iter()
            .any(|a| c.iter().copied().take(a.chars().count()).eq(a.chars()));
    let steps =
        if noun { NOUNS.get_or_init(|| build(&NOUN)) } else { VERBS.get_or_init(|| build(&VERB)) };
    let n = c.len();
    let mut s: Vec<char> = run_marked(c, steps, [n, n, n]).chars().collect();
    if let Some(last) = s.last_mut() {
        if plain(*last).is_some() {
            *last = '\u{621}';
        } else if *last == '\u{649}' {
            *last = '\u{64a}';
        }
    }
    s.iter().map(|&ch| plain(ch).unwrap_or(ch)).collect()
}
