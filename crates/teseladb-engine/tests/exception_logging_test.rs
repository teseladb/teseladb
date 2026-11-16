//! Tests of how exceptions print in the log: an exception is logged with `format!("{}", ex)`
//! (the `Display` of `ExceptionPtr`), and `ExceptionPtr::with_nested(e, current)` nests the
//! current exception in `e`. Each type prints as `std::any::type_name` gives it, an exception
//! thrown with `throw_with_nested` prints as its own type followed by `: <nested>`, and every
//! exception has a `what()`, which is printed too. Also covers malformed format strings and
//! exceptions that carry a backtrace.

#![cfg(target_os = "linux")]

use std::any::type_name;
use std::fmt;
use std::sync::LazyLock;

use teseladb::engine::core::future::{
    ErrorCategory, ExceptionPtr, NestedException, RuntimeError, SystemError,
};
#[cfg(not(engine_backtrace_unimplemented))]
use teseladb::engine::core::relabel_config::regex::Regex;
use teseladb::engine::core::sstring::Sstring;
#[cfg(not(engine_backtrace_unimplemented))]
use teseladb::engine::util::backtrace::internal::Backtraced;
use teseladb::engine::util::backtrace::make_backtraced_exception_ptr;
use teseladb::engine::util::log::{FormatInfo, Logger, Ostream};

// A type that plays the part of an unknown object in the logging
// function.
#[derive(Debug)]
struct UnknownObj {
    message: Sstring,
}

impl UnknownObj {
    fn new(message: String) -> Self {
        UnknownObj { message: Sstring::from(message.as_str()) }
    }
}

/// The unknown object: its log shows its type name and `what()`, as for any exception.
impl fmt::Display for UnknownObj {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for UnknownObj {}

// This functions generates an exception chain nesting_level+1 deep
// for each nesting level it throws one of two types of objects, an
// unknown object or a runtime error,
// and it chooses the type of thrown object
// according to the bit in the `nesting_level` place in the
// `tests_instance` paramter or in other words according to:
// bool(test_instance & (1<<nesting_level))
fn exception_generator(test_instance: u32, nesting_level: i32) -> Result<(), ExceptionPtr> {
    if nesting_level > 0
        && let Err(current) = exception_generator(test_instance >> 1, nesting_level - 1)
    {
        let msg = format!("Exception Level {}", nesting_level);
        if test_instance & 1 != 0 {
            // Throw the unknown object
            return Err(ExceptionPtr::with_nested(UnknownObj::new(msg), current));
        } else {
            return Err(ExceptionPtr::with_nested(RuntimeError(msg), current));
        }
    }
    if nesting_level == 0 {
        if test_instance & 1 != 0 {
            return Err(ExceptionPtr::new(UnknownObj::new(format!(
                "Exception Level {}",
                nesting_level
            ))));
        } else {
            return Err(ExceptionPtr::new(RuntimeError(format!(
                "Exception Level {}",
                nesting_level
            ))));
        }
    }
    Ok(())
}

// This function generates the expected logging output string of an exception
// thrown by the  exception generator function with a specific output.
fn exception_generator_str(mut test_instance: u32, mut nesting_level: i32) -> String {
    let mut ret = String::new();
    let runtime_err_str = type_name::<RuntimeError>();
    let unknown_obj_str = type_name::<UnknownObj>();
    // A nesting exception is the outer exception itself, which carries the nested one.
    let nested_exception_with_unknown_obj_str = unknown_obj_str;
    let nested_exception_with_runtime_err_str = runtime_err_str;

    while nesting_level > 0 {
        if test_instance & 1 != 0 {
            ret += &format!(
                "{} (Exception Level {})",
                nested_exception_with_unknown_obj_str, nesting_level
            );
        } else {
            ret += &format!(
                "{} (Exception Level {})",
                nested_exception_with_runtime_err_str, nesting_level
            );
        }
        ret += ": ";
        test_instance >>= 1;
        nesting_level -= 1;
    }

    if test_instance & 1 != 0 {
        ret += &format!("{} (Exception Level {})", unknown_obj_str, nesting_level);
    } else {
        ret += &format!("{} (Exception Level {})", runtime_err_str, nesting_level);
    }
    ret
}

// Test all variations of nested exceptions of some
// depth
#[test]
fn nested_exception_logging1() {
    const LEVELS_TO_TEST: i32 = 3;
    for level in 0..LEVELS_TO_TEST {
        let mut inst: i32 = (1 << (level + 1)) - 1;
        while inst >= 0 {
            let mut log_msg = String::new();
            if let Err(e) = exception_generator(inst as u32, level) {
                log_msg += &e.to_string();
            }
            assert_eq!(log_msg, exception_generator_str(inst as u32, level));
            inst -= 1;
        }
    }
}

/// A nesting exception thrown outside of a catch: it nests no exception, so the log shows
/// only the exception's type and message (no `: <no exception>` part).
#[derive(Debug)]
struct StdNestedException;

impl fmt::Display for StdNestedException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("nested error")
    }
}

impl std::error::Error for StdNestedException {}

// Test logging of nested exception not mixed in with anything
#[test]
fn nested_exception_logging2() {
    let mut log_msg = String::new();
    // Thrown outside of a catch: it nests no exception.
    let e = ExceptionPtr::new(StdNestedException);
    log_msg += &e.to_string();
    assert_eq!(log_msg, format!("{} (nested error)", type_name::<StdNestedException>()));
}

#[derive(Debug)]
struct VeryImportantException;

impl fmt::Display for VeryImportantException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("very important information")
    }
}

impl std::error::Error for VeryImportantException {}

// Test logging of nested exception that have a system error mixed with other exceptions
// so that the system error is in the middle of the exception chain.
#[test]
fn nested_exception_logging3() {
    let mut log_msg = String::new();
    let first = ExceptionPtr::new(VeryImportantException);
    let second = ExceptionPtr::with_nested(
        SystemError::with_category(1, ErrorCategory::Generic, "my error"),
        first,
    );
    let third =
        ExceptionPtr::with_nested(UnknownObj::new("This is an unknown object".to_string()), second);
    log_msg += &third.to_string();
    let expected_string = format!(
        "{} (This is an unknown object): {} (error generic:1, my error: Operation not permitted): {} (very important information)",
        type_name::<UnknownObj>(),
        type_name::<SystemError>(),
        type_name::<VeryImportantException>()
    );
    assert_eq!(log_msg, expected_string);
}

#[test]
fn unknown_object_thrown_test() {
    let e = ExceptionPtr::new(UnknownObj::new("This is an unknown object".to_string()));
    let log_msg = e.to_string();
    assert_eq!(log_msg, format!("{} (This is an unknown object)", type_name::<UnknownObj>()));
}

/// The buffer the logger writes to, read back by the test.
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self.0.lock() {
            Ok(mut v) => v.extend_from_slice(b),
            Err(p) => p.into_inner().extend_from_slice(b),
        }
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn format_error_test() {
    static L: LazyLock<std::pin::Pin<Box<Logger>>> =
        LazyLock::new(|| Logger::new("format_error_test").expect("logger registered once"));
    let log_msg = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    Logger::set_ostream(Ostream::Custom(Box::new(SharedBuf(log_msg.clone()))));

    let fmt_ = "bad format string: {}";
    // A runtime string passed as the message must not be treated as a format
    // string: malformed braces in it are printed, not parsed, so we test
    // with a runtime format string here.
    L.error_runtime(FormatInfo::new(fmt_));

    let out = String::from_utf8_lossy(&log_msg.lock().expect("log buffer")).into_owned();
    println!("{}", out);
    assert!(out.contains(file!()));
    assert!(out.contains("format_error_test"));
    assert!(out.contains(fmt_));
}

/// The pattern `re` compiled with the library's regex. It has no case-insensitive option;
/// the text it is matched against is the library's own output, whose case the pattern
/// spells exactly, so the match is case-sensitive.
#[cfg(not(engine_backtrace_unimplemented))]
fn icase(re: &str) -> Regex {
    Regex::new(re).expect("regex")
}

/// The type name a runtime error thrown with a backtrace prints as.
#[cfg(not(engine_backtrace_unimplemented))]
fn backtraced_runtime_error() -> &'static str {
    type_name::<Backtraced<RuntimeError>>()
}

#[test]
fn throw_with_backtrace_exception_logging() {
    let e = make_backtraced_exception_ptr(RuntimeError(
        "throw_with_backtrace_exception_logging".to_string(),
    ));
    let log_msg = e.to_string();

    #[cfg(not(engine_backtrace_unimplemented))]
    {
        let regex_str = format!(
            "{} \\(throw_with_backtrace_exception_logging Backtrace:(\\s+(\\S+\\+)?0x[0-9a-f]+)+\\)",
            backtraced_runtime_error()
        );
        assert!(icase(&regex_str).search(&log_msg).is_some(), "{}\n{}", regex_str, log_msg);
    }
    let _ = log_msg;
}

#[test]
fn throw_with_backtrace_nested_exception_logging() {
    let outer = make_backtraced_exception_ptr(RuntimeError("outer".to_string()));
    let e =
        ExceptionPtr::with_nested(UnknownObj::new("This is an unknown object".to_string()), outer);
    let log_msg = e.to_string();

    #[cfg(not(engine_backtrace_unimplemented))]
    {
        let regex_str = format!(
            "{}.*{} \\(outer Backtrace:(\\s+(\\S+\\+)?0x[0-9a-f]+)+\\)",
            type_name::<UnknownObj>(),
            backtraced_runtime_error()
        );
        assert!(icase(&regex_str).search(&log_msg).is_some(), "{}\n{}", regex_str, log_msg);
    }
    let _ = log_msg;
}

#[test]
fn throw_with_backtrace_engine_nested_exception_logging() {
    let outer = ExceptionPtr::new(UnknownObj::new("This is an unknown object".to_string()));
    let inner = make_backtraced_exception_ptr(RuntimeError("inner".to_string()));
    let e = ExceptionPtr::new(NestedException::new(inner, outer));
    let log_msg = e.to_string();

    #[cfg(not(engine_backtrace_unimplemented))]
    {
        let regex_str = format!(
            "{}:.*{} \\(inner Backtrace:(\\s+(\\S+\\+)?0x[0-9a-f]+)+\\) \\(while cleaning up after {} \\(This is an unknown object\\)\\)",
            type_name::<NestedException>(),
            backtraced_runtime_error(),
            type_name::<UnknownObj>()
        );
        assert!(icase(&regex_str).search(&log_msg).is_some(), "{}\n{}", regex_str, log_msg);
    }
    let _ = log_msg;
}

#[test]
fn double_throw_with_backtrace_engine_nested_exception_logging() {
    let outer = make_backtraced_exception_ptr(RuntimeError("outer".to_string()));
    let inner = make_backtraced_exception_ptr(RuntimeError("inner".to_string()));
    let e = ExceptionPtr::new(NestedException::new(inner, outer));
    let log_msg = e.to_string();

    #[cfg(not(engine_backtrace_unimplemented))]
    {
        let regex_str = format!(
            "{}:.*{} \\(inner Backtrace:(\\s+(\\S+\\+)?0x[0-9a-f]+)+\\) \\(while cleaning up after .*{} \\(outer Backtrace:(\\s+(\\S+\\+)?0x[0-9a-f]+)+\\)\\)",
            type_name::<NestedException>(),
            backtraced_runtime_error(),
            backtraced_runtime_error()
        );
        assert!(icase(&regex_str).search(&log_msg).is_some(), "{}\n{}", regex_str, log_msg);
    }
    let _ = log_msg;
}
