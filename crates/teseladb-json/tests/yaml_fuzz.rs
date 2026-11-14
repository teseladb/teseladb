//! Deterministic fuzzing of the YAML subset: random configurations written as YAML
//! read back as the same value, and arbitrary or mutated texts never panic.

use std::fmt::Write as _;

use teseladb_json::{Number, Object, Value, parse_yaml};
use teseladb_types::Rng;

fn word(rng: &mut Rng) -> String {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ_";
    (0..rng.range(1, 9)).map(|_| char::from(LETTERS[rng.below(53)])).collect()
}

/// A scalar and how to write it.
fn scalar(rng: &mut Rng) -> (Value, String) {
    match rng.range(0, 7) {
        0 => (Value::Null, ["null", "~", "Null"][rng.below(3)].to_owned()),
        1 => {
            let b = rng.chance(0.5);
            (Value::Bool(b), b.to_string())
        }
        2 => {
            let n = rng.next_u64().cast_signed() >> rng.range(0, 63);
            (Value::Number(Number::Int(n)), n.to_string())
        }
        3 => {
            let f =
                f64::from(u32::try_from(rng.range(0, 1_000_000)).expect("below 10^6")) / 64.0 + 0.5;
            (Value::Number(Number::Float(f)), format!("{f:?}"))
        }
        4 => {
            let w = word(rng);
            (Value::String(w.clone()), w)
        }
        5 => {
            let s: String = (0..rng.range(0, 8))
                .map(|_| ['a', '\'', ' ', '#', ':', 'é', '-'][rng.below(7)])
                .collect();
            (Value::String(s.clone()), format!("'{}'", s.replace('\'', "''")))
        }
        _ => {
            let s: String = (0..rng.range(0, 8))
                .map(|_| ['b', '"', '\\', '\t', '\n', '#', '🦀'][rng.below(7)])
                .collect();
            let escaped = s
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\t', "\\t")
                .replace('\n', "\\n");
            (Value::String(s), format!("\"{escaped}\""))
        }
    }
}

/// A mapping of `depth` levels at most, and its YAML at `indent`.
fn mapping(rng: &mut Rng, indent: usize, depth: u32) -> (Value, String) {
    let mut obj = Object::new();
    let mut text = String::new();
    let pad = " ".repeat(indent);
    for _ in 0..rng.range(1, 5) {
        let key = word(rng);
        if obj.get(&key).is_some() {
            continue;
        }
        let pick = if depth == 0 { rng.range(0, 3) } else { rng.range(0, 5) };
        let (v, written) = match pick {
            0 | 1 => {
                let (v, s) = scalar(rng);
                (v, format!("{pad}{key}: {s}\n"))
            }
            2 => {
                let items: Vec<(Value, String)> =
                    (0..rng.range(0, 4)).map(|_| scalar(rng)).collect();
                let flow = items.iter().map(|(_, s)| s.as_str()).collect::<Vec<_>>().join(", ");
                let v = Value::Array(items.into_iter().map(|(v, _)| v).collect());
                (v, format!("{pad}{key}: [{flow}]\n"))
            }
            3 => {
                let items: Vec<(Value, String)> =
                    (0..rng.range(1, 4)).map(|_| scalar(rng)).collect();
                let block = items.iter().fold(String::new(), |mut out, (_, s)| {
                    let _ = writeln!(out, "{pad}  - {s}");
                    out
                });
                let v = Value::Array(items.into_iter().map(|(v, _)| v).collect());
                (v, format!("{pad}{key}:\n{block}"))
            }
            _ => {
                let (v, inner) = mapping(rng, indent + 2, depth - 1);
                (v, format!("{pad}{key}:  # nested\n{inner}"))
            }
        };
        obj.insert(key, v);
        text.push_str(&written);
    }
    (Value::Object(obj), text)
}

#[test]
fn random_configurations_read_back() {
    let mut rng = Rng::new(0x7a31);
    for _ in 0..5_000 {
        let (want, text) = mapping(&mut rng, 0, 3);
        let got = parse_yaml(text.as_bytes()).unwrap_or_else(|e| panic!("{e}\n{text}"));
        assert_eq!(got, want, "{text}");
    }
}

#[test]
fn arbitrary_and_mutated_texts_never_panic() {
    let mut rng = Rng::new(99);
    let alphabet = b"ab:-[]{},'\"\\#&*!|>?~ \t\r\n0123456789.xue\xc3\xa9\xff";
    for _ in 0..20_000 {
        let n = rng.below(60);
        let doc: Vec<u8> = (0..n).map(|_| alphabet[rng.below(alphabet.len())]).collect();
        let _ = parse_yaml(&doc);
    }
    for _ in 0..5_000 {
        let mut m = mapping(&mut rng, 0, 3).1.into_bytes();
        let i = rng.below(m.len());
        match rng.range(0, 3) {
            0 => m[i] = alphabet[rng.below(alphabet.len())],
            1 => {
                m.remove(i);
            }
            _ => m.insert(i, m[i]),
        }
        let _ = parse_yaml(&m);
    }
}
