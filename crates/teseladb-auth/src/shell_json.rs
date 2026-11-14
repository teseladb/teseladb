//! JSON as the reference server reads it from its options and configuration (`--auditFilter`,
//! `security.ldap.userToDNMapping`): keys without quotes and strings in single quotes, as the shell
//! writes them, turned into strict JSON for the Extended JSON reader.

/// `text` with every key written without quotes quoted (`{a: 1}` as `{"a": 1}`) and every
/// string in single quotes in double ones; the rest as it is.
pub fn quote_keys(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len() + 8);
    let mut i = 0;
    // Whether a key may start here: after `{` or `,`, outside any string.
    let mut key_next = false;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' | '\'' => {
                let mut j = i + 1;
                let mut body = String::new();
                while j < chars.len() && chars[j] != c {
                    match (chars[j], chars.get(j + 1)) {
                        // `\'` is a quote of a single-quoted string; JSON has no such escape.
                        ('\\', Some('\'')) => {
                            body.push('\'');
                            j += 1;
                        }
                        ('\\', Some(&next)) => {
                            body.push('\\');
                            body.push(next);
                            j += 1;
                        }
                        // Only in a single-quoted string: the loop ends at the closing quote.
                        ('"', _) => body.push_str("\\\""),
                        (other, _) => body.push(other),
                    }
                    j += 1;
                }
                out.push('"');
                out.push_str(&body);
                if j < chars.len() {
                    out.push('"');
                }
                i = j + 1;
                key_next = false;
                continue;
            }
            '{' | ',' => key_next = true,
            c if key_next && is_key_char(c) => {
                let mut j = i;
                while j < chars.len() && is_key_char(chars[j]) {
                    j += 1;
                }
                let mut k = j;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                if chars.get(k) == Some(&':') {
                    out.push('"');
                    out.extend(&chars[i..j]);
                    out.push('"');
                    i = j;
                    key_next = false;
                    continue;
                }
                key_next = false;
            }
            c if c.is_whitespace() => {}
            _ => key_next = false,
        }
        out.push(c);
        i += 1;
    }
    out
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '.'
}
