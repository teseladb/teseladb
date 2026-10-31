//! `teseladb-unicodegen [--check]`: writes or checks `crates/teseladb-collation/data/uca.bin` from
//! the official Unicode and CLDR files (003 FR-018, D-043).
#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let (code, message) = teseladb_unicodegen::run(&args, root);
    if code == 0 {
        println!("{message}");
    } else {
        eprintln!("{message}");
    }
    ExitCode::from(code as u8)
}
