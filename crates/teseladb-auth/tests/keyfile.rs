//! Key files as `refnode` 9.0.4 reads them (013 T046, FR-034; measured on 31-10-2026).

use teseladb_auth::keyfile::{KeyFileError, parse};

#[test]
fn a_key_loses_its_whitespace() {
    let k = "abcdefghijklmnopqrstuvwxyz0123456789";
    assert_eq!(parse(k), Ok(vec![k.to_owned()]));
    assert_eq!(parse("abc def\nghi jkl mnopq\n"), Ok(vec!["abcdefghijklmnopq".to_owned()]));
    assert_eq!(parse("'quoted+/='\n"), Ok(vec!["quoted+/=".to_owned()]));
}

#[test]
fn a_sequence_gives_several_keys() {
    let text = "- key1abcdefgh\n- \"key2abcdefgh\"\n\n";
    assert_eq!(parse(text), Ok(vec!["key1abcdefgh".to_owned(), "key2abcdefgh".to_owned()]));
}

#[test]
fn bad_files_are_refused_without_their_keys_in_the_message() {
    assert_eq!(parse(""), Err(KeyFileError::Format));
    assert_eq!(parse("\n  \n"), Err(KeyFileError::Format));
    assert_eq!(parse("abc$def ghi%"), Err(KeyFileError::InvalidChar));
    assert_eq!(parse("abc"), Err(KeyFileError::Size(3)));
    assert_eq!(parse(&"a".repeat(1025)), Err(KeyFileError::Size(1025)));
    assert_eq!(parse(&"a".repeat(1024)).map(|k| k[0].len()), Ok(1024));
    assert_eq!(parse("- goodgood\n- bad$bad"), Err(KeyFileError::InvalidChar));
    let p = "/etc/k";
    assert_eq!(
        KeyFileError::Format.message(p),
        "Only strings and sequences are supported for YAML key files"
    );
    let invalid = KeyFileError::InvalidChar.message(p);
    assert_eq!(invalid, "invalid char in key file /etc/k");
    assert!(KeyFileError::Size(3).message(p).contains("3 characters"));
}
