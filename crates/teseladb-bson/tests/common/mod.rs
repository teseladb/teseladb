//! The BSON corpus of RefDb's specifications for the tests: downloaded by
//! `scripts/fetch-bson-corpus.sh` (decision D-042) into `.tools/bson-corpus/<commit>/`.

#![allow(dead_code)]

use std::fs;
use std::path::PathBuf;

use teseladb_json::Value as Json;

/// One file of the corpus: its name and its JSON.
pub struct CorpusFile {
    pub name: String,
    pub json: Json,
}

impl CorpusFile {
    /// The cases under `key` (`valid`, `decodeErrors`, …).
    pub fn cases(&self, key: &str) -> Vec<Json> {
        self.json.get(key).and_then(Json::as_array).unwrap_or(&[]).to_vec()
    }
}

/// Every file of `scripts/bson-corpus.sha256`; fails if one is missing.
pub fn corpus() -> Vec<CorpusFile> {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let list =
        fs::read_to_string(root.join("scripts/bson-corpus.sha256")).expect("the corpus list");
    let commit = list.lines().find_map(|l| l.strip_prefix("# commit ")).expect("a '# commit' line");
    let dir = root.join(".tools/bson-corpus").join(commit);
    list.lines()
        .filter(|l| !l.starts_with('#') && !l.is_empty())
        .map(|l| {
            let name = l.split_whitespace().nth(1).expect("<sha256>  <file>").to_owned();
            let path = dir.join(&name);
            let text = fs::read(&path).unwrap_or_else(|e| {
                panic!("{}: {e}; run scripts/fetch-bson-corpus.sh", path.display())
            });
            CorpusFile { name, json: teseladb_json::parse(&text).expect("corpus JSON") }
        })
        .collect()
}

/// Bytes from the corpus's hexadecimal.
pub fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "{s}");
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
}

/// Every valid document of the corpus, in canonical BSON.
pub fn valid_documents() -> Vec<Vec<u8>> {
    corpus()
        .iter()
        .flat_map(|f| f.cases("valid"))
        .map(|c| hex(c.get("canonical_bson").and_then(Json::as_str).expect("canonical_bson")))
        .collect()
}
