//! The Italian stemmer of snowballstem.org (`algorithms/italian/stemmer.html`).

use super::word::{Word, regions, rv};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'à' | 'è' | 'ì' | 'ò' | 'ù')
}

const PRONOUNS: &[&str] = &[
    "ci", "gli", "la", "le", "li", "lo", "mi", "ne", "si", "ti", "vi", "sene", "gliela", "gliele",
    "glieli", "glielo", "gliene", "mela", "mele", "meli", "melo", "mene", "tela", "tele", "teli",
    "telo", "tene", "cela", "cele", "celi", "celo", "cene", "vela", "vele", "veli", "velo", "vene",
];

const STEP1_R2: &[&str] = &[
    "anza", "anze", "ico", "ici", "ica", "ice", "iche", "ichi", "ismo", "ismi", "abile", "abili",
    "ibile", "ibili", "ista", "iste", "isti", "istà", "istè", "istì", "oso", "osi", "osa", "ose",
    "mente", "atrice", "atrici", "ante", "anti",
];
const STEP1_IC: &[&str] = &["azione", "azioni", "atore", "atori"];
const STEP1_OTHER: &[&str] = &[
    "logia", "logie", "uzione", "uzioni", "usione", "usioni", "enza", "enze", "amento", "amenti",
    "imento", "imenti", "amente", "ità", "ivo", "ivi", "iva", "ive",
];

const VERB: &[&str] = &[
    "ammo", "ando", "ano", "are", "arono", "asse", "assero", "assi", "assimo", "ata", "ate", "ati",
    "ato", "ava", "avamo", "avano", "avate", "avi", "avo", "emmo", "enda", "ende", "endi", "endo",
    "erà", "erai", "eranno", "ere", "erebbe", "erebbero", "erei", "eremmo", "eremo", "ereste",
    "eresti", "erete", "erò", "erono", "essero", "ete", "eva", "evamo", "evano", "evate", "evi",
    "evo", "iamo", "immo", "irà", "irai", "iranno", "ire", "irebbe", "irebbero", "irei", "iremmo",
    "iremo", "ireste", "iresti", "irete", "irò", "irono", "isca", "iscano", "isce", "isci", "isco",
    "iscono", "issero", "ita", "ite", "iti", "ito", "iva", "ivamo", "ivano", "ivate", "ivi", "ivo",
    "ar", "ir", "ono", "uta", "ute", "uti", "uto",
];

/// Removes `suffix` when it starts at or after `from`, and says whether it did.
fn drop_from(w: &mut Word, suffix: &str, from: usize) -> bool {
    let done = w.ends(suffix) && w.start_of(suffix) >= from;
    if done {
        w.cut(suffix.chars().count());
    }
    done
}

fn prelude(text: &str) -> Word {
    let mut c: Vec<char> = text
        .chars()
        .map(|c| match c {
            'á' => 'à',
            'é' => 'è',
            'í' => 'ì',
            'ó' => 'ò',
            'ú' => 'ù',
            _ => c,
        })
        .collect();
    for i in 1..c.len() {
        if c[i] == 'u' && c[i - 1] == 'q' {
            c[i] = 'U';
        }
    }
    for i in 1..c.len().saturating_sub(1) {
        if matches!(c[i], 'u' | 'i') && vowel(c[i - 1]) && vowel(c[i + 1]) {
            c[i] = c[i].to_ascii_uppercase();
        }
    }
    Word { c }
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = prelude(text);
    // `refsearch` starts RV after `divan` in the words that begin with it (`divano`, `divani` → `divan`).
    let rv = if w.starts("divan") { 5 } else { rv(&w.c, vowel) };
    let (r1, r2) = regions(&w.c, vowel);
    attached_pronoun(&mut w, rv);
    if !step1(&mut w, rv, r1, r2)
        && let Some(s) = w.longest_within(VERB, rv)
    {
        w.cut(s.chars().count());
    }
    if let Some(s) = w.longest_within(&["a", "e", "i", "o", "à", "è", "ì", "ò"], rv) {
        w.cut(s.chars().count());
        drop_from(&mut w, "i", rv);
    }
    if w.longest_within(&["ch", "gh"], rv).is_some() {
        w.cut(1);
    }
    w.c.iter().map(|c| c.to_ascii_lowercase()).collect()
}

fn attached_pronoun(w: &mut Word, rv: usize) {
    let Some(p) = w.longest(PRONOUNS) else { return };
    let rest = Word { c: w.c[..w.start_of(p)].to_vec() };
    let Some(v) = rest.longest(&["ando", "endo", "ar", "er", "ir"]) else { return };
    if rest.start_of(v) < rv {
        return;
    }
    if v.len() == 2 {
        w.replace(p, "e");
    } else {
        w.cut(p.chars().count());
    }
}

/// The standard suffixes; says whether one went.
fn step1(w: &mut Word, rv: usize, r1: usize, r2: usize) -> bool {
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
        "logia" | "logie" if in_r2 => w.replace(s, "log"),
        "uzione" | "uzioni" | "usione" | "usioni" if in_r2 => w.replace(s, "u"),
        "enza" | "enze" if in_r2 => w.replace(s, "ente"),
        "amento" | "amenti" | "imento" | "imenti" if start >= rv => w.cut(6),
        "amente" if start >= r1 => {
            w.cut(6);
            if drop_from(w, "iv", r2) {
                drop_from(w, "at", r2);
            } else if let Some(t) = w.longest(&["os", "ic", "abil"]) {
                drop_from(w, t, r2);
            }
        }
        "ità" if in_r2 => {
            w.cut(3);
            if let Some(t) = w.longest(&["abil", "ic", "iv"]) {
                drop_from(w, t, r2);
            }
        }
        "ivo" | "ivi" | "iva" | "ive" if in_r2 => {
            w.cut(3);
            if drop_from(w, "at", r2) {
                drop_from(w, "ic", r2);
            }
        }
        _ => return false,
    }
    true
}
