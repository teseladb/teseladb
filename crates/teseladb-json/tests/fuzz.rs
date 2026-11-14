//! Deterministic fuzzing: the parser never panics on arbitrary or mutated inputs, and every
//! randomly generated value reads back identically after being written.

use teseladb_json::{Number, Object, Value, parse, to_string};
use teseladb_types::Rng;

fn random_value(rng: &mut Rng, depth: u32) -> Value {
    let pick = if depth > 4 { rng.range(0, 6) } else { rng.range(0, 8) };
    match pick {
        0 => Value::Null,
        1 => Value::Bool(rng.chance(0.5)),
        2 => Value::Number(Number::Int(rng.next_u64().cast_signed())),
        3 => Value::Number(Number::UInt(rng.next_u64() | 1 << 63)),
        4 => {
            let f = f64::from_bits(rng.next_u64());
            Value::Number(Number::Float(if f.is_finite() { f } else { 0.5 }))
        }
        5 => Value::String(random_string(rng)),
        6 => Value::Array((0..rng.range(0, 5)).map(|_| random_value(rng, depth + 1)).collect()),
        _ => {
            let mut o = Object::new();
            for _ in 0..rng.range(0, 5) {
                o.insert(random_string(rng), random_value(rng, depth + 1));
            }
            Value::Object(o)
        }
    }
}

fn random_string(rng: &mut Rng) -> String {
    (0..rng.range(0, 8))
        .map(|_| match rng.range(0, 4) {
            0 => char::from(rng.range(0, 0x20).to_le_bytes()[0]),
            1 => ['"', '\\', '/', 'é', '🦀', '\u{2028}'][rng.below(6)],
            _ => char::from(rng.range(0x20, 0x7f).to_le_bytes()[0]),
        })
        .collect()
}

#[test]
fn random_values_round_trip() {
    let mut rng = Rng::new(0x5eed);
    for _ in 0..5_000 {
        let v = random_value(&mut rng, 0);
        let text = to_string(&v).expect("writes");
        assert_eq!(parse(text.as_bytes()).expect("parses"), v, "{text}");
    }
}

#[test]
fn arbitrary_and_mutated_inputs_never_panic() {
    let mut rng = Rng::new(42);
    let alphabet = b"{}[]:,\"\\ntrufalsenul0123456789-+.eE \t\r\nuabcdef\xc3\xa9\xff";
    for _ in 0..20_000 {
        let n = rng.below(40);
        let doc: Vec<u8> = (0..n).map(|_| alphabet[rng.below(alphabet.len())]).collect();
        let _ = parse(&doc);
    }
    // Mutations of valid documents: one byte changed, deleted or duplicated.
    for _ in 0..5_000 {
        let text = to_string(&random_value(&mut rng, 0)).expect("writes").into_bytes();
        if text.is_empty() {
            continue;
        }
        let mut m = text.clone();
        let i = rng.below(m.len());
        match rng.range(0, 3) {
            0 => m[i] = alphabet[rng.below(alphabet.len())],
            1 => {
                m.remove(i);
            }
            _ => m.insert(i, m[i]),
        }
        if let Ok(v) = parse(&m) {
            // If it is still valid, it is also written and read back identically.
            assert_eq!(parse(to_string(&v).expect("writes").as_bytes()).expect("parses"), v);
        }
    }
}
