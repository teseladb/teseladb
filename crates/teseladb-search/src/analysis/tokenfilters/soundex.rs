//! Daitch-Mokotoff Soundex, from the rules and coding chart JewishGen publishes
//! (<https://www.jewishgen.org/InfoFiles/soundex.html>, read on 30-10-2026), with the points the
//! chart leaves open measured on the reference search server 1.70.6.
//!
//! The letters of a token are read left to right, taking at each step the longest chart entry
//! that matches. An entry has a code for the start of the token (its first entry, after any
//! skipped letters), one for when a vowel follows and one for any other place; a code may be
//! empty (not coded), and an entry that "may sound in one of two ways" has two codes, so the
//! token branches. Within a branch a code the previous code ends with is not written again (`CK`
//! as 45 followed by `G` gives 45, `RS` as 94 followed by `TS` gives 94, `XXX` gives 5 and 54,
//! while `R` followed by `RS` as 94 gives 9 and 94), except in `MN` and `NM`, also with skipped
//! letters between them; a letter not coded ends the run. Characters that are not letters are
//! dropped, and letters the chart does not list are folded or skipped as [`FOLD`] says (`ß` is
//! one `S`, the dotless `ı` is skipped). As measured, `Þ` codes 7, `UE` codes like `UI`, `MN` and
//! `NM` are 66 also at the start, and the Romanian `ț` (comma below) codes like `ţ` (cedilla).
//! Each branch gives six digits, cut or padded with zeros; the filter emits each distinct code
//! once.

/// Codes of one chart entry: at the start, before a vowel, elsewhere. Each holds one or two
/// alternatives; an empty string is "not coded".
type Codes = [&'static [&'static str]; 3];

/// The chart: each spelling (alternates spelled out) with its codes, longest spellings first
/// where one is a prefix of another.
const CHART: &[(&str, Codes)] = &[
    ("SCHTSCH", [&["2"], &["4"], &["4"]]),
    ("SCHTSH", [&["2"], &["4"], &["4"]]),
    ("SCHTCH", [&["2"], &["4"], &["4"]]),
    ("SHTSCH", [&["2"], &["4"], &["4"]]),
    ("ZHDZH", [&["2"], &["4"], &["4"]]),
    ("SHTCH", [&["2"], &["4"], &["4"]]),
    ("SHTSH", [&["2"], &["4"], &["4"]]),
    ("STSCH", [&["2"], &["4"], &["4"]]),
    ("TTSCH", [&["4"], &["4"], &["4"]]),
    ("SHCH", [&["2"], &["4"], &["4"]]),
    ("SCHT", [&["2"], &["43"], &["43"]]),
    ("SCHD", [&["2"], &["43"], &["43"]]),
    ("STCH", [&["2"], &["4"], &["4"]]),
    ("STRZ", [&["2"], &["4"], &["4"]]),
    ("STRS", [&["2"], &["4"], &["4"]]),
    ("STSH", [&["2"], &["4"], &["4"]]),
    ("SZCZ", [&["2"], &["4"], &["4"]]),
    ("SZCS", [&["2"], &["4"], &["4"]]),
    ("TTCH", [&["4"], &["4"], &["4"]]),
    ("TSCH", [&["4"], &["4"], &["4"]]),
    ("TTSZ", [&["4"], &["4"], &["4"]]),
    ("ZDZH", [&["2"], &["4"], &["4"]]),
    ("ZSCH", [&["4"], &["4"], &["4"]]),
    ("CHS", [&["5"], &["54"], &["54"]]),
    ("CSZ", [&["4"], &["4"], &["4"]]),
    ("CZS", [&["4"], &["4"], &["4"]]),
    ("DRZ", [&["4"], &["4"], &["4"]]),
    ("DRS", [&["4"], &["4"], &["4"]]),
    ("DSH", [&["4"], &["4"], &["4"]]),
    ("DSZ", [&["4"], &["4"], &["4"]]),
    ("DZH", [&["4"], &["4"], &["4"]]),
    ("DZS", [&["4"], &["4"], &["4"]]),
    ("SCH", [&["4"], &["4"], &["4"]]),
    ("SHT", [&["2"], &["43"], &["43"]]),
    ("SZT", [&["2"], &["43"], &["43"]]),
    ("SHD", [&["2"], &["43"], &["43"]]),
    ("SZD", [&["2"], &["43"], &["43"]]),
    ("TCH", [&["4"], &["4"], &["4"]]),
    ("TRZ", [&["4"], &["4"], &["4"]]),
    ("TRS", [&["4"], &["4"], &["4"]]),
    ("TSH", [&["4"], &["4"], &["4"]]),
    ("TTS", [&["4"], &["4"], &["4"]]),
    ("TTZ", [&["4"], &["4"], &["4"]]),
    ("TZS", [&["4"], &["4"], &["4"]]),
    ("TSZ", [&["4"], &["4"], &["4"]]),
    ("ZDZ", [&["2"], &["4"], &["4"]]),
    ("ZHD", [&["2"], &["43"], &["43"]]),
    ("ZSH", [&["4"], &["4"], &["4"]]),
    ("AI", [&["0"], &["1"], &[""]]),
    ("AJ", [&["0"], &["1"], &[""]]),
    ("AY", [&["0"], &["1"], &[""]]),
    ("AU", [&["0"], &["7"], &[""]]),
    ("CH", [&["5", "4"], &["5", "4"], &["5", "4"]]),
    ("CK", [&["5", "45"], &["5", "45"], &["5", "45"]]),
    ("CZ", [&["4"], &["4"], &["4"]]),
    ("CS", [&["4"], &["4"], &["4"]]),
    ("DS", [&["4"], &["4"], &["4"]]),
    ("DZ", [&["4"], &["4"], &["4"]]),
    ("DT", [&["3"], &["3"], &["3"]]),
    ("EI", [&["0"], &["1"], &[""]]),
    ("EJ", [&["0"], &["1"], &[""]]),
    ("EY", [&["0"], &["1"], &[""]]),
    ("EU", [&["1"], &["1"], &[""]]),
    ("FB", [&["7"], &["7"], &["7"]]),
    ("IA", [&["1"], &[""], &[""]]),
    ("IE", [&["1"], &[""], &[""]]),
    ("IO", [&["1"], &[""], &[""]]),
    ("IU", [&["1"], &[""], &[""]]),
    ("KS", [&["5"], &["54"], &["54"]]),
    ("KH", [&["5"], &["5"], &["5"]]),
    ("MN", [&["66"], &["66"], &["66"]]),
    ("NM", [&["66"], &["66"], &["66"]]),
    ("OI", [&["0"], &["1"], &[""]]),
    ("OJ", [&["0"], &["1"], &[""]]),
    ("OY", [&["0"], &["1"], &[""]]),
    ("PF", [&["7"], &["7"], &["7"]]),
    ("PH", [&["7"], &["7"], &["7"]]),
    ("RZ", [&["94", "4"], &["94", "4"], &["94", "4"]]),
    ("RS", [&["94", "4"], &["94", "4"], &["94", "4"]]),
    ("SH", [&["4"], &["4"], &["4"]]),
    ("SC", [&["2"], &["4"], &["4"]]),
    ("ST", [&["2"], &["43"], &["43"]]),
    ("SZ", [&["4"], &["4"], &["4"]]),
    ("SD", [&["2"], &["43"], &["43"]]),
    ("TH", [&["3"], &["3"], &["3"]]),
    ("TS", [&["4"], &["4"], &["4"]]),
    ("TC", [&["4"], &["4"], &["4"]]),
    ("TZ", [&["4"], &["4"], &["4"]]),
    ("UI", [&["0"], &["1"], &[""]]),
    ("UJ", [&["0"], &["1"], &[""]]),
    ("UY", [&["0"], &["1"], &[""]]),
    ("UE", [&["0"], &["1"], &[""]]),
    ("ZD", [&["2"], &["43"], &["43"]]),
    ("ZH", [&["4"], &["4"], &["4"]]),
    ("ZS", [&["4"], &["4"], &["4"]]),
    ("A", [&["0"], &[""], &[""]]),
    ("Ą", [&[""], &[""], &["6", ""]]),
    ("B", [&["7"], &["7"], &["7"]]),
    ("C", [&["5", "4"], &["5", "4"], &["5", "4"]]),
    ("D", [&["3"], &["3"], &["3"]]),
    ("E", [&["0"], &[""], &[""]]),
    ("Ę", [&[""], &[""], &["6", ""]]),
    ("F", [&["7"], &["7"], &["7"]]),
    ("G", [&["5"], &["5"], &["5"]]),
    ("H", [&["5"], &["5"], &[""]]),
    ("I", [&["0"], &[""], &[""]]),
    ("J", [&["1", "4"], &["", "4"], &["", "4"]]),
    ("K", [&["5"], &["5"], &["5"]]),
    ("L", [&["8"], &["8"], &["8"]]),
    ("M", [&["6"], &["6"], &["6"]]),
    ("N", [&["6"], &["6"], &["6"]]),
    ("O", [&["0"], &[""], &[""]]),
    ("P", [&["7"], &["7"], &["7"]]),
    ("Q", [&["5"], &["5"], &["5"]]),
    ("R", [&["9"], &["9"], &["9"]]),
    ("S", [&["4"], &["4"], &["4"]]),
    ("T", [&["3"], &["3"], &["3"]]),
    ("Ţ", [&["3", "4"], &["3", "4"], &["3", "4"]]),
    ("Ț", [&["3", "4"], &["3", "4"], &["3", "4"]]),
    ("U", [&["0"], &[""], &[""]]),
    ("V", [&["7"], &["7"], &["7"]]),
    ("W", [&["7"], &["7"], &["7"]]),
    ("X", [&["5"], &["54"], &["54"]]),
    ("Y", [&["1"], &[""], &[""]]),
    ("Z", [&["4"], &["4"], &["4"]]),
    ("Þ", [&["7"], &["7"], &["7"]]),
];

/// Letters read as other letters, as measured: accented Latin letters as their base letter and a
/// few others. A letter neither here nor in the chart is skipped: it codes nothing, keeps two
/// letters around it from forming one chart entry and leaves the previous code in place.
const FOLD: &[(char, &str)] = &[
    ('À', "A"),
    ('Á', "A"),
    ('Â', "A"),
    ('Ã', "A"),
    ('Ä', "A"),
    ('Å', "A"),
    ('Æ', "AE"),
    ('Ç', "C"),
    ('È', "E"),
    ('É', "E"),
    ('Ê', "E"),
    ('Ë', "E"),
    ('Ì', "I"),
    ('Í', "I"),
    ('Î', "I"),
    ('Ï', "I"),
    ('Ð', "D"),
    ('Ñ', "N"),
    ('Ò', "O"),
    ('Ó', "O"),
    ('Ô', "O"),
    ('Õ', "O"),
    ('Ö', "O"),
    ('Ø', "O"),
    ('Ù', "U"),
    ('Ú', "U"),
    ('Û', "U"),
    ('Ý', "Y"),
    ('Ī', "I"),
    ('Ł', "L"),
    ('Ō', "O"),
    ('Ś', "S"),
    ('Ű', "U"),
    ('Ÿ', "Y"),
    ('Ż', "Z"),
    ('Ẽ', "E"),
];

/// The letters that count as a vowel when choosing an entry's code.
const VOWELS: &[char] = &['A', 'E', 'I', 'O', 'U'];

/// Digits in a code.
const CODE_LENGTH: usize = 6;

/// Most branches followed for one token, so a long token of two-way letters stays bounded.
const MAX_BRANCHES: usize = 1 << 10;

/// One branch: the code so far and the last code read, empty after a letter not coded.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Branch {
    code: String,
    last: &'static str,
}

/// The letters the coder reads, in uppercase and folded by [`FOLD`]; other characters are
/// dropped.
fn letters(text: &str) -> Vec<char> {
    let mut out = Vec::with_capacity(text.len());
    let upper = text.chars().flat_map(|c| match c {
        // One `S`, not the two of its uppercase; the dotless `ı` is not an `I`.
        'ß' => vec!['S'],
        'ı' => vec!['ı'],
        c => c.to_uppercase().collect(),
    });
    for c in upper.filter(|c| c.is_alphabetic()) {
        match FOLD.iter().find(|&&(f, _)| f == c) {
            Some((_, to)) => out.extend(to.chars()),
            None => out.push(c),
        }
    }
    out
}

/// The longest chart entry at the start of `rest`.
fn entry(rest: &[char]) -> Option<(usize, &'static Codes)> {
    CHART.iter().find_map(|(spelling, codes)| {
        let n = spelling.chars().count();
        (rest.len() >= n && spelling.chars().zip(rest).all(|(a, &b)| a == b)).then_some((n, codes))
    })
}

/// The Daitch-Mokotoff codes of `text`.
pub(super) fn daitch_mokotoff(text: &str) -> Vec<String> {
    let letters = letters(text);
    let mut branches = vec![Branch { code: String::new(), last: "" }];
    // The last letter of the previous chart entry: skipped letters do not change it.
    let mut previous: Option<char> = None;
    let mut i = 0;
    while i < letters.len() {
        let Some((n, codes)) = entry(&letters[i..]) else {
            i += 1;
            continue;
        };
        let place = if previous.is_none() {
            0
        } else if letters.get(i + n).is_some_and(|c| VOWELS.contains(c)) {
            1
        } else {
            2
        };
        let alternatives = codes[place];
        let mut next = Vec::with_capacity(branches.len() * alternatives.len());
        for b in &branches {
            for &code in alternatives.iter().take(MAX_BRANCHES / branches.len().max(1)) {
                let separate = matches!(&letters[i..i + n], ['M', 'N'] | ['N', 'M'])
                    || matches!((previous, letters[i]), (Some('M'), 'N') | (Some('N'), 'M'));
                let mut branch = Branch { code: b.code.clone(), last: code };
                if separate || !b.last.ends_with(code) {
                    branch.code.push_str(code);
                }
                if !next.contains(&branch) {
                    next.push(branch);
                }
            }
        }
        branches = next;
        previous = Some(letters[i + n - 1]);
        i += n;
    }
    let mut out: Vec<String> = Vec::new();
    for b in branches {
        let mut code: String = b.code.chars().take(CODE_LENGTH).collect();
        while code.len() < CODE_LENGTH {
            code.push('0');
        }
        if !out.contains(&code) {
            out.push(code);
        }
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn chart_examples() {
        for (name, code) in [
            ("Auerbach", "097500"),
            ("Ohrbach", "097500"),
            ("Lipshitz", "874400"),
            ("Lippszyc", "874400"),
            ("Lewinsky", "876450"),
            ("Levinski", "876450"),
            ("Szlamawicz", "486740"),
            ("Shlamovitz", "486740"),
            ("Golden", "583600"),
            ("Alpert", "087930"),
            ("Breuer", "791900"),
            ("Haber", "579000"),
            ("Manheim", "665600"),
            ("Mintz", "664000"),
            ("Topf", "370000"),
            ("Kleinman", "586660"),
        ] {
            assert!(daitch_mokotoff(name).contains(&code.to_string()), "{name}");
        }
        assert_eq!(daitch_mokotoff("Peters"), ["739400", "734000"]);
        assert_eq!(daitch_mokotoff("123"), ["000000"]);
    }
}
