//! `teseladb-tzgen <tzdata tarball or directory> [--check]`: writes or checks
//! `crates/teseladb-tz/src/tzdata.rs` from the IANA time zone files (010 FR-007, D-026).
#![forbid(unsafe_code)]

fn main() -> std::process::ExitCode {
    let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    teseladb_tzgen::main(
        std::env::args().skip(1).collect(),
        root,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
}
