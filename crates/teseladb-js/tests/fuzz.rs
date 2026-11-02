//! Deterministic fuzzing of the parser and the interpreter (016 FR-011, T009): token soups and
//! mutations of the measured expressions are compiled and, when they compile, run with small
//! limits. Nothing may panic, and every call ends within its limits. `TESELADB_FUZZ_SECS` runs
//! it for that long (the 24-hour run of SC-003), `TESELADB_FUZZ_SEED` replays one seed.

use std::time::{Duration, Instant};

use teseladb_js::{JsCtx, Limits, Program};
use teseladb_types::Rng;

const CORPUS: &str = include_str!("measured/expressions.tsv");
const ROUNDS: u64 = 3_000;

const TOKENS: &[&str] = &[
    "function",
    "(",
    ")",
    "{",
    "}",
    "[",
    "]",
    ";",
    ",",
    ".",
    "?.",
    "=>",
    "=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "**",
    "++",
    "--",
    "==",
    "===",
    "!",
    "&&",
    "||",
    "??",
    "?",
    ":",
    "<",
    ">>",
    "~",
    "var",
    "let",
    "const",
    "if",
    "else",
    "for",
    "of",
    "in",
    "while",
    "do",
    "return",
    "throw",
    "try",
    "catch",
    "finally",
    "switch",
    "case",
    "default",
    "break",
    "continue",
    "new",
    "class",
    "extends",
    "super",
    "this",
    "yield",
    "function*",
    "async",
    "await",
    "typeof",
    "delete",
    "void",
    "x",
    "y",
    "a",
    "obj",
    "1",
    "0",
    "1n",
    "-1",
    "1e309",
    "0x1f",
    "'s'",
    "\"q\"",
    "`t${x}`",
    "/a+/g",
    "null",
    "undefined",
    "true",
    "NaN",
    "Infinity",
    "[]",
    "{}",
    "...",
    "Proxy",
    "Symbol",
    "BigInt",
    "Promise",
    "Array",
    "Object",
    "JSON",
    "Math",
    "Map",
    "Set",
    "WeakMap",
    "Uint8Array",
    "DataView",
    "ArrayBuffer",
    "Iterator",
    "Error",
    "Date",
    "RegExp",
    "String",
    "Number",
    "NumberLong",
    "ObjectId",
    "print",
    "sleep",
    "\n",
    " ",
];

fn soup(rng: &mut Rng) -> String {
    (0..rng.range(1, 30))
        .map(|_| TOKENS[rng.range(0, TOKENS.len() as u64) as usize])
        .collect::<Vec<_>>()
        .join(" ")
}

fn mutate(rng: &mut Rng, src: &str) -> String {
    let mut chars: Vec<char> = src.chars().collect();
    for _ in 0..rng.range(1, 4) {
        if chars.is_empty() {
            break;
        }
        let i = rng.range(0, chars.len() as u64) as usize;
        match rng.range(0, 4) {
            0 => {
                chars.remove(i);
            }
            1 => chars.insert(i, chars[i]),
            2 => {
                let t = TOKENS[rng.range(0, TOKENS.len() as u64) as usize];
                for (k, c) in t.chars().enumerate() {
                    chars.insert(i + k, c);
                }
            }
            _ => {
                chars[i] = ['(', ')', '{', '}', '[', ']', ';', ',', '`', '\'', '"', '\\']
                    [rng.range(0, 12) as usize]
            }
        }
    }
    chars.into_iter().collect()
}

fn run(src: &str) {
    for body in [
        format!("function(){{ {src}\n}}"),
        format!("function(){{ return ({src}) }}"),
        src.to_owned(),
    ] {
        let Ok(p) = Program::compile(&body) else { continue };
        let mut ctx = JsCtx {
            limits: Limits { steps: 200_000, heap: 4 << 20, depth: 64 },
            ..JsCtx::default()
        };
        let _ = p.call(None, &[], &mut ctx);
    }
}

#[test]
fn parser_and_interpreter_survive_arbitrary_and_mutated_code() {
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let replay: Option<u64> = std::env::var("TESELADB_FUZZ_SEED").ok().and_then(|s| s.parse().ok());
    let corpus: Vec<&str> =
        CORPUS.lines().filter_map(|l| l.split_once('\t').map(|(e, _)| e)).collect();
    let start = Instant::now();
    let mut seed = replay.unwrap_or(0x5eed_0016);
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for _ in 0..100 {
            let src = if rng.chance(0.5) {
                soup(&mut rng)
            } else {
                let i = rng.range(0, corpus.len() as u64) as usize;
                mutate(&mut rng, corpus[i])
            };
            // `TESELADB_FUZZ_LOG` names a file that holds the input running, to find a finding.
            if let Ok(path) = std::env::var("TESELADB_FUZZ_LOG") {
                std::fs::write(path, &src).expect("the log file");
            }
            run(&src);
            rounds += 1;
        }
        if replay.is_some() {
            break;
        }
        match secs {
            Some(s) if start.elapsed() < Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < ROUNDS => seed += 1,
            None => break,
        }
    }
    eprintln!("fuzz: {rounds} inputs in {:?}", start.elapsed());
}
