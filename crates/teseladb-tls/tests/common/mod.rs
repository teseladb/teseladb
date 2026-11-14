//! The values of the RFC 8448 traces, as transcribed in `tests/data/rfc8448.tsv`: section,
//! side, step, label and hex value, in the order the RFC lists them.

use std::path::Path;

/// One value of a trace.
#[derive(Clone, Debug)]
pub struct Value {
    /// RFC 8448 section (3 to 7).
    pub section: u8,
    /// `client` or `server`.
    pub side: String,
    /// The step's description.
    pub step: String,
    /// The value's label.
    pub label: String,
    /// The bytes.
    pub bytes: Vec<u8>,
}

/// Every value of the file.
pub fn load() -> Vec<Value> {
    let text = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/rfc8448.tsv"),
    )
    .expect("read_to_string succeeds");
    text.lines()
        .map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            let bytes = (0..f[4].len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&f[4][i..i + 2], 16).expect("from_str_radix succeeds"))
                .collect();
            Value {
                section: f[0].parse().expect("parse succeeds"),
                side: f[1].into(),
                step: f[2].into(),
                label: f[3].into(),
                bytes,
            }
        })
        .collect()
}

/// The values of one section, in order.
pub fn section(all: &[Value], n: u8) -> Vec<Value> {
    all.iter().filter(|v| v.section == n).cloned().collect()
}

/// The `nth` (from 0) value of `section` with this side, step prefix and label.
pub fn find(values: &[Value], side: &str, step: &str, label: &str, nth: usize) -> Vec<u8> {
    values
        .iter()
        .filter(|v| v.side == side && v.step.starts_with(step) && v.label == label)
        .nth(nth)
        .unwrap_or_else(|| panic!("no {side} {step} {label} #{nth}"))
        .bytes
        .clone()
}
