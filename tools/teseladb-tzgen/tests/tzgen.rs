//! The generator of `teseladb-tz`: reading TZif files and writing the database.

use std::collections::BTreeMap;
use std::path::Path;

use teseladb_tzgen::tzif::{Type, Tzif};

fn header(counts: [u32; 6]) -> Vec<u8> {
    let mut h = b"TZif2".to_vec();
    h.extend([0u8; 15]);
    for c in counts {
        h.extend(c.to_be_bytes());
    }
    h
}

/// A TZif v2 file: no v1 data, two transitions between two types, and a footer.
fn sample() -> Vec<u8> {
    let mut b = header([0; 6]);
    b.extend(header([0, 0, 0, 2, 2, 8]));
    for t in [-100_i64, 1_000_000] {
        b.extend(t.to_be_bytes());
    }
    b.extend([1u8, 0]);
    for (off, dst, idx) in [(3600_i32, 0u8, 0u8), (7200, 1, 4)] {
        b.extend(off.to_be_bytes());
        b.extend([dst, idx]);
    }
    b.extend(b"AAA\0BBB\0");
    b.extend(b"\nAAA-1BBB,M3.5.0,M10.5.0/3\n");
    b
}

#[test]
fn tzif_files_give_transitions_types_and_footer() {
    let z = Tzif::parse(&sample()).expect("valid").expect("a TZif file");
    assert_eq!(z.transitions, [-100, 1_000_000]);
    assert_eq!(z.indices, [1, 0]);
    assert_eq!(z.types[0], Type { offset: 3600, dst: false, abbr: "AAA".to_owned() });
    assert_eq!(z.types[1], Type { offset: 7200, dst: true, abbr: "BBB".to_owned() });
    assert_eq!(z.footer, "AAA-1BBB,M3.5.0,M10.5.0/3");
}

#[test]
fn other_files_are_skipped_and_cut_files_refused() {
    assert_eq!(Tzif::parse(b"# zone.tab"), Ok(None));
    let full = sample();
    for cut in [10, 50, 70, 100, 110, 120] {
        assert!(Tzif::parse(&full[..cut]).is_err(), "{cut}");
    }
    let mut bad_second = header([0; 6]);
    bad_second.extend(b"XXXX");
    bad_second.extend([0u8; 40]);
    assert!(Tzif::parse(&bad_second).is_err());
}

#[test]
fn the_database_shares_the_data_of_linked_names() {
    let z = Tzif::parse(&sample()).expect("valid").expect("a TZif file");
    let mut zones = BTreeMap::new();
    zones.insert("Test/A".to_owned(), z.clone());
    zones.insert("Test/B".to_owned(), z.clone());
    let mut other = z;
    other.footer = "UTC0".to_owned();
    zones.insert("Test/C".to_owned(), other);
    let text = teseladb_tzgen::render("2099z", &zones);
    assert!(text.contains("pub const VERSION: &str = \"2099z\";"));
    assert!(text.contains("(\"Test/A\", 0),"));
    assert!(text.contains("(\"Test/B\", 0),"));
    assert!(text.contains("(\"Test/C\", 1),"));
    assert!(text.contains("pub static ZONES: [ZoneData; 2]"));
    assert!(text.contains("LocalType { offset: 7200, dst: true, abbr: \"BBB\" }"));
}

#[test]
fn the_generator_needs_a_tzdata_directory() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (code, message) = teseladb_tzgen::run(&[], root);
    assert_eq!(code, 1);
    assert!(message.starts_with("usage:"), "{message}");
    let (code, message) =
        teseladb_tzgen::run(&["--check".to_owned(), "/nonexistent/tzdata".to_owned()], root);
    assert_eq!(code, 1);
    assert!(message.contains("version"), "{message}");
}

#[test]
fn the_database_is_written_and_checked_from_compiled_files() {
    let base = std::env::temp_dir().join(format!("tzgen-test-{}", std::process::id()));
    let zoneinfo = base.join("zoneinfo");
    let root = base.join("repo");
    std::fs::create_dir_all(zoneinfo.join("Test")).expect("a directory");
    std::fs::create_dir_all(root.join("crates/teseladb-tz/src")).expect("a directory");
    std::fs::write(zoneinfo.join("Test/Zone"), sample()).expect("a file");
    std::fs::write(zoneinfo.join("zone.tab"), "# not a TZif file").expect("a file");
    let wrote = teseladb_tzgen::write_database(&zoneinfo, "2099z", &root, false).expect("written");
    assert_eq!(wrote, "wrote crates/teseladb-tz/src/tzdata.rs: 1 zones, tzdata 2099z");
    let ok = teseladb_tzgen::write_database(&zoneinfo, "2099z", &root, true).expect("the same");
    assert!(ok.contains("matches"), "{ok}");
    let drift = teseladb_tzgen::write_database(&zoneinfo, "2100a", &root, true)
        .expect_err("a different version differs");
    assert!(drift.contains("differs"), "{drift}");
    std::fs::write(zoneinfo.join("Test/Cut"), &sample()[..60]).expect("a file");
    assert!(teseladb_tzgen::write_database(&zoneinfo, "2099z", &root, false).is_err());
    assert!(teseladb_tzgen::write_database(&base.join("missing"), "2099z", &root, false).is_err());
    std::fs::remove_dir_all(&base).expect("cleaned");
}

#[test]
fn the_generator_reports_tools_that_fail() {
    let base = std::env::temp_dir().join(format!("tzgen-fail-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("a directory");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // A tarball that is no tarball: tar fails.
    let tarball = base.join("broken.tar.gz");
    std::fs::write(&tarball, b"not a tarball").expect("a file");
    let (code, message) = teseladb_tzgen::run(&[tarball.to_string_lossy().into_owned()], root);
    assert_eq!(code, 1);
    assert!(message.contains("tar"), "{message}");
    // A directory with a version but no Makefile: make fails.
    let dir = base.join("tzdata");
    std::fs::create_dir_all(&dir).expect("a directory");
    std::fs::write(dir.join("version"), "2099z\n").expect("a file");
    let (code, message) = teseladb_tzgen::run(&[dir.to_string_lossy().into_owned()], root);
    assert_eq!(code, 1);
    assert!(message.contains("make"), "{message}");
    // A repository without the output's directory: the file cannot be written or checked.
    let zoneinfo = base.join("zoneinfo");
    std::fs::create_dir_all(&zoneinfo).expect("a directory");
    let nowhere = base.join("no-repo");
    assert!(teseladb_tzgen::write_database(&zoneinfo, "2099z", &nowhere, false).is_err());
    assert!(teseladb_tzgen::write_database(&zoneinfo, "2099z", &nowhere, true).is_err());
    std::fs::remove_dir_all(&base).expect("cleaned");
}

#[test]
fn the_binary_reports_success_and_failure() {
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_teseladb-tzgen"))
            .args(args)
            .output()
            .expect("run")
    };
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage"));
    // A directory whose zone files are already compiled is not one the binary reads: it builds
    // them with make, which fails here, so the success path is the library's (tested above).
    let base = std::env::temp_dir().join(format!("tzgen-bin-{}", std::process::id()));
    std::fs::create_dir_all(&base).expect("a directory");
    std::fs::write(base.join("version"), "2099z").expect("a file");
    let out = run(&[&base.to_string_lossy()]);
    assert_eq!(out.status.code(), Some(1));
    std::fs::remove_dir_all(&base).expect("cleaned");
}

#[test]
fn entries_that_are_neither_files_nor_directories_are_skipped() {
    let base = std::env::temp_dir().join(format!("tzgen-link-{}", std::process::id()));
    let zoneinfo = base.join("zoneinfo");
    let root = base.join("repo");
    std::fs::create_dir_all(&zoneinfo).expect("a directory");
    std::fs::create_dir_all(root.join("crates/teseladb-tz/src")).expect("a directory");
    std::os::unix::fs::symlink(base.join("nowhere"), zoneinfo.join("Broken")).expect("a link");
    std::fs::write(zoneinfo.join("Zone"), sample()).expect("a file");
    let wrote = teseladb_tzgen::write_database(&zoneinfo, "2099z", &root, false).expect("written");
    assert!(wrote.contains("1 zones"), "{wrote}");
    std::fs::remove_dir_all(&base).expect("cleaned");
}

#[test]
fn the_main_function_writes_the_message_where_it_belongs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_tzgen::main(&[], root, &mut out, &mut err);
    assert_eq!(code, std::process::ExitCode::from(1));
    assert!(out.is_empty());
    assert!(String::from_utf8_lossy(&err).starts_with("usage"));
}

#[test]
fn a_tarball_is_unpacked_built_with_make_and_zic_and_written() {
    let base = std::env::temp_dir().join(format!("tzgen-full-{}", std::process::id()));
    let src = base.join("src");
    let root = base.join("repo");
    std::fs::create_dir_all(&src).expect("a directory");
    std::fs::create_dir_all(root.join("crates/teseladb-tz/src")).expect("a directory");
    std::fs::write(src.join("version"), "2099z\n").expect("a file");
    std::fs::write(
        src.join("Makefile"),
        "main.zi:\n\tprintf 'Zone Test/A 1:00 - AAA\\n' > main.zi\n",
    )
    .expect("a file");
    let tarball = base.join("tzdata2099z.tar.gz");
    let tar = std::process::Command::new("tar")
        .args(["-czf", &tarball.to_string_lossy(), "-C", &src.to_string_lossy(), "."])
        .status()
        .expect("tar");
    assert!(tar.success());
    let (code, message) = teseladb_tzgen::run(&[tarball.to_string_lossy().into_owned()], &root);
    assert_eq!(
        (code, message.as_str()),
        (0, "wrote crates/teseladb-tz/src/tzdata.rs: 1 zones, tzdata 2099z")
    );
    let text =
        std::fs::read_to_string(root.join("crates/teseladb-tz/src/tzdata.rs")).expect("written");
    assert!(text.contains("\"Test/A\""));
    std::fs::remove_dir_all(&base).expect("cleaned");
}

#[test]
fn long_literals_are_grouped_in_threes() {
    for (v, text) in [
        (0, "0"),
        (99_999, "99999"),
        (-99_999, "-99999"),
        (100_000, "100_000"),
        (-2_208_988_800, "-2_208_988_800"),
        (i64::MIN, "-9_223_372_036_854_775_808"),
    ] {
        assert_eq!(teseladb_tzgen::literal(v), text);
    }
}
