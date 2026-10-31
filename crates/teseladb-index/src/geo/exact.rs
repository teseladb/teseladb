//! Exact sign of a 3×3 determinant of doubles, with floating-point expansions (sums of
//! non-overlapping doubles, after Shewchuk's «Adaptive Precision Floating-Point Arithmetic and
//! Fast Robust Geometric Predicates», 1997). The orientation of three points on the sphere is the
//! sign of `a · (b × c)`; near zero the rounded product cannot be trusted and this one decides.

use std::cmp::Ordering;

/// `a + b` as a rounded sum and its exact error.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let s = a + b;
    let bv = s - a;
    let av = s - bv;
    (s, (a - av) + (b - bv))
}

/// `a · b` as a rounded product and its exact error (the fused multiply-add is exact).
fn two_product(a: f64, b: f64) -> (f64, f64) {
    let p = a * b;
    (p, a.mul_add(b, -p))
}

/// Adds `b` to expansion `e` (components in increasing magnitude).
fn grow(e: &[f64], b: f64) -> Vec<f64> {
    let mut q = b;
    let mut out = Vec::with_capacity(e.len() + 1);
    for &x in e {
        let (s, err) = two_sum(q, x);
        out.push(err);
        q = s;
    }
    out.push(q);
    out
}

/// Expansion `e` times `b`.
fn scale(e: &[f64], b: f64) -> Vec<f64> {
    let mut out = Vec::with_capacity(2 * e.len());
    for &x in e {
        let (p, err) = two_product(x, b);
        out = grow(&out, err);
        out = grow(&out, p);
    }
    out
}

/// The exact sign of `x·y·z`, summed over the signed terms.
fn exact_sign(terms: &[(f64, f64, f64, bool)]) -> Ordering {
    let mut sum: Vec<f64> = Vec::new();
    for &(x, y, z, negative) in terms {
        let (p, err) = two_product(x, y);
        let product = scale(&[err, p], z);
        for c in product {
            sum = grow(&sum, if negative { -c } else { c });
        }
    }
    let top = sum.iter().rev().find(|c| **c != 0.0).copied().unwrap_or(0.0);
    top.partial_cmp(&0.0).unwrap_or(Ordering::Equal)
}

/// The sign of `a · (b × c)` with the three points moved by the same infinitesimal vector
/// `(ε, ε², ε³)` when they are exactly coplanar with the origin (simulation of simplicity,
/// Edelsbrunner and Mücke, 1990): the first non-zero component of `a×b + b×c + c×a`, that is,
/// the turn of the three points projected on the y-z, z-x and x-y planes in that order. Only
/// repeated points give `Equal`.
pub(crate) fn perturbed_det_sign(a: [f64; 3], b: [f64; 3], c: [f64; 3]) -> Ordering {
    let exact = det_sign(a, b, c);
    if exact != Ordering::Equal || a == b || b == c || c == a {
        return exact;
    }
    for (i, j) in [(1, 2), (2, 0), (0, 1)] {
        let turn = exact_sign(&[
            (a[i], b[j], 1.0, false),
            (a[j], b[i], 1.0, true),
            (b[i], c[j], 1.0, false),
            (b[j], c[i], 1.0, true),
            (c[i], a[j], 1.0, false),
            (c[j], a[i], 1.0, true),
        ]);
        if turn != Ordering::Equal {
            return turn;
        }
    }
    Ordering::Equal
}

/// Above this the rounded determinant of unit-length vectors has the right sign.
const TRUSTED: f64 = 1e-14;

/// The sign of `a · (b × c)`: positive when `a, b, c` turn counterclockwise around the origin.
pub(crate) fn det_sign(a: [f64; 3], b: [f64; 3], c: [f64; 3]) -> Ordering {
    let det = a[0] * (b[1] * c[2] - b[2] * c[1])
        + a[1] * (b[2] * c[0] - b[0] * c[2])
        + a[2] * (b[0] * c[1] - b[1] * c[0]);
    if det.abs() > TRUSTED {
        return det.partial_cmp(&0.0).unwrap_or(Ordering::Equal);
    }
    exact_sign(&[
        (a[0], b[1], c[2], false),
        (a[0], b[2], c[1], true),
        (a[1], b[2], c[0], false),
        (a[1], b[0], c[2], true),
        (a[2], b[0], c[1], false),
        (a[2], b[1], c[0], true),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_of_clear_and_degenerate_determinants() {
        let x = [1.0, 0.0, 0.0];
        let y = [0.0, 1.0, 0.0];
        let z = [0.0, 0.0, 1.0];
        assert_eq!(det_sign(x, y, z), Ordering::Greater);
        assert_eq!(det_sign(y, x, z), Ordering::Less);
        assert_eq!(det_sign(x, x, z), Ordering::Equal);
        // Three nearly collinear points: the rounded determinant is 0 but the exact one is not.
        let a = [1.0, 1e-20, 0.0];
        let b = [1.0, 0.0, 1e-20];
        let c = [1.0, 0.0, 0.0];
        assert_eq!(det_sign(a, b, c), Ordering::Greater);
        assert_eq!(det_sign(b, a, c), Ordering::Less);
        // Exactly collinear in the plane z = 0 with inexact products.
        let p = [0.1, 0.2, 0.0];
        let q = [0.3, 0.7, 0.0];
        let r = [0.9, 0.1, 0.0];
        assert_eq!(det_sign(p, q, r), Ordering::Equal);
    }

    #[test]
    fn perturbation_breaks_exact_ties_consistently() {
        let (a, b, c) = ([1.0, 0.0, 0.0], [0.6, 0.8, 0.0], [0.0, 1.0, 0.0]);
        assert_eq!(det_sign(a, b, c), Ordering::Equal);
        let s = perturbed_det_sign(a, b, c);
        assert_ne!(s, Ordering::Equal);
        assert_eq!(perturbed_det_sign(b, a, c), s.reverse());
        assert_eq!(perturbed_det_sign(b, c, a), s);
        assert_eq!(perturbed_det_sign(a, a, c), Ordering::Equal);
        // Coplanar in x = 0 and in y = 0: decided by the first projection that turns.
        assert_ne!(
            perturbed_det_sign([0.0, 1.0, 0.0], [0.0, 0.6, 0.8], [0.0, 0.0, 1.0]),
            Ordering::Equal
        );
        assert_ne!(
            perturbed_det_sign([1.0, 0.0, 0.0], [0.6, 0.0, 0.8], [0.0, 0.0, 1.0]),
            Ordering::Equal
        );
        // Not coplanar: the exact sign.
        assert_eq!(
            perturbed_det_sign([1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]),
            Ordering::Greater
        );
        // Three distinct points on one line through the origin: every projection is flat.
        assert_eq!(
            perturbed_det_sign([1.0, 1.0, 1.0], [2.0, 2.0, 2.0], [3.0, 3.0, 3.0]),
            Ordering::Equal
        );
    }

    #[test]
    fn expansions_keep_every_bit() {
        assert_eq!(two_sum(1.0, 1e-30), (1.0, 1e-30));
        let (p, e) = two_product(1.0 + f64::EPSILON, 1.0 + f64::EPSILON);
        assert_eq!(p, 1.0 + 2.0 * f64::EPSILON);
        assert_eq!(e, f64::EPSILON * f64::EPSILON);
        assert_eq!(exact_sign(&[(1.0, 1.0, 1.0, false), (1.0, 1.0, 1.0, true)]), Ordering::Equal);
        assert_eq!(exact_sign(&[(f64::NAN, 1.0, 1.0, false)]), Ordering::Equal);
    }
}
