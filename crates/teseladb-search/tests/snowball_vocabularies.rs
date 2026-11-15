//! Every Snowball stemmer of the search analyzers (011 FR-007) against the official test
//! vocabulary of its language (`snowball-data`), the version before the later rule changes: each
//! word must give exactly the stem the vocabulary's output gives. The files are those of
//! `scripts/snowball-vocabularies.sha256`, fetched into `.tools/snowball` with
//! `scripts/fetch-pinned.sh`, each checked by its SHA-256.

use std::collections::BTreeMap;
use std::path::PathBuf;

use teseladb_search::analysis::snowball::Stemmer;

const LIST: &str = include_str!("../../../scripts/snowball-vocabularies.sha256");

/// The `voc.txt` and `output.txt` paths of each language in the pinned list.
fn vocabularies() -> BTreeMap<String, (String, String)> {
    let mut out: BTreeMap<String, (String, String)> = BTreeMap::new();
    for line in LIST.lines().filter(|l| !l.starts_with('#') && !l.trim().is_empty()) {
        let rel = line.split_whitespace().nth(1).expect("a path after the hash");
        let mut parts = rel.split('/');
        let (_commit, lang, file) = (parts.next(), parts.next().expect("language"), parts.next());
        let entry = out.entry(lang.to_string()).or_default();
        match file {
            Some("voc.txt") => entry.0 = rel.to_string(),
            Some("output.txt") => entry.1 = rel.to_string(),
            other => panic!("unexpected file {other:?} in {rel}"),
        }
    }
    out
}

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.tools/snowball").join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{}: {e}; run scripts/fetch-pinned.sh scripts/snowball-vocabularies.sha256",
            path.display()
        )
    })
}

#[test]
fn every_stemmer_gives_the_stems_of_its_official_vocabulary() {
    let vocabularies = vocabularies();
    assert_eq!(vocabularies.len(), 22, "one vocabulary per Snowball stemmer");
    let mut checked = 0;
    for (lang, (voc, output)) in &vocabularies {
        let stemmer = Stemmer::by_name(lang).unwrap_or_else(|| panic!("no stemmer {lang}"));
        let (words, stems) = (read(voc), read(output));
        assert_eq!(words.lines().count(), stems.lines().count(), "{lang}");
        let mut stem = String::new();
        for (word, want) in words.lines().zip(stems.lines()) {
            // `stem_into` appends, so the buffer is emptied for each word.
            stem.clear();
            stemmer.stem_into(word, &mut stem);
            assert_eq!(stem, want, "{lang}: {word}");
            checked += 1;
        }
    }
    assert!(checked > 1_000_000, "{checked} words");
}
