//! Whether a validated document is canonical BSON, as `Document::to_bytes` writes it: arrays
//! name their elements by position from 0, regular expression options are in alphabetical order
//! and it nests within `MAX_COMMAND_DEPTH` levels. Such a document can be kept as its bytes and
//! written back by copying them (003 FR-020).

use teseladb_bson::{MAX_COMMAND_DEPTH, RawDoc, RawValue};

/// Whether `doc` (validated) is written back unchanged by `Document::to_bytes`.
pub(crate) fn is_canonical(doc: RawDoc<'_>) -> bool {
    canonical_doc(doc, false, 1)
}

fn canonical_doc(doc: RawDoc<'_>, array: bool, depth: usize) -> bool {
    if depth > MAX_COMMAND_DEPTH {
        return false;
    }
    let mut expected = 0usize;
    for (name, value) in doc.iter() {
        if array {
            if !is_position(name, expected) {
                return false;
            }
            expected += 1;
        }
        if !canonical_value(value, depth) {
            return false;
        }
    }
    true
}

/// Whether `name` is the decimal text of `position`.
fn is_position(name: &str, position: usize) -> bool {
    let mut buf = [0u8; 20];
    let mut n = position;
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    name.as_bytes() == &buf[at..]
}

fn canonical_value(value: RawValue<'_>, depth: usize) -> bool {
    match value {
        RawValue::Document(d) => canonical_doc(d, false, depth + 1),
        RawValue::Array(a) => canonical_doc(a.as_doc(), true, depth + 1),
        RawValue::JavaScriptWithScope { scope, .. } => canonical_doc(scope, false, depth + 1),
        RawValue::Regex { options, .. } => options.as_bytes().is_sorted(),
        _ => true,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_bson::{Document, RawDoc, Value};

    use super::{is_canonical, is_position};

    /// A document's BSON with element `bytes` (type byte, name and value) as its only field.
    fn with_element(bytes: &[u8]) -> Vec<u8> {
        let len = u32::try_from(bytes.len() + 5).expect("small");
        let mut out = len.to_le_bytes().to_vec();
        out.extend_from_slice(bytes);
        out.push(0);
        out
    }

    #[test]
    fn what_the_writer_writes_is_canonical() {
        let doc = Document::new()
            .with("a", Value::Array(vec![Value::Int32(1), Value::Array(vec![Value::Null])]))
            .with("r", Value::Regex { pattern: "x".into(), options: "mis".into() })
            .with("d", Document::new().with("b", 2.5));
        let bytes = doc.to_bytes().expect("small");
        assert!(is_canonical(RawDoc::from_bytes(&bytes).expect("valid")));
    }

    #[test]
    fn an_array_with_other_names_is_not() {
        // {a: ["x" at name "1"]}: an array whose first element is not named 0.
        let inner = with_element(&[0x10, b'1', 0, 7, 0, 0, 0]);
        let mut element = vec![0x04, b'a', 0];
        element.extend_from_slice(&inner);
        let bytes = with_element(&element);
        assert!(!is_canonical(RawDoc::from_bytes(&bytes).expect("valid")));
    }

    #[test]
    fn regex_options_out_of_order_are_not() {
        let bytes = with_element(&[0x0B, b'r', 0, b'x', 0, b's', b'i', 0]);
        assert!(!is_canonical(RawDoc::from_bytes(&bytes).expect("valid")));
    }

    #[test]
    fn positions_are_their_decimal_text() {
        assert!(is_position("0", 0));
        assert!(is_position("10", 10));
        assert!(!is_position("01", 1));
        assert!(!is_position("1", 2));
    }
}
