//! ICU 57.1's own collation tests (`collationtest.txt`, the oracle of the tailoring builder): each
//! `@ rules` or `@ locale` section builds a collator, each `* compare` line says whether the
//! string sorts after the previous one or equals it, and at which level they differ. Read as
//! ICU's `CollationTest::TestDataDriven` reads it, attributes (`%`) and locale keywords included.

use std::fs;
use std::path::{Path, PathBuf};

use teseladb_collation::{
    Alternate, CaseFirst, Collation, MaxVariable, Reordering, Root, Strength, Tailored,
};
use teseladb_unicodegen::builder::{Builder, Tailoring};
use teseladb_unicodegen::root_elements::RootElements;
use teseladb_unicodegen::{build_root, han_first_primary, icu_collations};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// ICU's `UnicodeString::unescape`: `\uXXXX`, `\UXXXXXXXX`, `\xH`, `\xHH`, `\x{H…}`, octal, C
/// escapes, and `\c` for any other `c`.
fn unescape(s: &str) -> Vec<u32> {
    let c: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] != '\\' || i + 1 == c.len() {
            out.push(u32::from(c[i]));
            i += 1;
            continue;
        }
        i += 1;
        let hex = |from: usize, max: usize| {
            let digits: String =
                c[from..].iter().take(max).take_while(|d| d.is_ascii_hexdigit()).collect();
            (u32::from_str_radix(&digits, 16).ok(), digits.len())
        };
        match c[i] {
            'u' | 'U' => {
                let n = if c[i] == 'u' { 4 } else { 8 };
                let (v, len) = hex(i + 1, n);
                out.push(v.expect("hex digits"));
                i += 1 + len;
            }
            'x' if c.get(i + 1) == Some(&'{') => {
                let (v, len) = hex(i + 2, 8);
                out.push(v.expect("hex digits"));
                i += 3 + len;
            }
            'x' => {
                let (v, len) = hex(i + 1, 2);
                out.push(v.expect("hex digits"));
                i += 1 + len;
            }
            '0'..='7' => {
                let digits: String =
                    c[i..].iter().take(3).take_while(|d| ('0'..='7').contains(d)).collect();
                out.push(u32::from_str_radix(&digits, 8).expect("octal"));
                i += digits.len();
            }
            e => {
                let v = match e {
                    'a' => 7,
                    'b' => 8,
                    'e' => 0x1B,
                    'f' => 0xC,
                    'n' => 0xA,
                    'r' => 0xD,
                    't' => 9,
                    'v' => 0xB,
                    other => u32::from(other),
                };
                out.push(v);
                i += 1;
            }
        }
    }
    out
}

fn to_string(cps: &[u32]) -> String {
    cps.iter().filter_map(|cp| char::from_u32(*cp)).collect()
}

/// The lines without comments and trailing spaces, numbered.
fn lines(text: &str) -> Vec<(usize, String)> {
    text.lines()
        .enumerate()
        .filter_map(|(n, l)| {
            let l = l.trim_start_matches('\u{FEFF}');
            let l = l.split('#').next().unwrap_or_default().trim_end();
            (!l.is_empty()).then(|| (n + 1, l.to_owned()))
        })
        .collect()
}

/// The collator of a section: its tailoring (none for the root), the options it starts with, the
/// options the attributes changed, and its script reordering codes.
struct Coll {
    tailoring: Option<Tailoring>,
    base: Collation,
    now: Collation,
    base_reorder: Vec<String>,
    reorder: Vec<String>,
}

impl Coll {
    fn new(tailoring: Option<Tailoring>) -> Coll {
        let s = tailoring.as_ref().map(|t| t.settings.clone()).unwrap_or_default();
        let d = Collation::default();
        let base = Collation {
            strength: s.strength.unwrap_or(d.strength),
            alternate: s.alternate.unwrap_or(d.alternate),
            case_first: s.case_first.unwrap_or(d.case_first),
            case_level: s.case_level.unwrap_or(d.case_level),
            numeric_ordering: s.numeric_ordering.unwrap_or(d.numeric_ordering),
            max_variable: s.max_variable.unwrap_or(d.max_variable),
            normalization: s.normalization.unwrap_or(d.normalization),
            backwards: s.backwards,
            tailoring: None,
        };
        let reorder = s.reorder.unwrap_or_default();
        Coll { tailoring, base, now: base, base_reorder: reorder.clone(), reorder }
    }

    /// Applies one attribute (`name=value`), as ICU's `setAttribute`.
    fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let (b, now) = (self.base, &mut self.now);
        let v = value.to_ascii_lowercase();
        let on = |v: &str, default: bool| match v {
            "on" | "yes" | "true" => Ok(true),
            "off" | "no" | "false" => Ok(false),
            "default" => Ok(default),
            other => Err(format!("value {other}")),
        };
        match name.to_ascii_lowercase().as_str() {
            "strength" | "colstrength" | "ks" => {
                now.strength = match v.as_str() {
                    "primary" | "level1" => Strength::Primary,
                    "secondary" | "level2" => Strength::Secondary,
                    "tertiary" | "level3" => Strength::Tertiary,
                    "quaternary" | "level4" => Strength::Quaternary,
                    "identical" | "identic" => Strength::Identical,
                    "default" => b.strength,
                    other => return Err(format!("strength {other}")),
                }
            }
            "alternate" | "colalternate" | "ka" => {
                now.alternate = match v.as_str() {
                    "shifted" => Alternate::Shifted,
                    "non-ignorable" | "noignore" => Alternate::NonIgnorable,
                    "default" => b.alternate,
                    other => return Err(format!("alternate {other}")),
                }
            }
            "casefirst" | "kf" => {
                now.case_first = match v.as_str() {
                    "upper" => CaseFirst::Upper,
                    "lower" => CaseFirst::Lower,
                    "off" | "false" => CaseFirst::Off,
                    "default" => b.case_first,
                    other => return Err(format!("caseFirst {other}")),
                }
            }
            "maxvariable" | "kv" => {
                now.max_variable = match v.as_str() {
                    "space" => MaxVariable::Space,
                    "punct" => MaxVariable::Punct,
                    "symbol" => MaxVariable::Symbol,
                    "currency" => MaxVariable::Currency,
                    other => return Err(format!("maxVariable {other}")),
                }
            }
            "caselevel" | "kc" => now.case_level = on(&v, b.case_level)?,
            "backwards" | "colbackwards" | "kb" => now.backwards = on(&v, b.backwards)?,
            "numeric" | "kn" => now.numeric_ordering = on(&v, b.numeric_ordering)?,
            "kk" => now.normalization = on(&v, b.normalization)?,
            "kr" => self.set_reorder(&value.split('-').map(str::to_owned).collect::<Vec<_>>()),
            other => return Err(format!("attribute {other}")),
        }
        Ok(())
    }

    fn set_reorder(&mut self, codes: &[String]) {
        self.reorder =
            if codes == ["default"] { self.base_reorder.clone() } else { codes.to_vec() };
    }
}

/// The ICU locale ID of a test's language tag and the attributes it carries in `-u-` or `@`
/// keywords other than the collation type.
fn locale_id(tag: &str) -> (String, Vec<(String, String)>) {
    let mut attrs = Vec::new();
    if let Some((base, keywords)) = tag.split_once('@') {
        let mut id = base.to_owned();
        for kv in keywords.split(';') {
            let (k, v) = kv.split_once('=').unwrap();
            if k.eq_ignore_ascii_case("collation") {
                id = format!("{base}@collation={}", v.to_ascii_lowercase());
            } else {
                attrs.push((k.to_owned(), v.to_owned()));
            }
        }
        return (id, attrs);
    }
    let (base, ext) = tag.split_once("-u-").unwrap_or((tag, ""));
    let mut id = base.replace('-', "_");
    let words: Vec<&str> = ext.split('-').filter(|w| !w.is_empty()).collect();
    let mut i = 0;
    while i < words.len() {
        let key = words[i];
        let mut values = Vec::new();
        i += 1;
        while i < words.len() && words[i].len() > 2 {
            values.push(words[i]);
            i += 1;
        }
        let value = if values.is_empty() { "true".to_owned() } else { values.join("-") };
        if key == "co" {
            let ty = match value.as_str() {
                "phonebk" => "phonebook",
                "trad" => "traditional",
                other => other,
            };
            id = format!("{id}@collation={ty}");
        } else {
            attrs.push((key.to_owned(), value));
        }
    }
    (id, attrs)
}

#[derive(Default)]
struct Report {
    compared: usize,
    failures: Vec<String>,
}

/// The level of the first difference, in ICU's order (1, 2, c, 3, 4, i), at most the collator's.
fn difference_level(key: &dyn Fn(Collation) -> Vec<u8>, c: Collation) -> Option<&'static str> {
    let mut steps: Vec<(&str, Collation)> = vec![
        ("1", Collation { strength: Strength::Primary, case_level: false, ..c }),
        ("2", Collation { strength: Strength::Secondary, case_level: false, ..c }),
    ];
    if c.case_level {
        let s = c.strength.min(Strength::Secondary);
        steps.push(("c", Collation { strength: s, ..c }));
    }
    steps.push(("3", Collation { strength: Strength::Tertiary, ..c }));
    steps.push(("4", Collation { strength: Strength::Quaternary, ..c }));
    steps.push(("i", Collation { strength: Strength::Identical, ..c }));
    let last = steps.iter().position(|(l, _)| match c.strength {
        Strength::Primary => *l == if c.case_level { "c" } else { "1" },
        Strength::Secondary => *l == if c.case_level { "c" } else { "2" },
        Strength::Tertiary => *l == "3",
        Strength::Quaternary => *l == "4",
        Strength::Identical => *l == "i",
    });
    steps.into_iter().take(last.unwrap() + 1).find(|(_, s)| key(*s).is_empty()).map(|(l, _)| l)
}

#[test]
fn icu_57_collation_tests() {
    let root_dir = repo();
    let (tables, homeless) = build_root(&root_dir).unwrap();
    let elements = RootElements::new(&tables, &homeless);
    let variable_top = tables.variable_top;
    let han = han_first_primary(&tables).unwrap();
    let root = Root::new(tables);
    let icu = icu_collations(&root_dir).unwrap();
    let list = fs::read_to_string(root_dir.join("scripts/icu-coll.sha256")).unwrap();
    let dest = list.lines().find_map(|l| l.strip_prefix("# dest ")).unwrap();
    let path = root_dir.join(dest).join("source/test/testdata/collationtest.txt");
    let lines = lines(&fs::read_to_string(path).unwrap());

    let build = |rules: &str| Builder::new(&root, &elements, &icu, variable_top, han).build(rules);
    let mut report = Report::default();
    let mut coll: Option<Coll> = Some(Coll::new(None));
    let mut test_name = String::new();
    let mut i = 0;
    while i < lines.len() {
        let (n, line) = &lines[i];
        i += 1;
        if let Some(name) = line.strip_prefix("** test: ") {
            test_name = name.to_owned();
        } else if line == "@ root" {
            coll = Some(Coll::new(None));
        } else if let Some(tag) = line.strip_prefix("@ locale ") {
            let (id, attrs) = locale_id(tag);
            let r = icu.resolve(&id);
            let (_, rules) = icu.rules(&r.locale, &r.collation).unwrap();
            coll = match build(rules) {
                Ok(t) => {
                    let mut c = Coll::new(Some(t));
                    for (k, v) in &attrs {
                        c.set(k, v).unwrap_or_else(|e| panic!("line {n}: {e}"));
                    }
                    Some(c)
                }
                Err(e) => {
                    report
                        .failures
                        .push(format!("line {n} ({test_name}): {id} does not build: {e}"));
                    None
                }
            };
        } else if line == "@ rules" {
            let mut rules = Vec::new();
            while i < lines.len() && !lines[i].1.starts_with(['%', '*', '@']) {
                rules.extend(unescape(&lines[i].1));
                i += 1;
            }
            coll = match build(&to_string(&rules)) {
                Ok(t) => Some(Coll::new(Some(t))),
                Err(e) => {
                    report
                        .failures
                        .push(format!("line {n} ({test_name}): rules do not build: {e}"));
                    None
                }
            };
        } else if let Some(attr) = line.strip_prefix("% ") {
            let Some(c) = coll.as_mut() else { continue };
            if let Some(codes) = attr.strip_prefix("reorder") {
                c.set_reorder(&codes.split_whitespace().map(str::to_owned).collect::<Vec<_>>());
            } else {
                let (k, v) = attr.split_once('=').unwrap();
                c.set(k, v).unwrap_or_else(|e| panic!("line {n}: {e}"));
            }
        } else if line == "* compare" {
            let mut prev: Option<Vec<u32>> = None;
            while i < lines.len() && !lines[i].1.starts_with(['%', '*', '@']) {
                let (n, l) = &lines[i];
                i += 1;
                let (relation, s) = l.split_once(char::is_whitespace).expect("relation and string");
                let s = unescape(s.trim());
                let previous = prev.replace(s.clone());
                let (Some(previous), Some(c)) = (previous, coll.as_ref()) else { continue };
                let reordering = Reordering::new(&c.reorder, root.groups()).unwrap();
                let tailored = c.tailoring.as_ref().map(|t| {
                    let mut m = Tailored::new(&root);
                    for (lo, hi) in &t.suppressed {
                        m.suppress_contractions(*lo, *hi);
                    }
                    for (before, cps, ces) in &t.mappings {
                        m.insert_context(before, cps, ces.clone());
                    }
                    m
                });
                let key = |col: Collation, cps: &[u32]| match &tailored {
                    Some(m) => col.sort_key_in(m, &root, reordering.as_ref(), cps),
                    None => col.sort_key_in(&root, &root, reordering.as_ref(), cps),
                };
                // As ICU's harness: with normalization, and also without it when neither string
                // needs canonical reordering.
                let needs = |cps: &[u32]| {
                    root.nfd(cps.iter().copied()) != root.decompose(cps.iter().copied())
                };
                let mut modes = vec![true];
                if !needs(&previous) && !needs(&s) {
                    modes.push(false);
                }
                for normalization in modes {
                    let col = Collation { normalization, ..c.now };
                    let order = key(col, &previous).cmp(&key(col, &s));
                    let differs = |k: Collation| -> Vec<u8> {
                        if key(k, &previous) == key(k, &s) { vec![1] } else { Vec::new() }
                    };
                    let level = difference_level(&differs, col);
                    let ok = match relation {
                        "=" => order.is_eq(),
                        "<" => order.is_lt(),
                        r => order.is_lt() && level == r.strip_prefix('<'),
                    };
                    report.compared += 1;
                    if !ok {
                        report.failures.push(format!(
                            "line {n} ({test_name}, normalization {normalization}): {relation} {s:04X?} after {previous:04X?}: order {order:?}, level {level:?}"
                        ));
                    }
                }
            }
        } else {
            panic!("line {n}: unexpected {line}");
        }
    }
    eprintln!("compared {}, failures {}", report.compared, report.failures.len());
    for f in report.failures.iter().take(60) {
        eprintln!("FAIL {f}");
    }
    assert!(report.failures.is_empty(), "{} failures", report.failures.len());
}
