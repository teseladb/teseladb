//! Normalization Form D (UAX #15): canonical decomposition, applied recursively, with Hangul
//! syllables decomposed by arithmetic (Unicode §3.12), and then canonical ordering: each run of
//! non-starters sorted by combining class, keeping the order of equal classes.

use crate::elements::Mappings;
use crate::root::Root;

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = 21 * T_COUNT;
const S_COUNT: u32 = 19 * N_COUNT;

/// `cps` in NFD. Any code point is accepted: a lone surrogate does not decompose and gets an
/// implicit weight, as in ICU.
pub(crate) fn nfd(root: &Root, cps: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut out = Vec::new();
    for cp in cps {
        decompose(root, cp, &mut out);
    }
    reorder(root, &mut out);
    out
}

/// `cps` with each code point decomposed but no canonical reordering.
pub(crate) fn decompose_all(root: &Root, cps: impl IntoIterator<Item = u32>) -> Vec<u32> {
    let mut out = Vec::new();
    for cp in cps {
        decompose(root, cp, &mut out);
    }
    out
}

fn decompose(root: &Root, cp: u32, out: &mut Vec<u32>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let s = cp - S_BASE;
        out.push(L_BASE + s / N_COUNT);
        out.push(V_BASE + (s % N_COUNT) / T_COUNT);
        if !s.is_multiple_of(T_COUNT) {
            out.push(T_BASE + s % T_COUNT);
        }
        return;
    }
    match root.decomposition(cp) {
        Some(to) => {
            for c in to {
                decompose(root, *c, out);
            }
        }
        None => out.push(cp),
    }
}

/// Sorts every run of non-starters by combining class, stably.
fn reorder(root: &Root, cps: &mut [u32]) {
    let mut i = 0;
    while i < cps.len() {
        if root.combining_class(cps[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < cps.len() && root.combining_class(cps[i]) != 0 {
            i += 1;
        }
        cps[start..i].sort_by_key(|cp| root.combining_class(*cp));
    }
}
