//! The Estonian stemmer of snowballstem.org (`algorithms/estonian/stemmer.html`), written from its
//! description in the version the reference search server 1.70.6 runs, which is older than the apostrophe rules of
//! Snowball 3.1.0: the apostrophe is not in RV, R1 has no case for it and a final apostrophe stays.
//! The emphasis step needs four characters before the suffix rather than before R1. Both measured
//! on the 57 537 words of the Estonian Snowball vocabulary, all equal to the reference search server.

use super::word::{Word, region};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'õ' | 'ä' | 'ö' | 'ü')
}

/// RV: the letters a suffix may follow in the case endings and the degrees.
fn rv(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'u' | 'o')
}

/// Letters `-ki` may follow.
fn ki(c: char) -> bool {
    matches!(c, 'k' | 'p' | 't' | 'g' | 'b' | 'd' | 's' | 'h' | 'f' | 'š' | 'z' | 'ž')
}

/// Letters `-gi` may follow.
fn gi(c: char) -> bool {
    matches!(c, 'c' | 'j' | 'l' | 'm' | 'n' | 'q' | 'r' | 'v' | 'w' | 'x') || vowel(c)
}

const LONG_VOWELS: &[&str] = &["aa", "ee", "ii", "oo", "uu", "ää", "öö", "üü", "õõ"];

/// Irregular short verbs and the stem each of their forms gets.
const VERB_EXCEPTIONS: &[(&str, &[&str])] = &[
    ("joo", &["joon", "jood", "joob", "joote", "joome", "joovad"]),
    ("joo", &["jõin", "jõid", "jõi", "jõime", "jõite"]),
    ("joo", &["joomata", "juuakse", "joodakse", "juua", "jooma"]),
    ("saa", &["saan", "saad", "saab", "saate", "saame", "saavad"]),
    ("saa", &["saaksin", "saaksid", "saaks", "saaksite", "saaksime"]),
    ("saa", &["sain", "said", "sai", "saite", "saime"]),
    ("saa", &["saamata", "saadakse", "saadi", "saama", "saada"]),
    ("viima", &["viin", "viid", "viib", "viite", "viime", "viivad"]),
    ("viima", &["viiksin", "viiksid", "viiks", "viiksite", "viiksime"]),
    ("viima", &["viisin", "viisite", "viisime"]),
    ("viima", &["viimata", "viiakse", "viidi", "viima", "viia"]),
    ("keesi", &["keen", "keeb", "keed", "kees", "keeme", "keete", "keevad"]),
    ("keesi", &["keeksin", "keeks", "keeksid", "keeksime", "keeksite"]),
    ("keesi", &["keemata", "keema", "keeta", "keedakse"]),
    ("löö", &["löön", "lööd", "lööb", "lööme", "lööte", "löövad"]),
    ("löö", &["lööksin", "lööksid", "lööks", "lööksime", "lööksite"]),
    ("löö", &["löömata", "lüüakse", "löödakse", "löödi", "lööma", "lüüa"]),
    ("lõi", &["lõin", "lõid", "lõi", "lõime", "lõite"]),
    ("loo", &["loon", "lood", "loob", "loome", "loote", "loovad"]),
    ("loo", &["looksin", "looksid", "looks", "looksime", "looksite"]),
    ("loo", &["loomata", "luuakse", "loodi", "luua", "looma"]),
    ("käisi", &["käin", "käib", "käid", "käis", "käime", "käite", "käivad"]),
    ("käisi", &["käiksin", "käiks", "käiksid", "käiksime", "käiksite"]),
    ("käisi", &["käimata", "käiakse", "käidi", "käia", "käima"]),
    ("söö", &["söön", "sööb", "sööd", "sööme", "sööte", "söövad"]),
    ("söö", &["sööksin", "sööks", "sööksid", "sööksime", "sööksite"]),
    ("söö", &["sõin", "sõi", "sõid", "sõime", "sõite"]),
    ("söö", &["söömata", "süüakse", "söödakse", "söödi", "sööma", "süüa"]),
    ("too", &["toon", "tood", "toob", "toote", "toome", "toovad"]),
    ("too", &["tooksin", "tooksid", "tooks", "tooksite", "tooksime"]),
    ("too", &["tõin", "tõid", "tõi", "tõime", "tõite"]),
    ("too", &["toomata", "tuuakse", "toodi", "tooma", "tuua"]),
    ("võisi", &["võin", "võid", "võib", "võime", "võis", "võite", "võivad"]),
    ("võisi", &["võiksin", "võiksid", "võiks", "võiksime", "võiksite"]),
    ("võisi", &["võimata", "võidakse", "võidi", "võida", "võima"]),
    ("jääma", &["jään", "jääd", "jääb", "jääme", "jääte", "jäävad"]),
    ("jääma", &["jääksin", "jääksid", "jääks", "jääksime", "jääksite"]),
    ("jääma", &["jäime", "jäite", "jäin", "jäid", "jäi"]),
    ("jääma", &["jäämata", "jäädakse", "jääda", "jääma", "jäädi"]),
    ("müüsi", &["müün", "müüd", "müüb", "müüs", "müüme", "müüte", "müüvad"]),
    ("müüsi", &["müüksin", "müüksid", "müüks", "müüksime", "müüksite"]),
    ("müüsi", &["müümata", "müüakse", "müüdi", "müüa", "müüma"]),
    ("luge", &["loeb", "loen", "loed", "loeme", "loete", "loevad"]),
    ("luge", &["loeks", "loeksin", "loeksid", "loeksime", "loeksite"]),
    ("põde", &["põen", "põeb", "põed", "põeme", "põete", "põevad"]),
    ("põde", &["põeksin", "põeks", "põeksid", "põeksime", "põeksite"]),
    ("ladu", &["laon", "laob", "laod", "laome", "laote", "laovad"]),
    ("ladu", &["laoksin", "laoks", "laoksid", "laoksime", "laoksite"]),
    ("tegi", &["teeksin", "teeks", "teeksid", "teeksime", "teeksite"]),
    ("tegi", &["teen", "teeb", "teed", "teeme", "teete", "teevad"]),
    ("tegi", &["tegemata", "tehakse", "tehti", "tegema", "teha"]),
    ("nägi", &["näen", "näeb", "näed", "näeme", "näete", "näevad"]),
    ("nägi", &["näeksin", "näeks", "näeksid", "näeksime", "näeksite"]),
    ("nägi", &["nägemata", "nähakse", "nähti", "näha", "nägema"]),
];

const VERB: &[&str] = &[
    "nuksin", "nuksime", "nuksid", "nuksite", "ksin", "ksid", "ksime", "ksite", "mata", "takse",
    "dakse", "taks", "daks", "akse", "sime", "site", "sin", "me", "da", "n", "b",
];
const SPECIAL_NOUN: &[&str] = &[
    "lasse", "last", "lane", "lasi", "misse", "mist", "mine", "misi", "lisse", "list", "line",
    "lisi",
];
const CASE: &[&str] = &["sse", "st", "le", "lt", "ga", "ks", "ta", "t", "s", "l"];
const PLURAL: &[&str] = &["ikkude", "ikke", "ike", "sid", "te", "de", "d"];

/// Whether the `n` characters before the last `len` end in a long vowel.
fn long_vowel_before(w: &Word, len: usize) -> bool {
    let end = w.len() - len;
    end >= 2 && LONG_VOWELS.iter().any(|v| v.chars().eq(w.c[end - 2..end].iter().copied()))
}

/// The character just before the last `len`.
fn before(w: &Word, len: usize) -> Option<char> {
    w.back(len)
}

fn emphasis(w: &mut Word, r1: usize) {
    let Some(s) = w.longest_within(&["gi", "ki"], r1) else { return };
    let start = w.start_of(s);
    let Some(c) = before(w, 2) else { return };
    let ok = if s == "gi" { gi(c) && !long_vowel_before(w, 2) } else { ki(c) };
    if start >= 4 && ok {
        w.cut(2);
    }
}

/// Says whether a verb ending went.
fn verb(w: &mut Word, r1: usize) -> bool {
    let Some(s) = w.longest_within(VERB, r1) else { return false };
    let n = s.chars().count();
    match s {
        "akse" => w.replace(s, "a"),
        "me" | "da" | "n" | "b" => {
            if !before(w, n).is_some_and(vowel) {
                return false;
            }
            w.cut(n);
        }
        _ => w.cut(n),
    }
    true
}

fn special_noun(w: &mut Word, r1: usize) {
    let Some(s) = w.longest_within(SPECIAL_NOUN, r1) else { return };
    let by = if s.starts_with("la") {
        "lase"
    } else if s.starts_with('m') {
        "mise"
    } else {
        "lise"
    };
    w.replace(s, by);
}

/// Whether the suffix of `len` characters follows a letter of RV or a long vowel.
fn after_rv_or_long(w: &Word, len: usize) -> bool {
    before(w, len).is_some_and(rv) || long_vowel_before(w, len)
}

fn case_ending(w: &mut Word, r1: usize) {
    let Some(s) = w.longest_within(CASE, r1) else { return };
    let n = s.chars().count();
    let ok = if s == "t" { w.len() - n >= 4 } else { after_rv_or_long(w, n) };
    if ok {
        w.cut(n);
    }
}

fn plural(w: &mut Word, r1: usize) {
    let Some(s) = w.longest_within(PLURAL, r1) else { return };
    let n = s.chars().count();
    match s {
        "ikkude" | "ikke" | "ike" => w.replace(s, "iku"),
        "sid" => {
            if !long_vowel_before(w, n) {
                w.cut(n);
            }
        }
        "te" => {
            w.cut(n);
            if w.len() < 4 {
                w.c.push('t');
            } else if w.ends("mis") || w.ends("las") || w.ends("lis") {
                w.c.push('e');
            } else if w.ends("t") {
                w.c.extend(['t', 'e']);
            }
        }
        _ => {
            if after_rv_or_long(w, n) {
                w.cut(n);
            }
        }
    }
}

fn degrees(w: &mut Word, r1: usize) {
    let Some(s) = w.longest_within(&["mai", "ma", "m"], r1) else { return };
    let n = s.chars().count();
    if s == "ma" || before(w, n).is_some_and(rv) {
        w.cut(n);
    }
}

fn i_plural(w: &mut Word, r1: usize) {
    if w.longest_within(&["i"], r1).is_some() && before(w, 1).is_some_and(rv) {
        w.cut(1);
    }
}

fn nu(w: &mut Word, r1: usize) {
    if let Some(s) = w.longest_within(&["nu", "tu", "du", "va"], r1) {
        w.cut(s.chars().count());
    }
}

fn undouble_kpt(w: &mut Word, r1: usize) {
    let n = w.len();
    let double = n >= 3 && w.c[n - 2] == w.c[n - 3] && matches!(w.c[n - 2], 'k' | 'p' | 't');
    if double && vowel(w.c[n - 1]) && n > r1 {
        w.c.remove(n - 2);
    }
}

/// The stem of `word`.
pub fn stem(word: &str) -> String {
    if let Some((stem, _)) = VERB_EXCEPTIONS.iter().find(|(_, forms)| forms.contains(&word)) {
        return stem.to_string();
    }
    let mut w = Word::new(word);
    let r1 = region(&w.c, 0, vowel);
    emphasis(&mut w, r1);
    if !verb(&mut w, r1) {
        special_noun(&mut w, r1);
        case_ending(&mut w, r1);
        plural(&mut w, r1);
        degrees(&mut w, r1);
        i_plural(&mut w, r1);
        nu(&mut w, r1);
    }
    undouble_kpt(&mut w, r1);
    w.text()
}
