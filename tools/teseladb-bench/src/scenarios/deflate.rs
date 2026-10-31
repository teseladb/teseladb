//! `deflate` scenario: MB/s of inflating and of compressing per level over a corpus of
//! text resembling source code (repetitions at varied distances), with the targets of
//! 003 R7. Each timing is one pass over the whole corpus (inflating, the best of three), without a
//! warm-up (`--warmup`).

use std::time::Instant;

use teseladb_deflate::{deflate, inflate};
use teseladb_types::Rng;

use crate::cli::Args;
use crate::report::{Report, Target};

/// Corpus of `len` bytes: words from a small vocabulary, indentation and repeated lines.
pub fn corpus(len: usize) -> Vec<u8> {
    let words: Vec<&[u8]> = [
        "fn ", "let ", "mut ", "self", ".push(", "return ", "if ", "else ", "match ", "=> ",
        "Some(", "None", "Ok(", "Err(", "u64", "usize", "&[u8]", "{\n", "}\n", "    ", "// ",
        "pub ", "struct ", "impl ", "for ", "in ", "0..", "len()", "x", "y", "data", "buf", "out",
        "pos", " = ", ";\n",
    ]
    .iter()
    .map(|w| w.as_bytes())
    .collect();
    let mut rng = Rng::new(1);
    let mut out: Vec<u8> = Vec::with_capacity(len + 64);
    while out.len() < len {
        if out.len() > 4096 && rng.chance(0.15) {
            // Repeat an earlier line.
            let d = rng.range(1, (out.len() as u64).min(30_000)) as usize;
            let n = rng.range(10, 80) as usize;
            let from = out.len() - d;
            for k in 0..n {
                let b = out[from + k];
                out.push(b);
            }
        } else {
            out.extend_from_slice(words[rng.range(0, words.len() as u64) as usize]);
        }
    }
    out.truncate(len);
    out
}

fn mb_s(bytes: usize, secs: f64) -> f64 {
    bytes as f64 / 1e6 / secs.max(1e-9)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let mut report = Report::new("deflate");
    let data = corpus((args.records as usize).clamp(1 << 20, 256 << 20).min(64 << 20));
    for level in [1u8, 6, 9] {
        let t0 = Instant::now();
        let c = deflate(&data, level);
        let secs = t0.elapsed().as_secs_f64();
        let target = (level == 1).then_some(Target::AtLeast(150.0));
        report.push(&format!("deflate.l{level}_mb_per_s"), "MB/s", mb_s(data.len(), secs), target);
        report.push(
            &format!("deflate.l{level}_ratio"),
            "x",
            data.len() as f64 / c.len() as f64,
            None,
        );
        if level == 6 {
            // Best of three for inflating.
            let mut best = f64::MAX;
            for _ in 0..3 {
                let t0 = Instant::now();
                let back = inflate(&c).expect("our own stream inflates");
                best = best.min(t0.elapsed().as_secs_f64());
                assert!(back == data, "the round trip gives the data back");
            }
            report.push(
                "inflate_mb_per_s",
                "MB/s",
                mb_s(data.len(), best),
                Some(Target::AtLeast(400.0)),
            );
        }
    }
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn small_run() {
        let r = run(&Args { scenario: "deflate".into(), records: 1, ..Args::default() }).unwrap();
        assert!(r.metrics.iter().any(|m| m.name == "inflate_mb_per_s"));
        assert!(r.metrics.iter().any(|m| m.name == "deflate.l9_ratio" && m.value > 2.0));
        assert_eq!(corpus(1000).len(), 1000);
    }
}
