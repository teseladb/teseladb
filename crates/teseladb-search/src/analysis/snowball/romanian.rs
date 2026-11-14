//! The Romanian stemmer of snowballstem.org (`algorithms/romanian/stemmer.html`).

use super::word::{Word, regions, rv};
use crate::analysis::scratch::Chars;

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'ă' | 'â' | 'e' | 'i' | 'î' | 'o' | 'u')
}

const STEP0: &[(&str, &str)] = &[
    ("ul", ""),
    ("ului", ""),
    ("aua", "a"),
    ("ea", "e"),
    ("ele", "e"),
    ("elor", "e"),
    ("ii", "i"),
    ("iua", "i"),
    ("iei", "i"),
    ("iile", "i"),
    ("iilor", "i"),
    ("ilor", "i"),
    ("ile", "i"),
    ("atei", "at"),
    ("ație", "ați"),
    ("ația", "ați"),
];

const STEP1: &[(&[&str], &str)] = &[
    (&["abilitate", "abilitati", "abilităi", "abilități"], "abil"),
    (&["ibilitate"], "ibil"),
    (&["ivitate", "ivitati", "ivităi", "ivități"], "iv"),
    (
        &[
            "icitate",
            "icitati",
            "icităi",
            "icități",
            "icator",
            "icatori",
            "iciv",
            "iciva",
            "icive",
            "icivi",
            "icivă",
            "ical",
            "icala",
            "icale",
            "icali",
            "icală",
        ],
        "ic",
    ),
    (
        &[
            "ativ", "ativa", "ative", "ativi", "ativă", "ațiune", "atoare", "ator", "atori",
            "ătoare", "ător", "ători",
        ],
        "at",
    ),
    (&["itiv", "itiva", "itive", "itivi", "itivă", "ițiune", "itoare", "itor", "itori"], "it"),
];

const STEP2_DELETE: &[&str] = &[
    "at", "ata", "ată", "ati", "ate", "ut", "uta", "ută", "uti", "ute", "it", "ita", "ită", "iti",
    "ite", "ic", "ica", "ice", "ici", "ică", "abil", "abila", "abile", "abili", "abilă", "ibil",
    "ibila", "ibile", "ibili", "ibilă", "oasa", "oasă", "oase", "os", "osi", "oși", "ant", "anta",
    "ante", "anti", "antă", "ator", "atori", "itate", "itati", "ităi", "ități", "iv", "iva", "ive",
    "ivi", "ivă",
];
const STEP2_IST: &[&str] = &["ism", "isme", "ist", "ista", "iste", "isti", "istă", "iști"];

const STEP3_AFTER_CONSONANT: &[&str] = &[
    "are",
    "ere",
    "ire",
    "âre",
    "ind",
    "ând",
    "indu",
    "ându",
    "eze",
    "ească",
    "ez",
    "ezi",
    "ează",
    "esc",
    "ești",
    "ește",
    "ăsc",
    "ăști",
    "ăște",
    "am",
    "ai",
    "au",
    "eam",
    "eai",
    "ea",
    "eați",
    "eau",
    "iam",
    "iai",
    "ia",
    "iați",
    "iau",
    "ui",
    "ași",
    "arăm",
    "arăți",
    "ară",
    "uși",
    "urăm",
    "urăți",
    "ură",
    "iși",
    "irăm",
    "irăți",
    "iră",
    "âi",
    "âși",
    "ârăm",
    "ârăți",
    "âră",
    "asem",
    "aseși",
    "ase",
    "aserăm",
    "aserăți",
    "aseră",
    "isem",
    "iseși",
    "ise",
    "iserăm",
    "iserăți",
    "iseră",
    "âsem",
    "âseși",
    "âse",
    "âserăm",
    "âserăți",
    "âseră",
    "usem",
    "useși",
    "use",
    "userăm",
    "userăți",
    "useră",
];
const STEP3_DELETE: &[&str] = &[
    "ăm",
    "ați",
    "em",
    "eți",
    "im",
    "iți",
    "âm",
    "âți",
    "seși",
    "serăm",
    "serăți",
    "seră",
    "sei",
    "se",
    "sesem",
    "seseși",
    "sese",
    "seserăm",
    "seserăți",
    "seseră",
];

fn prelude(text: &str) -> Word {
    let mut c = Chars::collect(text.chars().map(|c| match c {
        'ş' => 'ș',
        'ţ' => 'ț',
        _ => c,
    }));
    for i in 1..c.len().saturating_sub(1) {
        if matches!(c[i], 'u' | 'i') && vowel(c[i - 1]) && vowel(c[i + 1]) {
            c[i] = c[i].to_ascii_uppercase();
        }
    }
    Word { c }
}

/// Stems `text`.
pub fn stem(text: &str, out: &mut String) {
    let mut w = prelude(text);
    let rv = rv(&w.c, vowel);
    let (r1, r2) = regions(&w.c, vowel);
    if let Some(s) = w.longest_from(STEP0.iter().map(|(k, _)| k), r1) {
        let by = STEP0.iter().find(|(k, _)| *k == s).map_or("", |(_, v)| v);
        if s != "ile" || !w.c[..w.start_of(s)].ends_with(&['a', 'b']) {
            w.replace(s, by);
        }
    }
    let keys1 = || STEP1.iter().flat_map(|(k, _)| k.iter());
    let mut removed = false;
    while let Some(s) = w.longest_from(keys1(), r1) {
        let by = STEP1.iter().find(|(k, _)| k.contains(&s)).map_or("", |(_, v)| v);
        w.replace(s, by);
        removed = true;
    }
    let keys2 = STEP2_DELETE.iter().chain(STEP2_IST).chain(&["iune", "iuni"]);
    if let Some(s) = w.longest_from(keys2, r2) {
        if STEP2_IST.contains(&s) {
            w.replace(s, "ist");
            removed = true;
        } else if STEP2_DELETE.contains(&s) {
            w.cut(s.chars().count());
            removed = true;
        } else if w.c[..w.start_of(s)].ends_with(&['ț']) {
            // `ț` and the suffix become `t`.
            w.cut(s.chars().count() + 1);
            w.c.push('t');
            removed = true;
        }
    }
    if !removed {
        let keys3 = STEP3_AFTER_CONSONANT.iter().chain(STEP3_DELETE);
        if let Some(s) = w.longest_within(keys3, rv) {
            let start = w.start_of(s);
            let after_consonant = start > rv && (!vowel(w.c[start - 1]) || w.c[start - 1] == 'u');
            if STEP3_DELETE.contains(&s) || after_consonant {
                w.cut(s.chars().count());
            }
        }
    }
    if let Some(s) = w.longest_from(&["a", "e", "i", "ie", "ă"], rv) {
        w.cut(s.chars().count());
    }
    out.extend(w.c.iter().map(|c| c.to_ascii_lowercase()));
}
