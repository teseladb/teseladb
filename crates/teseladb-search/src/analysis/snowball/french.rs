//! The French stemmer of snowballstem.org (`algorithms/french/stemmer.html`).

use super::word::{Word, regions};

fn vowel(c: char) -> bool {
    matches!(
        c,
        'a' | 'e'
            | 'i'
            | 'o'
            | 'u'
            | 'y'
            | 'â'
            | 'à'
            | 'ë'
            | 'é'
            | 'ê'
            | 'è'
            | 'ï'
            | 'î'
            | 'ô'
            | 'û'
            | 'ù'
    )
}

const STEP1: &[&str] = &[
    "ance",
    "iqUe",
    "isme",
    "able",
    "iste",
    "eux",
    "ances",
    "iqUes",
    "ismes",
    "ables",
    "istes",
    "atrice",
    "ateur",
    "ation",
    "atrices",
    "ateurs",
    "ations",
    "logie",
    "logies",
    "usion",
    "ution",
    "usions",
    "utions",
    "ence",
    "ences",
    "ement",
    "ements",
    "ité",
    "ités",
    "if",
    "ive",
    "ifs",
    "ives",
    "eaux",
    "aux",
    "euse",
    "euses",
    "issement",
    "issements",
    "amment",
    "emment",
    "ment",
    "ments",
];

const STEP2A: &[&str] = &[
    "îmes", "ît", "îtes", "i", "ie", "ies", "ir", "ira", "irai", "iraIent", "irais", "irait",
    "iras", "irent", "irez", "iriez", "irions", "irons", "iront", "is", "issaIent", "issais",
    "issait", "issant", "issante", "issantes", "issants", "isse", "issent", "isses", "issez",
    "issiez", "issions", "issons", "it",
];

const STEP2B_E: &[&str] = &[
    "é", "ée", "ées", "és", "èrent", "er", "era", "erai", "eraIent", "erais", "erait", "eras",
    "erez", "eriez", "erions", "erons", "eront", "ez", "iez",
];
const STEP2B_A: &[&str] = &[
    "âmes", "ât", "âtes", "a", "ai", "aIent", "ais", "ait", "ant", "ante", "antes", "ants", "as",
    "asse", "assent", "asses", "assiez", "assions",
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
    let mut c: Vec<char> = Vec::new();
    for ch in text.chars() {
        match ch {
            'ë' => c.extend(['H', 'e']),
            'ï' => c.extend(['H', 'i']),
            _ => c.push(ch),
        }
    }
    let n = c.len();
    for i in 0..n {
        let before = i > 0 && vowel(c[i - 1]);
        let after = i + 1 < n && vowel(c[i + 1]);
        match c[i] {
            'u' | 'i' if before && after => c[i] = c[i].to_ascii_uppercase(),
            'y' if before || after => c[i] = 'Y',
            'u' if i > 0 && c[i - 1] == 'q' => c[i] = 'U',
            _ => {}
        }
    }
    Word { c }
}

fn rv(c: &[char]) -> usize {
    let w = Word { c: c.to_vec() };
    if c.len() >= 2 && vowel(c[0]) && vowel(c[1]) {
        return 3.min(c.len());
    }
    if ["par", "col", "tap"].iter().any(|p| w.starts(p)) {
        return 3;
    }
    (1..c.len()).find(|&i| vowel(c[i])).map_or(c.len(), |i| i + 1)
}

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = prelude(text);
    let rv = rv(&w.c);
    let (r1, r2) = regions(&w.c, vowel);
    let before = w.c.clone();
    let step1 = step1(&mut w, rv, r1, r2);
    let mut changed = step1 != Step1::None;
    if step1 != Step1::Done {
        changed |= step2(&mut w, rv, r2);
    }
    if changed {
        if w.ends("Y") {
            w.replace("Y", "i");
        } else if w.ends("ç") {
            w.replace("ç", "c");
        }
    } else if w.c == before {
        residual(&mut w, rv, r2);
    }
    if w.longest(&["enn", "onn", "ett", "ell", "eill"]).is_some() {
        w.cut(1);
    }
    let mut i = w.len();
    while i > 0 && !vowel(w.c[i - 1]) {
        i -= 1;
    }
    if i < w.len() && i > 0 && matches!(w.c[i - 1], 'é' | 'è') {
        w.c[i - 1] = 'e';
    }
    let mut out = String::new();
    let mut i = 0;
    while i < w.len() {
        match (w.c[i], w.at(i + 1)) {
            ('H', Some('e')) => {
                out.push('ë');
                i += 1;
            }
            ('H', Some('i')) => {
                out.push('ï');
                i += 1;
            }
            ('H', _) => {}
            (c, _) => out.push(c.to_ascii_lowercase()),
        }
        i += 1;
    }
    out
}

/// What step 1 did.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Step1 {
    /// Nothing.
    None,
    /// Removed a suffix; step 2 does not run.
    Done,
    /// Removed `amment`, `emment` or `ment(s)`; step 2 runs.
    Verb,
}

fn step1(w: &mut Word, rv: usize, r1: usize, r2: usize) -> Step1 {
    let Some(s) = w.longest(STEP1) else { return Step1::None };
    let start = w.start_of(s);
    let in_r2 = start >= r2;
    let n = s.chars().count();
    match s {
        "ance" | "iqUe" | "isme" | "able" | "iste" | "eux" | "ances" | "iqUes" | "ismes"
        | "ables" | "istes"
            if in_r2 =>
        {
            w.cut(n)
        }
        "atrice" | "ateur" | "ation" | "atrices" | "ateurs" | "ations" if in_r2 => {
            w.cut(n);
            if w.ends("ic") && !drop_from(w, "ic", r2) {
                w.replace("ic", "iqU");
            }
        }
        "logie" | "logies" if in_r2 => w.replace(s, "log"),
        "usion" | "ution" | "usions" | "utions" if in_r2 => w.replace(s, "u"),
        "ence" | "ences" if in_r2 => w.replace(s, "ent"),
        "ement" | "ements" if start >= rv => {
            w.cut(n);
            if drop_from(w, "iv", r2) {
                drop_from(w, "at", r2);
            } else if w.ends("eus") {
                if !drop_from(w, "eus", r2) && w.start_of("eus") >= r1 {
                    w.replace("eus", "eux");
                }
            } else if let Some(t) = w.longest(&["abl", "iqU"]) {
                drop_from(w, t, r2);
            } else if let Some(t) = w.longest(&["ièr", "Ièr"])
                && w.start_of(t) >= rv
            {
                w.replace(t, "i");
            }
        }
        "ité" | "ités" if in_r2 => {
            w.cut(n);
            if w.ends("abil") {
                if !drop_from(w, "abil", r2) {
                    w.replace("abil", "abl");
                }
            } else if w.ends("ic") {
                if !drop_from(w, "ic", r2) {
                    w.replace("ic", "iqU");
                }
            } else {
                drop_from(w, "iv", r2);
            }
        }
        "if" | "ive" | "ifs" | "ives" if in_r2 => {
            w.cut(n);
            if drop_from(w, "at", r2) && w.ends("ic") && !drop_from(w, "ic", r2) {
                w.replace("ic", "iqU");
            }
        }
        "eaux" => w.replace(s, "eau"),
        "aux" if start >= r1 => w.replace(s, "al"),
        "euse" | "euses" if start >= r1 => {
            if in_r2 {
                w.cut(n);
            } else {
                w.replace(s, "eux");
            }
        }
        "issement" | "issements" if start >= r1 && start > 0 && !vowel(w.c[start - 1]) => w.cut(n),
        "amment" if start >= rv => {
            w.replace(s, "ant");
            return Step1::Verb;
        }
        "emment" if start >= rv => {
            w.replace(s, "ent");
            return Step1::Verb;
        }
        "ment" | "ments" if start > rv && vowel(w.c[start - 1]) => {
            w.cut(n);
            return Step1::Verb;
        }
        _ => return Step1::None,
    }
    Step1::Done
}

/// The verb suffixes; says whether one went.
fn step2(w: &mut Word, rv: usize, r2: usize) -> bool {
    if let Some(s) = w.longest_within(STEP2A, rv) {
        let start = w.start_of(s);
        if start > rv && !vowel(w.c[start - 1]) && w.c[start - 1] != 'H' {
            w.cut(s.chars().count());
            return true;
        }
    }
    let all: Vec<&str> = [&["ions"], STEP2B_E, STEP2B_A].concat();
    let Some(s) = w.longest_within(&all, rv) else { return false };
    match s {
        "ions" if w.start_of(s) < r2 => return false,
        _ => w.cut(s.chars().count()),
    }
    if STEP2B_A.contains(&s) {
        drop_from(w, "e", rv);
    }
    true
}

fn residual(w: &mut Word, rv: usize, r2: usize) {
    let kept_i = w.back(1) == Some('i') && w.back(2) != Some('H');
    if w.len() > 1
        && w.ends("s")
        && !kept_i
        && !matches!(w.back(1), Some('a' | 'o' | 'u' | 'è' | 's'))
    {
        w.cut(1);
    }
    let Some(s) = w.longest_within(&["ion", "ier", "ière", "Ier", "Ière", "e", "ë"], rv) else {
        return;
    };
    let start = w.start_of(s);
    match s {
        "ion" => {
            if start >= r2 && start > rv && matches!(w.c[start - 1], 's' | 't') {
                w.cut(3);
            }
        }
        "e" => w.cut(1),
        "ë" => {
            if w.c[..start].ends_with(&['g', 'u']) {
                w.cut(1);
            }
        }
        _ => w.replace(s, "i"),
    }
}
