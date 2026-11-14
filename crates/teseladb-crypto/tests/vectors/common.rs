//! Readers of the stored test vectors: Wycheproof JSON and NIST CAVP response files.

use std::path::PathBuf;

use teseladb_json::{Object, Value};

/// Path of a file under `tests/data/`.
pub fn data_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(rel)
}

/// Decodes lowercase or uppercase hex.
pub fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd hex length: {s}");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digit"))
        .collect()
}

/// Wycheproof's verdict for a test case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expected {
    /// Must be accepted.
    Valid,
    /// Must be rejected.
    Invalid,
    /// Either answer is allowed (legacy encodings, weak parameters).
    Acceptable,
}

const RESULT_VALID: &str = "valid";
const RESULT_INVALID: &str = "invalid";
const RESULT_ACCEPTABLE: &str = "acceptable";

/// One Wycheproof test case with its group.
pub struct Case<'a> {
    pub group: &'a Object,
    pub test: &'a Object,
    pub expected: Expected,
}

impl Case<'_> {
    pub fn id(&self) -> i64 {
        self.test.get("tcId").and_then(Value::as_i64).expect("tcId")
    }
    pub fn str(&self, key: &str) -> &str {
        self.test
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("tc {}: no {key}", self.id()))
    }
    pub fn hex(&self, key: &str) -> Vec<u8> {
        hex(self.str(key))
    }
    pub fn int(&self, key: &str) -> i64 {
        self.test
            .get(key)
            .and_then(Value::as_i64)
            .unwrap_or_else(|| panic!("tc {}: no {key}", self.id()))
    }
    pub fn group_int(&self, key: &str) -> i64 {
        self.group.get(key).and_then(Value::as_i64).unwrap_or_else(|| panic!("group: no {key}"))
    }
    pub fn group_obj(&self, key: &str) -> &Object {
        self.group.get(key).and_then(Value::as_object).unwrap_or_else(|| panic!("group: no {key}"))
    }
    pub fn group_str(&self, key: &str) -> &str {
        self.group.get(key).and_then(Value::as_str).unwrap_or_else(|| panic!("group: no {key}"))
    }
}

/// Parses a Wycheproof file and calls `f` for each case; returns how many ran.
pub fn wycheproof(file: &str, mut f: impl FnMut(&Case<'_>)) -> usize {
    let bytes = std::fs::read(data_path(&format!("wycheproof/{file}"))).expect("vector file");
    let root = teseladb_json::parse(&bytes).expect("json");
    let mut n = 0;
    for group in root.get("testGroups").and_then(Value::as_array).expect("testGroups") {
        let group = group.as_object().expect("group");
        for test in group.get("tests").and_then(Value::as_array).expect("tests") {
            let test = test.as_object().expect("test");
            let expected = match test.get("result").and_then(Value::as_str) {
                Some(RESULT_VALID) => Expected::Valid,
                Some(RESULT_INVALID) => Expected::Invalid,
                Some(RESULT_ACCEPTABLE) => Expected::Acceptable,
                other => panic!("unknown result {other:?}"),
            };
            f(&Case { group, test, expected });
            n += 1;
        }
    }
    n
}

/// Checks an outcome against the verdict: `Acceptable` takes either.
pub fn check(case: &Case<'_>, ok: bool) {
    match case.expected {
        Expected::Valid => assert!(ok, "tc {} should pass", case.id()),
        Expected::Invalid => assert!(!ok, "tc {} should fail", case.id()),
        Expected::Acceptable => {}
    }
}

/// Records of a CAVP `.rsp` file: each a list of `KEY = value` pairs, with the `[SECTION]` they
/// fall under.
pub fn cavp(file: &str) -> Vec<(String, Vec<(String, String)>)> {
    let text = std::fs::read_to_string(data_path(&format!("cavp/{file}"))).expect("rsp file");
    let mut out = Vec::new();
    let mut section = String::new();
    let mut cur: Vec<(String, String)> = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') {
            continue;
        }
        if line.is_empty() {
            if !cur.is_empty() {
                out.push((section.clone(), std::mem::take(&mut cur)));
            }
            continue;
        }
        if line.starts_with('[') {
            section = line.trim_matches(|c| c == '[' || c == ']').to_string();
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            cur.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    if !cur.is_empty() {
        out.push((section, cur));
    }
    out
}

/// Value of `key` in a CAVP record.
pub fn field<'a>(rec: &'a [(String, String)], key: &str) -> &'a str {
    rec.iter().find(|(k, _)| k == key).map_or_else(|| panic!("no {key}"), |(_, v)| v.as_str())
}
