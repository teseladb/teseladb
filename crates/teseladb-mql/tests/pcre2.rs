//! The regular expression engine against PCRE2 10.43's own test data (006 T058, FR-015): every
//! pattern of `testoutput1`, `testoutput4` and `testoutput5` that RefDb can send (only the
//! options `i`, `m`, `s`, `x` and inline equivalents, no `pcre2test` subject modifiers) is compiled
//! and run on its subjects, and the result is compared with the output PCRE2 printed: the match
//! and every capture group, `No match`, or the compile error. The files are BSD-licensed and are
//! not copied into the repository: `scripts/fetch-pinned.sh crates/teseladb-mql/tests/pcre2.sha256`
//! downloads them, checked against their SHA-256.

use std::fmt::Write;
use std::path::PathBuf;

use teseladb_mql::regex::Regex;

/// Where `scripts/fetch-pinned.sh crates/teseladb-mql/tests/pcre2.sha256` puts the files.
fn testdata(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../.tools/pcre2/pcre2-10.43/testdata")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run scripts/fetch-pinned.sh crates/teseladb-mql/tests/pcre2.sha256",
            path.display()
        )
    });
    // The non-UTF files hold some bytes above 0x7F; those cases are not RefDb's (always UTF).
    bytes.iter().map(|&b| char::from(b)).collect()
}

/// What PCRE2 printed for a subject.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The groups printed, in order, `None` for `<unset>`.
    Match(Vec<Option<String>>),
    NoMatch,
    /// A match error such as the limit of steps.
    Error,
}

/// One pattern and what PCRE2 did with it.
struct Case {
    line: usize,
    pattern: String,
    options: String,
    /// The pattern is in UTF mode.
    utf: bool,
    /// `None` when PCRE2 refused the pattern.
    subjects: Option<Vec<(String, Outcome)>>,
}

/// Modifiers of `pcre2test` that RefDb's options or inline settings express, and those that only
/// change what `pcre2test` prints.
fn translate(modifiers: &str) -> Option<(String, String, bool)> {
    let mut options = String::new();
    let mut prefix = String::new();
    let mut utf = false;
    for m in modifiers.split(',').filter(|m| !m.is_empty()) {
        match m {
            "i" | "m" | "s" | "x" => options.push_str(m),
            "xx" => prefix.push_str("(?xx)"),
            "n" | "no_auto_capture" => prefix.push_str("(?n)"),
            "dupnames" => prefix.push_str("(?J)"),
            "ungreedy" => prefix.push_str("(?U)"),
            "utf" => utf = true,
            "mark" | "aftertext" => {}
            flags if flags.chars().all(|c| "imsx".contains(c)) => options.push_str(flags),
            _ => return None,
        }
    }
    Some((options, prefix, utf))
}

/// A subject line of `pcre2test` with its escapes decoded; `None` if it has subject modifiers.
fn subject(line: &str, utf: bool) -> Option<String> {
    let chars: Vec<char> = line.trim().chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let hex = |s: &[char]| u32::from_str_radix(&s.iter().collect::<String>(), 16).ok();
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c != '\\' {
            out.push(c);
            continue;
        }
        let e = *chars.get(i)?;
        i += 1;
        let cp = match e {
            'a' => 7,
            'b' => 8,
            'e' => 27,
            'f' => 12,
            'n' => 10,
            'r' => 13,
            't' => 9,
            'v' => 11,
            // `\=` starts subject modifiers and `\[...]{n}` repeats text: not plain subjects.
            '=' | '[' => return None,
            'x' if chars.get(i) == Some(&'{') => {
                let end = chars[i..].iter().position(|&c| c == '}')? + i;
                let v = hex(&chars[i + 1..end])?;
                i = end + 1;
                v
            }
            'x' => {
                let n = chars[i..].iter().take(2).take_while(|c| c.is_ascii_hexdigit()).count();
                let v = hex(&chars[i..i + n]).unwrap_or(0);
                i += n;
                v
            }
            '0'..='7' => {
                let n =
                    chars[i - 1..].iter().take(3).take_while(|c| ('0'..='7').contains(c)).count();
                let s: String = chars[i - 1..i - 1 + n].iter().collect();
                i += n - 1;
                u32::from_str_radix(&s, 8).ok()?
            }
            c if c.is_ascii_alphanumeric() => return None,
            c => u32::from(c),
        };
        if !utf && cp > 0x7F {
            return None;
        }
        out.push(char::from_u32(cp)?);
    }
    Some(out)
}

/// Text read one byte per character, decoded as UTF-8.
fn utf8(text: &str) -> Option<String> {
    let bytes: Option<Vec<u8>> = text.chars().map(|c| u8::try_from(u32::from(c)).ok()).collect();
    String::from_utf8(bytes?).ok()
}

/// A captured string as `pcre2test` prints it.
fn printed(s: &str, utf: bool) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match u32::from(c) {
            0x20..=0x7E => out.push(c),
            cp if utf => {
                let _ = write!(out, "\\x{{{cp:02x}}}");
            }
            cp => {
                let _ = write!(out, "\\x{cp:02x}");
            }
        }
    }
    out
}

/// The cases of a `testoutput` file.
fn cases(text: &str) -> Vec<Case> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].starts_with('/') {
            i += 1;
            continue;
        }
        let line = i + 1;
        let mut body = lines[i][1..].to_string();
        let end = loop {
            if let Some(end) = pattern_end(&body) {
                break Some(end);
            }
            i += 1;
            if i >= lines.len() {
                break None;
            }
            body.push('\n');
            body.push_str(lines[i]);
        };
        i += 1;
        let Some(end) = end else { break };
        let pattern = body[..end].to_string();
        let modifiers = body[end + 1..].trim().to_string();
        let Some((options, prefix, utf)) = translate(&modifiers) else { continue };
        if !pattern.is_ascii() && !utf {
            continue;
        }
        // The file was read byte by byte; UTF patterns and subjects are UTF-8.
        let Some(pattern) = utf8(&pattern) else { continue };
        let pattern = with_prefix(&pattern, &prefix);
        if lines.get(i).is_some_and(|l| l.starts_with("Failed: error")) {
            out.push(Case { line, pattern, options, utf, subjects: None });
            continue;
        }
        let mut subjects = Vec::new();
        let mut usable = true;
        while i < lines.len() && !lines[i].starts_with('/') {
            let l = lines[i];
            if !l.starts_with("    ") {
                i += 1;
                if !(l.is_empty() || l.starts_with("\\=") || l.starts_with('#')) {
                    usable = false;
                }
                continue;
            }
            let line = if utf { utf8(l) } else { Some(l.to_string()) };
            let decoded = line.and_then(|l| subject(&l, utf));
            i += 1;
            let mut groups = Vec::new();
            let mut outcome = None;
            let mut ok = true;
            while i < lines.len() && !lines[i].starts_with("    ") && !lines[i].is_empty() {
                let r = lines[i];
                i += 1;
                if r == "No match" {
                    outcome = Some(Outcome::NoMatch);
                } else if r.starts_with("Error -47") || r.starts_with("Error -52") {
                    outcome = Some(Outcome::Error);
                } else if let Some((n, text)) = group_line(r) {
                    if n != groups.len() {
                        ok = false;
                    }
                    groups.push((text != "<unset>").then(|| text.to_string()));
                } else if r.starts_with("--->") {
                    // A callout's trace: the subject lines that follow are not subjects.
                    usable = false;
                } else if r.starts_with("MK: ") || r.starts_with(" 0+ ") {
                } else if r.starts_with("\\=") || r.starts_with('#') || r.starts_with('/') {
                    i -= 1;
                    break;
                } else {
                    ok = false;
                }
            }
            if !groups.is_empty() {
                outcome = Some(Outcome::Match(groups));
            }
            if let (Some(s), Some(o), true) = (decoded, outcome, ok) {
                subjects.push((s, o));
            }
        }
        if usable {
            out.push(Case { line, pattern, options, utf, subjects: Some(subjects) });
        }
    }
    out
}

/// ` n: text`.
fn group_line(line: &str) -> Option<(usize, &str)> {
    let (n, text) = line.split_once(": ").or_else(|| line.strip_suffix(':').map(|n| (n, "")))?;
    let n = n.trim_start();
    (n.len() < line.len() && n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty())
        .then(|| (n.parse().ok(), text))
        .and_then(|(n, t)| n.map(|n| (n, t)))
}

/// The byte offset of the closing `/` of a pattern followed only by modifiers.
fn pattern_end(body: &str) -> Option<usize> {
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut last = None;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1,
            b'/' => last = Some(i),
            _ => {}
        }
        i += 1;
    }
    let end = last?;
    (!body[end + 1..].trim_end().contains(char::is_whitespace)).then_some(end)
}

/// `prefix` after the verbs that must open the pattern.
fn with_prefix(pattern: &str, prefix: &str) -> String {
    if prefix.is_empty() {
        return pattern.to_string();
    }
    let mut at = 0;
    while pattern[at..].starts_with("(*")
        && let Some(close) = pattern[at..].find(')')
        && pattern[at + 2..at + close].chars().all(|c| c.is_ascii_uppercase() || c == '_')
    {
        at += close + 1;
    }
    format!("{}{prefix}{}", &pattern[..at], &pattern[at..])
}

/// Runs every case of `file`; returns the failures.
fn run(file: &str) -> (usize, Vec<String>) {
    let mut failures = Vec::new();
    let mut count = 0;
    for case in cases(&testdata(file)) {
        count += 1;
        let compiled = Regex::new(&case.pattern, &case.options);
        let where_ = format!("{file}:{} /{}/{}", case.line, case.pattern, case.options);
        let (re, subjects) = match (compiled, case.subjects) {
            (Err(_), None) => continue,
            (Ok(_), None) => {
                failures.push(format!("{where_}: compiles, PCRE2 refuses it"));
                continue;
            }
            (Err(e), Some(_)) => {
                failures.push(format!("{where_}: {e}"));
                continue;
            }
            (Ok(re), Some(s)) => (re, s),
        };
        for (s, want) in subjects {
            let got = match re.find_at(&s, 0) {
                Err(_) => Outcome::Error,
                Ok(None) => Outcome::NoMatch,
                Ok(Some(m)) => {
                    let mut groups: Vec<Option<String>> = m
                        .groups
                        .iter()
                        .map(|g| g.map(|(a, b)| printed(&s[a.min(b)..b.max(a)], case.utf)))
                        .collect();
                    while groups.len() > 1 && groups.last() == Some(&None) {
                        groups.pop();
                    }
                    Outcome::Match(groups)
                }
            };
            if got != want {
                failures.push(format!("{where_} on {s:?}: got {got:?}, PCRE2 {want:?}"));
            }
        }
    }
    (count, failures)
}

fn check(file: &str) {
    let (count, failures) = run(file);
    assert!(count > 100, "{file}: only {count} cases read");
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn perl_compatible_non_utf() {
    check("testoutput1");
}

#[test]
fn perl_compatible_utf() {
    check("testoutput4");
}

#[test]
fn pcre2_specific_utf() {
    check("testoutput5");
}

#[test]
fn pcre2_specific() {
    check("testoutput2");
}
