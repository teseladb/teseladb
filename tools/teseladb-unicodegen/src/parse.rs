//! Readers of the official Unicode and CLDR files: `FractionalUCA.txt` (the CLDR root collation in
//! ICU's form), `UnicodeData.txt` (canonical decompositions and combining classes), `PropList.txt`
//! (`Unified_Ideograph`) and `Blocks.txt`.

use teseladb_collation::{COMMON_WEIGHT16, Ce, Tables, implicit_primary};

/// A line that does not read, with its file and line number.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseError(pub String);

fn bad(file: &str, line: usize, what: &str) -> ParseError {
    ParseError(format!("{file}:{line}: {what}"))
}

fn hex(s: &str) -> Option<u32> {
    u32::from_str_radix(s.trim(), 16).ok()
}

/// The lines of a file without comments and blanks, with their numbers.
fn lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate().filter_map(|(i, l)| {
        let l = l.split('#').next().unwrap_or_default().trim();
        (!l.is_empty()).then_some((i + 1, l))
    })
}

/// Bytes such as `29 05` left-aligned in `width` bytes; an empty field is 0.
fn weight(field: &str, width: usize) -> Option<u32> {
    let bytes: Vec<u32> = field.split_whitespace().map(hex).collect::<Option<_>>()?;
    if bytes.len() > width || bytes.iter().any(|b| *b > 0xFF) {
        return None;
    }
    let padded = bytes.iter().copied().chain(std::iter::repeat(0)).take(width);
    Some(padded.fold(0u32, |v, b| (v << 8) | b))
}

/// One `[p, s, t]` element; `[U+4E00, t]` and its forms are the implicit primary of a code point.
fn element(text: &str, t: &Tables) -> Option<Ce> {
    let parts: Vec<&str> = text.split(',').map(str::trim).collect();
    let w16 = |f: &str| weight(f, 2).and_then(|w| u16::try_from(w).ok());
    if let Some(cp) = parts[0].strip_prefix("U+") {
        let primary = implicit_primary(hex(cp)?, &t.unified_ideographs, &t.core_han_blocks);
        let (secondary, tertiary) = match parts[1..] {
            [] => (COMMON_WEIGHT16, COMMON_WEIGHT16),
            [ter] => (COMMON_WEIGHT16, w16(ter)?),
            [sec, ter] => (w16(sec)?, w16(ter)?),
            _ => return None,
        };
        return Some(Ce { primary, secondary, tertiary });
    }
    let [p, s, ter] = parts[..] else { return None };
    Some(Ce { primary: weight(p, 4)?, secondary: w16(s)?, tertiary: w16(ter)? })
}

/// `FractionalUCA.txt`, ICU's form of the CLDR root collation: its version, its variable top and
/// every mapping, single code points, contractions and mappings in context (`L | ·`, ICU's
/// prefixes) apart. Needs the ideograph ranges
/// already read, for the implicit primaries it refers to. Returns the elements of the `FDD0 …`
/// lines, which belong to the root's weights without being any string's.
pub fn fractional_uca(text: &str, t: &mut Tables) -> Result<Vec<Ce>, ParseError> {
    const FILE: &str = "FractionalUCA.txt";
    let mut in_context = Vec::new();
    let mut homeless = Vec::new();
    for (n, line) in lines(text.trim_start_matches('\u{FEFF}')) {
        if let Some(d) = line.strip_prefix('[') {
            if let Some(v) = d.strip_prefix("UCA version = ") {
                t.version = v.trim_end_matches(']').trim().to_owned();
            } else if let Some(v) = d.strip_prefix("variable top = ") {
                let top = weight(v.trim_end_matches(']'), 4);
                t.variable_top = top.ok_or_else(|| bad(FILE, n, "variable top"))?;
            }
            continue;
        }
        let (cps, weights) = line.split_once(';').ok_or_else(|| bad(FILE, n, "no ';'"))?;
        let code_points = |s: &str| s.split_whitespace().map(hex).collect::<Option<Vec<u32>>>();
        let (context, cps) = match cps.split_once('|') {
            Some((c, s)) => (code_points(c), code_points(s)),
            None => (Some(Vec::new()), code_points(cps)),
        };
        let (Some(context), Some(cps)) = (context, cps) else {
            return Err(bad(FILE, n, "code points"));
        };
        let ces: Vec<Ce> = weights
            .split('[')
            .skip(1)
            .map(|e| e.trim().strip_suffix(']').and_then(|e| element(e, t)))
            .collect::<Option<_>>()
            .ok_or_else(|| bad(FILE, n, "an element"))?;
        if cps.is_empty() || ces.is_empty() {
            return Err(bad(FILE, n, "a mapping without code points or elements"));
        }
        // As ICU's genuca: contractions starting with U+FDD0 only give the root its weights
        // (they are no mappings), and `FDD1 FDD0` only marks where unassigned code points start.
        if cps.len() > 1 && cps[0] == 0xFDD0 {
            if cps == [0xFDD0, 0x34] {
                t.numeric_primary = ces[0].primary;
            }
            homeless.extend(ces);
            continue;
        }
        if cps == [0xFDD1, 0xFDD0] {
            continue;
        }
        if !context.is_empty() {
            in_context.push((context, cps, ces));
            continue;
        }
        push(t, cps, &ces);
    }
    for (context, cps, ces) in in_context {
        let start = u32::try_from(t.ces.len()).expect("fits");
        let len = u8::try_from(ces.len()).expect("short expansions");
        t.ces.extend(ces);
        t.contexts.push((context, cps, start, len));
    }
    t.singles.sort_unstable();
    t.contractions.sort_unstable();
    t.contexts.sort_unstable();
    Ok(homeless)
}

fn push(t: &mut Tables, cps: Vec<u32>, ces: &[Ce]) {
    let start = u32::try_from(t.ces.len()).expect("fits");
    let len = u8::try_from(ces.len()).expect("short expansions");
    t.ces.extend_from_slice(ces);
    if cps.len() == 1 {
        t.singles.push((cps[0], start, len));
    } else {
        t.contractions.push((cps, start, len));
    }
}

/// `UnicodeData.txt`: canonical decompositions (without a `<tag>`) and combining classes.
pub fn unicode_data(text: &str, t: &mut Tables) -> Result<(), ParseError> {
    const FILE: &str = "UnicodeData.txt";
    for (n, line) in lines(text) {
        let f: Vec<&str> = line.split(';').collect();
        if f.len() < 6 {
            return Err(bad(FILE, n, "fewer than 6 fields"));
        }
        let cp = hex(f[0]).ok_or_else(|| bad(FILE, n, "code point"))?;
        let class: u8 = f[3].parse().map_err(|_| bad(FILE, n, "combining class"))?;
        if class != 0 {
            t.combining.push((cp, class));
        }
        if f[2] == "Nd" {
            let value = f.get(6).and_then(|v| v.parse::<u8>().ok());
            t.digits.push((cp, value.ok_or_else(|| bad(FILE, n, "a digit without its value"))?));
        }
        let d = f[5].trim();
        if !d.is_empty() && !d.starts_with('<') {
            let to = d
                .split_whitespace()
                .map(hex)
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| bad(FILE, n, "decomposition"))?;
            t.decompositions.push((cp, to));
        }
    }
    t.combining.sort_unstable();
    t.decompositions.sort_unstable();
    t.digits.sort_unstable();
    Ok(())
}

/// A `LO..HI` or `CP` range.
fn range(s: &str) -> Option<(u32, u32)> {
    match s.split_once("..") {
        Some((lo, hi)) => Some((hex(lo)?, hex(hi)?)),
        None => hex(s).map(|cp| (cp, cp)),
    }
}

/// `PropList.txt`: the ranges of `Unified_Ideograph`.
pub fn prop_list(text: &str, t: &mut Tables) -> Result<(), ParseError> {
    for (n, line) in lines(text) {
        let (r, prop) = line.split_once(';').ok_or_else(|| bad("PropList.txt", n, "no ';'"))?;
        if prop.trim() == "Unified_Ideograph" {
            t.unified_ideographs.push(range(r).ok_or_else(|| bad("PropList.txt", n, "range"))?);
        }
    }
    t.unified_ideographs.sort_unstable();
    Ok(())
}

/// `Blocks.txt`: the blocks whose ideographs take the first implicit base (UCA §10.1.3).
pub fn blocks(text: &str, t: &mut Tables) -> Result<(), ParseError> {
    for (n, line) in lines(text) {
        let (r, name) = line.split_once(';').ok_or_else(|| bad("Blocks.txt", n, "no ';'"))?;
        if matches!(name.trim(), "CJK Unified Ideographs" | "CJK Compatibility Ideographs") {
            t.core_han_blocks.push(range(r).ok_or_else(|| bad("Blocks.txt", n, "range"))?);
        }
    }
    t.core_han_blocks.sort_unstable();
    Ok(())
}

/// The code of a script marker's sample character for ICU's special groups.
fn special_group(cp: u32) -> Option<&'static str> {
    Some(match cp {
        0x00A0 => "space",
        0x201C => "punct",
        0x263A => "symbol",
        0x20AC => "currency",
        0x0034 => "digit",
        _ => return None,
    })
}

/// The reorder groups, from the root's script markers (`FDD1 x`, already read) and the scripts
/// of `Scripts.txt` with their ISO 15924 codes from `PropertyValueAliases.txt`; equivalent codes
/// as ICU's (`Hrkt` with Hiragana and Katakana, `Hans` and `Hant` with Han) and `Zzzz` for the
/// unassigned code points, whose group starts at `unassigned_start`.
pub fn groups(
    scripts: &str,
    aliases: &str,
    unassigned_start: u32,
    t: &mut Tables,
) -> Result<(), ParseError> {
    let mut short = std::collections::HashMap::new();
    for (n, line) in lines(aliases) {
        let f: Vec<&str> = line.split(';').map(str::trim).collect();
        if f[0] == "sc" {
            let [_, code, long, ..] = f[..] else {
                return Err(bad("PropertyValueAliases.txt", n, "sc"));
            };
            short.insert(long.to_owned(), code.to_owned());
        }
    }
    let mut ranges = Vec::new();
    for (n, line) in lines(scripts) {
        let (r, name) = line.split_once(';').ok_or_else(|| bad("Scripts.txt", n, "no ';'"))?;
        let r = range(r).ok_or_else(|| bad("Scripts.txt", n, "range"))?;
        let code =
            short.get(name.trim()).ok_or_else(|| bad("Scripts.txt", n, "an unknown script"))?;
        ranges.push((r, code.clone()));
    }
    let script = |cp: u32| {
        ranges.iter().find(|((lo, hi), _)| (*lo..=*hi).contains(&cp)).map(|(_, c)| c.clone())
    };
    let mut groups: Vec<(u32, Vec<String>)> = Vec::new();
    for (cps, start, _) in &t.contractions {
        let [0xFDD1, x] = cps[..] else { continue };
        let code = match special_group(x) {
            Some(s) => s.to_owned(),
            None => script(x)
                .ok_or_else(|| ParseError(format!("FractionalUCA.txt: no script for U+{x:04X}")))?,
        };
        let mut codes = vec![code.clone()];
        match code.as_str() {
            "Hira" | "Kana" => codes.push("Hrkt".into()),
            "Hani" => codes.extend(["Hans".into(), "Hant".into()]),
            _ => {}
        }
        let primary = t.ces[*start as usize].primary;
        match groups.iter_mut().find(|(p, _)| *p == primary) {
            Some((_, existing)) => {
                for c in codes {
                    if !existing.contains(&c) {
                        existing.push(c);
                    }
                }
            }
            None => groups.push((primary, codes)),
        }
    }
    groups.push((unassigned_start, vec!["Zzzz".into()]));
    groups.sort();
    t.groups = groups;
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_file_reads_and_refuses_what_is_not_its_format() {
        let mut t = Tables {
            unified_ideographs: vec![(0x4E00, 0x9FD5)],
            core_han_blocks: vec![(0x4E00, 0x9FFF)],
            ..Tables::default()
        };
        let homeless = fractional_uca(
            "\u{FEFF}[UCA version = 8.0.0]\n[variable top = 0B FF FF FF]\n[first regular [0C 04, 05, 05]]\n\
             0041; [29, 05, 9C]\t# A\n0020; [03 05, 05, 05]\n004C; [3D, 05, 9C]\n\
             004C | 00B7; [, FB 50, 05]\n0000; [,,]\n3220; [09 8C, 05, 17][U+4E00, 10][U+4E8C][U+4E09, 70, 05]\n\
             FDD0 0041; [, 90, 20]\nFDD1 FDD0; [E4, 05, 05]\nFDD1 00A0; [03 02 02, 05, 05]\n",
            &mut t,
        )
        .unwrap();
        assert_eq!((t.version.as_str(), t.variable_top), ("8.0.0", 0x0BFF_FFFF));
        let ce = |primary, secondary, tertiary| Ce { primary, secondary, tertiary };
        assert_eq!(homeless, [ce(0, 0x9000, 0x2000)]);
        assert_eq!(
            t.singles,
            [(0, 3, 1), (0x20, 1, 1), (0x41, 0, 1), (0x4C, 2, 1), (0x3220, 4, 4)]
        );
        assert_eq!(t.contractions, [(vec![0xFDD1, 0xA0], 8, 1)]);
        assert_eq!(t.contexts, [(vec![0x4C], vec![0xB7], 9, 1)]);
        assert_eq!(t.ces[0], ce(0x2900_0000, 0x0500, 0x9C00));
        assert_eq!(t.ces[1], ce(0x0305_0000, 0x0500, 0x0500));
        assert_eq!(t.ces[3], ce(0, 0, 0));
        let han = |cp| implicit_primary(cp, &t.unified_ideographs, &t.core_han_blocks);
        assert_eq!(
            t.ces[4..8],
            [
                ce(0x098C_0000, 0x0500, 0x1700),
                ce(han(0x4E00), 0x0500, 0x1000),
                ce(han(0x4E8C), 0x0500, 0x0500),
                ce(han(0x4E09), 0x7000, 0x0500),
            ]
        );
        assert_eq!(t.ces[9], ce(0, 0xFB50, 0x0500));
        for bad_line in [
            " ; [29, 05, 05]",
            "0041 [29, 05, 05]",
            "zz ; [29, 05, 05]",
            "0041 | zz; [29, 05, 05]",
            "0041; [29, 05, 05",
            "0041; [29, 05]",
            "0041; [xx, 05, 05]",
            "0041; [29 05 05 05 05, 05, 05]",
            "0041; [129, 05, 05]",
            "0041; [29, 05 05 05, 05]",
            "0041; [U+zz]",
            "0041; [U+4E00, 1, 2, 3]",
            "0041; [U+4E00, xx]",
            "0041;",
            "[variable top = zz]",
        ] {
            assert!(fractional_uca(bad_line, &mut Tables::default()).is_err(), "{bad_line}");
        }
        let mut t = Tables::default();
        unicode_data(
            "00C0;LATIN CAPITAL LETTER A WITH GRAVE;Lu;0;L;0041 0300;;;;N;;;;00E0;\n\
             0300;COMBINING GRAVE ACCENT;Mn;230;NSM;;;;;N;;;;;\n00BD;VULGAR FRACTION ONE HALF;No;0;ON;<fraction> 0031 2044;;;;N;;;;;\n",
            &mut t,
        )
        .unwrap();
        assert_eq!(t.decompositions, [(0xC0, vec![0x41, 0x300])]);
        assert_eq!(t.combining, [(0x300, 230)]);
        for bad_line in ["0041;A;Lu", "zz;A;Lu;0;L;;", "0041;A;Lu;x;L;;", "0041;A;Lu;0;L;zz 1;"] {
            assert!(unicode_data(bad_line, &mut Tables::default()).is_err(), "{bad_line}");
        }
        prop_list("4E00..9FD5    ; Unified_Ideograph # Lo\nFA0E ; Unified_Ideograph\n0020 ; White_Space\n", &mut t).unwrap();
        assert_eq!(t.unified_ideographs, [(0x4E00, 0x9FD5), (0xFA0E, 0xFA0E)]);
        assert!(prop_list("4E00", &mut Tables::default()).is_err());
        assert!(prop_list("zz ; Unified_Ideograph", &mut Tables::default()).is_err());
        blocks("4E00..9FFF; CJK Unified Ideographs\nF900..FAFF; CJK Compatibility Ideographs\n0000..007F; Basic Latin\n", &mut t).unwrap();
        assert_eq!(t.core_han_blocks, [(0x4E00, 0x9FFF), (0xF900, 0xFAFF)]);
        assert!(blocks("4E00..9FFF", &mut Tables::default()).is_err());
        assert!(blocks("zz; CJK Unified Ideographs", &mut Tables::default()).is_err());
        // Reorder groups: markers, specials, equivalents, the unassigned group, and bad lines.
        let aliases =
            "sc ; Latn ; Latin\nsc ; Hira ; Hiragana\nsc ; Kana ; Katakana\nsc ; Hani ; Han\n";
        let scripts = "0041..005A ; Latin\n3041 ; Hiragana\n30AB ; Katakana\n4E00 ; Han\n";
        let ce = |p| Ce { primary: p, secondary: 0x0500, tertiary: 0x0500 };
        let mut t = Tables {
            ces: vec![
                ce(0x0302_0200),
                ce(0x2802_0200),
                ce(0x7904_0200),
                ce(0x7904_0200),
                ce(0x7D02_0200),
            ],
            contractions: vec![
                (vec![0xFDD1, 0xA0], 0, 1),
                (vec![0xFDD1, 0x41], 1, 1),
                (vec![0xFDD1, 0x3041], 2, 1),
                (vec![0xFDD1, 0x30AB], 3, 1),
                (vec![0xFDD1, 0x4E00], 4, 1),
                (vec![0x41, 0x42], 1, 1),
            ],
            ..Tables::default()
        };
        groups(scripts, aliases, 0xE500_0000, &mut t).unwrap();
        let codes: Vec<(u32, Vec<&str>)> =
            t.groups.iter().map(|(p, c)| (*p, c.iter().map(String::as_str).collect())).collect();
        assert_eq!(
            codes,
            [
                (0x0302_0200, vec!["space"]),
                (0x2802_0200, vec!["Latn"]),
                (0x7904_0200, vec!["Hira", "Hrkt", "Kana"]),
                (0x7D02_0200, vec!["Hani", "Hans", "Hant"]),
                (0xE500_0000, vec!["Zzzz"]),
            ]
        );
        for (scripts, aliases) in [
            ("0041 ; Latin", "sc ; Latn"),
            ("0041 Latin", "sc ; Latn ; Latin"),
            ("zz ; Latin", "sc ; Latn ; Latin"),
            ("0041 ; Klingon", "sc ; Latn ; Latin"),
            ("0042 ; Latin", "sc ; Latn ; Latin"),
        ] {
            let mut t = Tables {
                ces: vec![ce(0x2802_0200)],
                contractions: vec![(vec![0xFDD1, 0x41], 0, 1)],
                ..Tables::default()
            };
            assert!(groups(scripts, aliases, 0, &mut t).is_err(), "{scripts} / {aliases}");
        }
        // A decimal digit without its value.
        assert!(
            unicode_data("0030;DIGIT ZERO;Nd;0;EN;;;;;N;;;;;", &mut Tables::default()).is_err()
        );
        let mut t = Tables::default();
        unicode_data("0030;DIGIT ZERO;Nd;0;EN;;0;0;0;N;;;;;", &mut t).unwrap();
        assert_eq!(t.digits, [(0x30, 0)]);
    }
}
