//! The generator (003 FR-018, T023): from the official files it builds the tables committed in
//! `crates/teseladb-collation/data/uca.bin` byte for byte, `--check` notices a difference, and
//! missing or broken data and wrong arguments are errors that say what to do.

use std::fs;
use std::path::{Path, PathBuf};

use teseladb_unicodegen::{LOCALES, OUTPUT, build, build_locales, run};

fn repo() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
}

/// A repository in a temporary directory whose lists point at `cldr` and `ucd`, and at the real
/// ICU data.
fn fake_repo(name: &str, cldr: &Path, ucd: &Path) -> PathBuf {
    let root = std::env::temp_dir().join(format!("unicodegen-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("scripts")).unwrap();
    let icu = fs::read_to_string(repo().join("scripts/icu-coll.sha256")).unwrap();
    let icu_dest = icu.lines().find_map(|l| l.strip_prefix("# dest ")).unwrap();
    let icu = icu.replace(
        &format!("# dest {icu_dest}"),
        &format!("# dest {}", repo().join(icu_dest).display()),
    );
    fs::write(root.join("scripts/icu-coll.sha256"), icu).unwrap();
    fs::write(root.join("scripts/unicode-cldr.sha256"), format!("# dest {}\n", cldr.display()))
        .unwrap();
    fs::write(root.join("scripts/unicode-ucd.sha256"), format!("# dest {}\n", ucd.display()))
        .unwrap();
    root
}

fn real_dirs() -> (PathBuf, PathBuf) {
    let dest = |list: &str| {
        let text = fs::read_to_string(repo().join("scripts").join(list)).unwrap();
        repo().join(text.lines().find_map(|l| l.strip_prefix("# dest ")).unwrap())
    };
    (dest("unicode-cldr.sha256"), dest("unicode-ucd.sha256"))
}

#[test]
fn the_official_files_give_the_committed_tables() {
    let t = build(&repo()).unwrap();
    assert_eq!(t.version, "8.0.0");
    // FractionalUCA's mappings: allkeys' 30,069 (its four contractions `L·` are contexts
    // `L|·` here) plus ICU's 132 script-start markers (`FDD1 …`); the `FDD0 …` lines and
    // `FDD1 FDD0` are none.
    assert_eq!((t.singles.len(), t.contractions.len(), t.contexts.len()), (29_191, 1_006, 4));
    assert_eq!(t.variable_top, 0x0BFF_FFFF);
    assert_eq!(t.core_han_blocks, [(0x4E00, 0x9FFF), (0xF900, 0xFAFF)]);
    assert!(t.unified_ideographs.contains(&(0x4E00, 0x9FD5)));
    assert_eq!(t.to_bytes(), fs::read(repo().join(OUTPUT)).unwrap());
    let locales = build_locales(&repo()).unwrap();
    assert_eq!(locales.to_bytes(), fs::read(repo().join(LOCALES)).unwrap());
    assert_eq!((locales.bundles.len(), locales.tailorings.len()), (110, 139));
    assert_eq!(run(&["--check".into()], &repo()).0, 0);
}

#[test]
fn arguments_missing_data_and_differences_are_errors() {
    assert_eq!(
        run(&["--force".into()], &repo()),
        (2, "usage: teseladb-unicodegen [--check]".into())
    );
    let (cldr, ucd) = real_dirs();
    // The data is there but the output directory is not: writing fails; with it, it is written
    // and checks; a different file does not.
    let root = fake_repo("write", &cldr, &ucd);
    assert_eq!(run(&[], &root).0, 1);
    fs::create_dir_all(root.join(OUTPUT).parent().unwrap()).unwrap();
    assert_eq!(run(&[], &root).0, 0);
    assert_eq!(fs::read(root.join(OUTPUT)).unwrap(), fs::read(repo().join(OUTPUT)).unwrap());
    assert_eq!(fs::read(root.join(LOCALES)).unwrap(), fs::read(repo().join(LOCALES)).unwrap());
    assert_eq!(run(&["--check".into()], &root).0, 0);
    fs::write(root.join(LOCALES), b"old").unwrap();
    assert!(run(&["--check".into()], &root).1.contains("locales.bin is not what"));
    fs::write(root.join(OUTPUT), b"old").unwrap();
    let (code, message) = run(&["--check".into()], &root);
    assert_eq!(code, 1);
    assert!(message.contains("is not what the generator writes"), "{message}");
    fs::remove_dir_all(&root).unwrap();
    // No data: the message says how to get it.
    let empty = std::env::temp_dir().join(format!("unicodegen-empty-{}", std::process::id()));
    let root = fake_repo("missing", &empty, &empty);
    let (code, message) = run(&[], &root);
    assert_eq!(code, 1);
    assert!(message.contains("run scripts/fetch-pinned.sh"), "{message}");
    // A list without its destination, and no list at all.
    fs::write(root.join("scripts/unicode-cldr.sha256"), "# source x\n").unwrap();
    assert!(build(&root).unwrap_err().contains("has no '# dest'"));
    fs::remove_file(root.join("scripts/unicode-cldr.sha256")).unwrap();
    assert!(build(&root).is_err());
    fs::remove_dir_all(&root).unwrap();
    // A data file that does not read.
    let broken =
        std::env::temp_dir().join(format!("unicodegen-broken-data-{}", std::process::id()));
    fs::create_dir_all(broken.join("common/uca")).unwrap();
    fs::write(broken.join("common/uca/FractionalUCA.txt"), "0041 [29, 05, 05]\n").unwrap();
    let root = fake_repo("broken", &broken, &ucd);
    assert!(build(&root).unwrap_err().contains("FractionalUCA.txt:1"));
    fs::remove_dir_all(&root).unwrap();
    fs::remove_dir_all(&broken).unwrap();
}

#[test]
fn a_tailoring_that_does_not_build_is_an_error() {
    // ICU data with the root and one locale whose rules do not read.
    let icu = std::env::temp_dir().join(format!("unicodegen-bad-icu-data-{}", std::process::id()));
    let coll = icu.join("source/data/coll");
    fs::create_dir_all(&coll).unwrap();
    let real = fs::read_to_string(repo().join("scripts/icu-coll.sha256")).unwrap();
    let dest = real.lines().find_map(|l| l.strip_prefix("# dest ")).unwrap();
    let real_root = repo().join(dest).join("source/data/coll/root.txt");
    fs::copy(real_root, coll.join("root.txt")).unwrap();
    fs::write(coll.join("aa.txt"), "aa{ collations{ standard{ Sequence{\"&\"} } } }").unwrap();
    fs::write(
        coll.join("bb.txt"),
        "bb{ collations{ standard{ Sequence{\"[reorder Grek Grek]\"} } } }",
    )
    .unwrap();
    let (cldr, ucd) = real_dirs();
    let root = fake_repo("bad-icu", &cldr, &ucd);
    let list = format!(
        "# dest {}\n0  source/data/coll/root.txt\n0  source/data/coll/aa.txt\n",
        icu.display()
    );
    fs::write(root.join("scripts/icu-coll.sha256"), list).unwrap();
    let err = build_locales(&root).unwrap_err();
    assert!(err.starts_with("aa standard: "), "{err}");
    assert_eq!(run(&[], &root).0, 1);
    let list = format!(
        "# dest {}\n0  source/data/coll/root.txt\n0  source/data/coll/bb.txt\n",
        icu.display()
    );
    fs::write(root.join("scripts/icu-coll.sha256"), list).unwrap();
    let err = build_locales(&root).unwrap_err();
    assert!(err.starts_with("bb standard: [reorder]"), "{err}");
    fs::remove_dir_all(&root).unwrap();
    fs::remove_dir_all(&icu).unwrap();
}

#[test]
fn the_binary_checks_and_refuses_unknown_arguments() {
    let bin = env!("CARGO_BIN_EXE_teseladb-unicodegen");
    let ok = std::process::Command::new(bin).arg("--check").output().unwrap();
    assert!(ok.status.success(), "{}", String::from_utf8_lossy(&ok.stderr));
    assert!(String::from_utf8_lossy(&ok.stdout).contains("is up to date"));
    let bad = std::process::Command::new(bin).arg("--nope").output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&bad.stderr).contains("usage"));
}
