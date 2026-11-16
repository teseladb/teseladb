#![cfg(target_os = "linux")]
//! The two-path file-system error (of a rename or a link, for instance) carries both paths
//! it was given, in order, and its message names both.

use std::path::{Path, PathBuf};

use teseladb::engine::util::exceptions::make_filesystem_error2;

/// `EEXIST`.
const EEXIST: i32 = 17;

#[test]
fn both_paths_are_kept() {
    let e = make_filesystem_error2("rename", PathBuf::from("/from"), PathBuf::from("/to"), EEXIST);
    assert_eq!(e.code(), EEXIST);
    assert_eq!(e.path1(), Path::new("/from"));
    assert_eq!(e.path2(), Some(Path::new("/to")));
    let text = e.to_string();
    assert!(text.starts_with("filesystem error: rename: "), "{text}");
    assert!(text.ends_with("[/from] [/to]"), "{text}");
}
