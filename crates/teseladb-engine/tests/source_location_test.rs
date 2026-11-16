//! Tests of `SourceLocation`: the file, line and function name a call site captures.

#![cfg(target_os = "linux")]

use teseladb::engine::util::source_location_compat::internal::SourceLocation;
use teseladb::source_location_current;

/// Checks that `loc` names the given file, line and function. The location is taken at
/// the call site, which passes it explicitly.
fn test_source_location_callee(ref_file: &str, ref_func: &str, ref_line: u32, loc: SourceLocation) {
    assert_eq!(loc.file_name(), ref_file);
    assert_eq!(loc.line(), ref_line);
    assert!(loc.function_name().contains(ref_func));
}

// `line!()` and the captured location must come from the same line, so the call stays on one.
#[rustfmt::skip]
fn test_source_location_caller() {
    test_source_location_callee(file!(), "test_source_location_caller", line!(), source_location_current!());
}

#[test]
fn test_source_location() {
    test_source_location_caller();
}
