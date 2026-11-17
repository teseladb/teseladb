//! Error codes of malformed relabel expressions and bracket expressions that compile, checked
//! against the original's std::regex (ECMAScript grammar, libstdc++ of the reference image):
//! the code each pattern below gets there is the one asserted here.
#![cfg(target_os = "linux")]

use teseladb::engine::core::relabel_config::regex::{ErrorType, Regex, RegexError};

fn code(expr: &str) -> Option<ErrorType> {
    match Regex::new(expr) {
        Ok(_) => None,
        Err(e) => {
            let err = e.downcast_ref::<RegexError>().unwrap_or_else(|| panic!("{expr}: {e}"));
            assert!(!err.to_string().is_empty());
            Some(err.code())
        }
    }
}

#[test]
fn error_codes_as_the_reference() {
    let cases: [(&str, Option<ErrorType>); 13] = [
        ("a{x}", Some(ErrorType::Badbrace)),
        ("a{1,x}", Some(ErrorType::Badbrace)),
        ("a{2,1}", Some(ErrorType::Badbrace)),
        ("a{1", Some(ErrorType::Brace)),
        ("a)", Some(ErrorType::Paren)),
        ("(?:a", Some(ErrorType::Paren)),
        ("[[:alpha:]", Some(ErrorType::Brack)),
        ("[\\w-z]", Some(ErrorType::Range)),
        ("[a-\\w]", Some(ErrorType::Range)),
        ("[z-a]", Some(ErrorType::Range)),
        ("[[a]", None),
        ("[[.a.]]", None),
        ("[[=a=]]", None),
    ];
    let wrong: Vec<String> = cases
        .iter()
        .filter_map(|(e, want)| {
            let got = code(e);
            (got != *want).then(|| format!("{e}: got {got:?}, the reference gives {want:?}"))
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:#?}");
}

#[test]
fn bracket_expressions_that_compile_match() {
    let r = Regex::new("[[a]+").unwrap();
    assert!(r.is_full_match("[a[a"));
    assert!(Regex::new("[[.a.]]").unwrap().is_full_match("a"));
    assert!(Regex::new("[[=a=]]").unwrap().is_full_match("a"));
}
