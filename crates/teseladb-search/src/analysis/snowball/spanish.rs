//! The Spanish stemmer of snowballstem.org (`algorithms/spanish/stemmer.html`).

use super::word::{Word, regions, rv};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'ü')
}

const PRONOUNS: &[&str] =
    &["me", "se", "sela", "selo", "selas", "selos", "la", "le", "lo", "las", "les", "los", "nos"];

const BEFORE_PRONOUN: &[&str] =
    &["iéndo", "ándo", "ár", "ér", "ír", "ando", "iendo", "ar", "er", "ir", "yendo"];

const STEP1_R2: &[&str] = &[
    "anza", "anzas", "ico", "ica", "icos", "icas", "ismo", "ismos", "able", "ables", "ible",
    "ibles", "ista", "istas", "oso", "osa", "osos", "osas", "amiento", "amientos", "imiento",
    "imientos",
];
const STEP1_IC: &[&str] =
    &["adora", "ador", "ación", "adoras", "adores", "aciones", "ante", "antes", "ancia", "ancias"];
const STEP1_OTHER: &[&str] = &[
    "logía", "logías", "ución", "uciones", "encia", "encias", "amente", "mente", "idad", "idades",
    "iva", "ivo", "ivas", "ivos",
];

const STEP2A: &[&str] =
    &["ya", "ye", "yan", "yen", "yeron", "yendo", "yo", "yó", "yas", "yes", "yais", "yamos"];

const STEP2B_GU: &[&str] = &["en", "es", "éis", "emos"];
const STEP2B: &[&str] = &[
    "arían", "arías", "arán", "arás", "aríais", "aría", "aréis", "aríamos", "aremos", "ará", "aré",
    "erían", "erías", "erán", "erás", "eríais", "ería", "eréis", "eríamos", "eremos", "erá", "eré",
    "irían", "irías", "irán", "irás", "iríais", "iría", "iréis", "iríamos", "iremos", "irá", "iré",
    "aba", "ada", "ida", "ía", "ara", "iera", "ad", "ed", "id", "ase", "iese", "aste", "iste",
    "an", "aban", "ían", "aran", "ieran", "asen", "iesen", "aron", "ieron", "ado", "ido", "ando",
    "iendo", "ió", "ar", "er", "ir", "as", "abas", "adas", "idas", "ías", "aras", "ieras", "ases",
    "ieses", "ís", "áis", "abais", "íais", "arais", "ierais", "aseis", "ieseis", "asteis",
    "isteis", "ados", "idos", "amos", "ábamos", "íamos", "imos", "áramos", "iéramos", "iésemos",
    "ásemos",
];

/// Removes the acute accents.
pub fn unaccent(c: char) -> char {
    match c {
        'á' => 'a',
        'é' => 'e',
        'í' => 'i',
        'ó' => 'o',
        'ú' => 'u',
        _ => c,
    }
}

/// Removes `suffix` when it starts at or after `from`, and says whether it did.
fn drop_from(w: &mut Word, suffix: &str, from: usize) -> bool {
    let done = w.ends(suffix) && w.start_of(suffix) >= from;
    if done {
        w.cut(suffix.chars().count());
    }
    done
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = Word::new(text);
    let rv = rv(&w.c, vowel);
    let (r1, r2) = regions(&w.c, vowel);
    attached_pronoun(&mut w, rv);
    if !step1(&mut w, r1, r2) {
        let before = w.len();
        if let Some(s) = w.longest_within(STEP2A, rv)
            && w.c.get(w.start_of(s).wrapping_sub(1)) == Some(&'u')
        {
            w.cut(s.chars().count());
        }
        if w.len() == before {
            step2b(&mut w, rv);
        }
    }
    if let Some(s) = w.longest_within(&["os", "a", "o", "á", "í", "ó", "e", "é"], rv) {
        w.cut(s.chars().count());
        if matches!(s, "e" | "é") && w.ends("gu") && w.len() > rv {
            w.cut(1);
        }
    }
    w.c.iter().map(|&c| unaccent(c)).collect()
}

fn attached_pronoun(w: &mut Word, rv: usize) {
    let Some(p) = w.longest(PRONOUNS) else { return };
    let rest = Word { c: w.c[..w.start_of(p)].to_vec() };
    let Some(v) = rest.longest(BEFORE_PRONOUN) else { return };
    let start = rest.start_of(v);
    if start < rv || (v == "yendo" && rest.c.get(start.wrapping_sub(1)) != Some(&'u')) {
        return;
    }
    w.cut(p.chars().count());
    for c in &mut w.c[start..] {
        *c = unaccent(*c);
    }
}

/// The standard suffixes; says whether one went.
fn step1(w: &mut Word, r1: usize, r2: usize) -> bool {
    let all: Vec<&str> = [STEP1_R2, STEP1_IC, STEP1_OTHER].concat();
    let Some(s) = w.longest(&all) else { return false };
    let start = w.start_of(s);
    let in_r2 = start >= r2;
    match s {
        _ if STEP1_R2.contains(&s) && in_r2 => w.cut(s.chars().count()),
        _ if STEP1_IC.contains(&s) && in_r2 => {
            w.cut(s.chars().count());
            drop_from(w, "ic", r2);
        }
        "logía" | "logías" if in_r2 => w.replace(s, "log"),
        "ución" | "uciones" if in_r2 => w.replace(s, "u"),
        "encia" | "encias" if in_r2 => w.replace(s, "ente"),
        "amente" if start >= r1 => {
            w.cut(6);
            if drop_from(w, "iv", r2) {
                drop_from(w, "at", r2);
            } else if let Some(t) = w.longest(&["os", "ic", "ad"]) {
                drop_from(w, t, r2);
            }
        }
        "mente" if in_r2 => {
            w.cut(5);
            if let Some(t) = w.longest(&["ante", "able", "ible"]) {
                drop_from(w, t, r2);
            }
        }
        "idad" | "idades" if in_r2 => {
            w.cut(s.chars().count());
            if let Some(t) = w.longest(&["abil", "ic", "iv"]) {
                drop_from(w, t, r2);
            }
        }
        "iva" | "ivo" | "ivas" | "ivos" if in_r2 => {
            w.cut(s.chars().count());
            drop_from(w, "at", r2);
        }
        _ => return false,
    }
    true
}

fn step2b(w: &mut Word, rv: usize) {
    let all: Vec<&str> = [STEP2B_GU, STEP2B].concat();
    let Some(s) = w.longest_within(&all, rv) else { return };
    w.cut(s.chars().count());
    if STEP2B_GU.contains(&s) && w.ends("gu") && w.len() > rv {
        w.cut(1);
    }
}
