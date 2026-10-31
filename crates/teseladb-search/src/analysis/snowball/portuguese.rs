//! The Portuguese stemmer of snowballstem.org (`algorithms/portuguese/stemmer.html`). As the
//! description does, `ã` and `õ` become `a~` and `o~` while stemming.

use super::word::{Word, regions, rv};

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'á' | 'é' | 'í' | 'ó' | 'ú' | 'â' | 'ê' | 'ô')
}

const STEP1_R2: &[&str] = &[
    "eza", "ezas", "ico", "ica", "icos", "icas", "ismo", "ismos", "ável", "ível", "ista", "istas",
    "oso", "osa", "osos", "osas", "amento", "amentos", "imento", "imentos", "adora", "ador",
    "aça~o", "adoras", "adores", "aço~es", "ante", "antes", "ância",
];
const STEP1_OTHER: &[&str] = &[
    "logia", "logias", "uça~o", "uço~es", "ência", "ências", "amente", "mente", "idade", "idades",
    "iva", "ivo", "ivas", "ivos", "ira", "iras",
];

const VERB: &[&str] = &[
    "ada", "ida", "ia", "aria", "eria", "iria", "ará", "ara", "erá", "era", "irá", "ava", "asse",
    "esse", "isse", "aste", "este", "iste", "ei", "arei", "erei", "irei", "am", "iam", "ariam",
    "eriam", "iriam", "aram", "eram", "iram", "avam", "em", "arem", "erem", "irem", "assem",
    "essem", "issem", "ado", "ido", "ando", "endo", "indo", "ara~o", "era~o", "ira~o", "ar", "er",
    "ir", "as", "adas", "idas", "ias", "arias", "erias", "irias", "arás", "aras", "erás", "eras",
    "irás", "avas", "es", "ardes", "erdes", "irdes", "ares", "eres", "ires", "asses", "esses",
    "isses", "astes", "estes", "istes", "is", "ais", "eis", "íeis", "aríeis", "eríeis", "iríeis",
    "áreis", "éreis", "íreis", "ásseis", "ésseis", "ísseis", "áveis", "ados", "idos", "ámos",
    "amos", "íamos", "aríamos", "eríamos", "iríamos", "áramos", "éramos", "íramos", "ávamos",
    "emos", "aremos", "eremos", "iremos", "ássemos", "êssemos", "íssemos", "imos", "armos",
    "ermos", "irmos", "eu", "iu", "ou", "ira", "iras",
];

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
    let mut w = Word { c: Vec::new() };
    for c in text.chars() {
        match c {
            'ã' => w.c.extend(['a', '~']),
            'õ' => w.c.extend(['o', '~']),
            _ => w.c.push(c),
        }
    }
    let rv = rv(&w.c, vowel);
    let (r1, r2) = regions(&w.c, vowel);
    let before = w.c.clone();
    if !step1(&mut w, rv, r1, r2)
        && let Some(s) = w.longest_within(VERB, rv)
    {
        w.cut(s.chars().count());
    }
    if w.c != before {
        if w.ends("ci") && w.len() > rv {
            w.cut(1);
        }
    } else if let Some(s) = w.longest_within(&["os", "a", "i", "o", "á", "í", "ó"], rv) {
        w.cut(s.chars().count());
    }
    if let Some(s) = w.longest_within(&["e", "é", "ê"], rv) {
        w.cut(s.chars().count());
        if (w.ends("gu") || w.ends("ci")) && w.len() > rv {
            w.cut(1);
        }
    } else if w.ends("ç") {
        w.replace("ç", "c");
    }
    let mut out = String::new();
    for &c in &w.c {
        match (out.chars().last(), c) {
            (Some('a'), '~') => {
                out.pop();
                out.push('ã');
            }
            (Some('o'), '~') => {
                out.pop();
                out.push('õ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// The standard suffixes; says whether one went.
fn step1(w: &mut Word, rv: usize, r1: usize, r2: usize) -> bool {
    let all: Vec<&str> = [STEP1_R2, STEP1_OTHER].concat();
    let Some(s) = w.longest(&all) else { return false };
    let start = w.start_of(s);
    let in_r2 = start >= r2;
    match s {
        _ if STEP1_R2.contains(&s) && in_r2 => w.cut(s.chars().count()),
        "logia" | "logias" if in_r2 => w.replace(s, "log"),
        "uça~o" | "uço~es" if in_r2 => w.replace(s, "u"),
        "ência" | "ências" if in_r2 => w.replace(s, "ente"),
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
            if let Some(t) = w.longest(&["ante", "avel", "ível"]) {
                drop_from(w, t, r2);
            }
        }
        "idade" | "idades" if in_r2 => {
            w.cut(s.chars().count());
            if let Some(t) = w.longest(&["abil", "ic", "iv"]) {
                drop_from(w, t, r2);
            }
        }
        "iva" | "ivo" | "ivas" | "ivos" if in_r2 => {
            w.cut(s.chars().count());
            drop_from(w, "at", r2);
        }
        "ira" | "iras" if start >= rv && w.c.get(start.wrapping_sub(1)) == Some(&'e') => {
            w.replace(s, "ir");
        }
        _ => return false,
    }
    true
}
