//! `expand` puts each literal in place of its mark, `${name:CONSTANT}`, and refuses a mark that names no constant.

use teseladb_names::{ALL, ROUTER, SASL_X509, UnknownName, expand, expand_static};

#[test]
fn a_mark_becomes_its_literal() {
    let text = "{\"mechanism\": \"${name:SASL_X509}\", \"process\": \"${name:ROUTER}\"}";
    let want = format!("{{\"mechanism\": \"{SASL_X509}\", \"process\": \"{ROUTER}\"}}");
    assert_eq!(expand(text), Ok(want));
}

#[test]
fn text_without_marks_stays_as_it_is() {
    for text in ["", "plain text", "a $ sign and ${ a brace", "${name:", "${other:ROUTER}"] {
        assert_eq!(expand(text).as_deref(), Ok(text), "{text}");
    }
}

#[test]
fn an_unknown_mark_is_an_error() {
    assert_eq!(expand("x ${name:NOT_A_NAME} y"), Err(UnknownName("NOT_A_NAME".to_owned())));
    assert_eq!(UnknownName("X".to_owned()).to_string(), "unknown name mark ${name:X}");
}

#[test]
fn every_constant_is_listed_once_and_expands() {
    let mut seen = std::collections::BTreeSet::new();
    for (name, value) in ALL {
        assert!(seen.insert(*name), "{name} listed twice");
        assert_eq!(expand(&format!("${{name:{name}}}")).as_deref(), Ok(*value));
        assert!(!value.is_empty(), "{name}");
    }
}

#[test]
fn static_text_without_marks_is_the_same_text() {
    let text: &'static str = "no marks here";
    assert!(std::ptr::eq(expand_static(text), text));
    assert_eq!(expand_static("${name:ROUTER}"), ROUTER);
}

#[test]
#[should_panic(expected = "unknown name mark ${name:NOPE}")]
fn static_text_with_an_unknown_mark_panics() {
    let _ = expand_static("${name:NOPE}");
}
