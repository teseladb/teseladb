//! The Hungarian stemmer of snowballstem.org (`algorithms/hungarian/stemmer.html`), with R1
//! kept out of the middle of a digraph, as `refsearch` does.

use super::word::Word;

fn vowel(c: char) -> bool {
    matches!(c, 'a' | 'á' | 'e' | 'é' | 'i' | 'í' | 'o' | 'ó' | 'ö' | 'ő' | 'u' | 'ú' | 'ü' | 'ű')
}

const DIGRAPHS: &[&str] = &["cs", "dzs", "dz", "gy", "ly", "ny", "sz", "ty", "zs"];

const DOUBLES: &[&str] = &[
    "bb", "cc", "ccs", "dd", "ff", "gg", "ggy", "jj", "kk", "ll", "lly", "mm", "nn", "nny", "pp",
    "rr", "ss", "ssz", "tt", "tty", "vv", "zz", "zzs",
];

/// R1: after the first consonant (or digraph) when the word starts with a vowel, otherwise
/// after the first vowel.
fn r1(c: &[char]) -> usize {
    let Some(&first) = c.first() else { return 0 };
    if !vowel(first) {
        return c.iter().position(|&x| vowel(x)).map_or(c.len(), |i| i + 1);
    }
    let Some(i) = c.iter().position(|&x| !vowel(x)) else { return c.len() };
    let rest = Word { c: c[i..].to_vec() };
    DIGRAPHS.iter().find(|d| rest.starts(d)).map_or(i + 1, |d| i + d.chars().count())
}

/// Removes `suffix` and one letter of the double consonant before it, when the suffix is in R1.
fn instrumental(w: &mut Word, suffixes: &[&str], r1: usize) {
    let Some(s) = w.longest_from(suffixes, r1) else { return };
    let rest = Word { c: w.c[..w.start_of(s)].to_vec() };
    if rest.longest(DOUBLES).is_some() {
        w.cut(s.chars().count());
        let n = w.len();
        w.c.remove(n - 2);
    }
}

/// Replaces the longest suffix of `table` in R1; says whether one went.
fn rewrite(w: &mut Word, table: &[(&[&str], &str)], r1: usize) -> bool {
    let keys: Vec<&str> = table.iter().flat_map(|(k, _)| k.iter().copied()).collect();
    let Some(s) = w.longest_from(&keys, r1) else { return false };
    let by = table.iter().find(|(k, _)| k.contains(&s)).map_or("", |(_, v)| v);
    w.replace(s, by);
    true
}

const STEP2: &[&str] = &[
    "ban", "ben", "ba", "be", "ra", "re", "nak", "nek", "val", "vel", "tól", "től", "ról", "ről",
    "ból", "ből", "hoz", "hez", "höz", "nál", "nél", "ig", "at", "et", "ot", "öt", "ért", "képp",
    "képpen", "kor", "ul", "ül", "vá", "vé", "onként", "enként", "anként", "ként", "en", "on",
    "an", "ön", "n", "t",
];

/// Stems `text`.
pub fn stem(text: &str) -> String {
    let mut w = Word::new(text);
    let r1 = r1(&w.c);
    instrumental(&mut w, &["al", "el"], r1);
    if rewrite(&mut w, &[(STEP2, "")], r1)
        && let Some(s) = w.longest_from(&["á", "é"], r1)
    {
        w.replace(s, if s == "á" { "a" } else { "e" });
    }
    rewrite(&mut w, &[(&["án", "ánként"], "a"), (&["én"], "e")], r1);
    rewrite(
        &mut w,
        &[(&["astul", "estül", "stul", "stül"], ""), (&["ástul"], "a"), (&["éstül"], "e")],
        r1,
    );
    instrumental(&mut w, &["á", "é"], r1);
    rewrite(
        &mut w,
        &[
            (&["oké", "öké", "aké", "eké", "ké", "éi", "é"], ""),
            (&["áké", "áéi"], "a"),
            (&["éké", "ééi", "éé"], "e"),
        ],
        r1,
    );
    rewrite(
        &mut w,
        &[
            (
                &[
                    "ünk", "unk", "nk", "juk", "jük", "uk", "ük", "em", "om", "am", "m", "od",
                    "ed", "ad", "öd", "d", "ja", "je", "a", "e", "o",
                ],
                "",
            ),
            (&["ánk", "ájuk", "ám", "ád", "á"], "a"),
            (&["énk", "éjük", "ém", "éd", "é"], "e"),
        ],
        r1,
    );
    rewrite(
        &mut w,
        &[
            (
                &[
                    "jaim", "jeim", "aim", "eim", "im", "jaid", "jeid", "aid", "eid", "id", "jai",
                    "jei", "ai", "ei", "i", "jaink", "jeink", "eink", "aink", "ink", "jaitok",
                    "jeitek", "aitok", "eitek", "itek", "jeik", "jaik", "aik", "eik", "ik",
                ],
                "",
            ),
            (&["áim", "áid", "ái", "áink", "áitok", "áik"], "a"),
            (&["éim", "éid", "éi", "éink", "éitek", "éik"], "e"),
        ],
        r1,
    );
    rewrite(&mut w, &[(&["ák"], "a"), (&["ék"], "e"), (&["ök", "ok", "ek", "ak", "k"], "")], r1);
    w.text()
}
