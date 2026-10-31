//! The contents of a `--keyFile` (013 FR-034), read as `refnode` 9.0.4 reads it (measured): a YAML
//! string, or a YAML sequence of strings for a change of key, each with its whitespace removed,
//! made of base64 characters and from 6 to 1024 of them long. The keys are secrets: no error
//! names them.

/// The shortest key.
pub const MIN_KEY_LEN: usize = 6;
/// The longest key.
pub const MAX_KEY_LEN: usize = 1024;

/// Why a key file was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyFileError {
    /// Neither a string nor a sequence (an empty file).
    Format,
    /// A character outside base64.
    InvalidChar,
    /// A key shorter or longer than allowed, with its length.
    Size(usize),
}

impl KeyFileError {
    /// RefDb's message for file `path`, without the key that `refnode` prints along.
    pub fn message(&self, path: &str) -> String {
        match self {
            KeyFileError::Format => {
                "Only strings and sequences are supported for YAML key files".into()
            }
            KeyFileError::InvalidChar => format!("invalid char in key file {path}"),
            KeyFileError::Size(n) => format!(
                "Security key size is out range: {path} has {n} characters, from {MIN_KEY_LEN} to {MAX_KEY_LEN} allowed"
            ),
        }
    }
}

/// A key of the file: its characters without whitespace, checked.
fn key(text: &str) -> Result<String, KeyFileError> {
    let k: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=')) {
        return Err(KeyFileError::InvalidChar);
    }
    if !(MIN_KEY_LEN..=MAX_KEY_LEN).contains(&k.len()) {
        return Err(KeyFileError::Size(k.len()));
    }
    Ok(k)
}

/// A YAML scalar without its quotes.
fn unquoted(s: &str) -> &str {
    let s = s.trim();
    for q in ['\'', '"'] {
        if let Some(inner) = s.strip_prefix(q).and_then(|r| r.strip_suffix(q)) {
            return inner;
        }
    }
    s
}

/// The keys of a key file's contents, in order; the first is the one this node uses.
pub fn parse(text: &str) -> Result<Vec<String>, KeyFileError> {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err(KeyFileError::Format);
    }
    if lines.iter().all(|l| l.trim_start().starts_with('-')) {
        return lines
            .iter()
            .map(|l| key(unquoted(l.trim_start().trim_start_matches('-'))))
            .collect();
    }
    Ok(vec![key(unquoted(text))?])
}
