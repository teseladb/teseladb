//! The reference search library's `SloppyMath` haversine distance, which `near` on `geo` fields scores with: `cos`
//! and `asin` from tables with a fourth-order Taylor step, so distances (and scores) come out
//! as the reference search library's do rather than as the exact formula's.

use std::sync::OnceLock;

const TO_METERS: f64 = 6_371_008.771_4;
const ONE_DIV_F2: f64 = 1.0 / 2.0;
const ONE_DIV_F3: f64 = 1.0 / 6.0;
const ONE_DIV_F4: f64 = 1.0 / 24.0;
const SIN_COS_TABS_SIZE: usize = (1 << 11) + 1;
const ASIN_TABS_SIZE: usize = (1 << 13) + 1;

struct Tables {
    delta_hi: f64,
    delta_lo: f64,
    indexer: f64,
    max_for_modulo: f64,
    sin: Vec<f64>,
    cos: Vec<f64>,
    asin_max: f64,
    asin_delta: f64,
    asin_indexer: f64,
    asin: Vec<[f64; 5]>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let pio2_hi = f64::from_bits(0x3FF9_21FB_5440_0000);
        let pio2_lo = f64::from_bits(0x3DD0_B461_1A62_6331);
        let (twopi_hi, twopi_lo) = (4.0 * pio2_hi, 4.0 * pio2_lo);
        let delta_hi = twopi_hi / (SIN_COS_TABS_SIZE - 1) as f64;
        let delta_lo = twopi_lo / (SIN_COS_TABS_SIZE - 1) as f64;
        let indexer = 1.0 / (delta_hi + delta_lo);
        let max_for_modulo = (f64::from(i32::MAX >> 9) / indexer) * 0.99;
        let mut sin = Vec::with_capacity(SIN_COS_TABS_SIZE);
        let mut cos = Vec::with_capacity(SIN_COS_TABS_SIZE);
        for i in 0..SIN_COS_TABS_SIZE {
            let angle = i as f64 * delta_hi + i as f64 * delta_lo;
            let (mut s, mut c) = (angle.sin(), angle.cos());
            // The angles of a null sine or cosine get an exact zero.
            if i == SIN_COS_TABS_SIZE / 4 {
                c = 0.0;
            } else if i == 2 * SIN_COS_TABS_SIZE / 4 {
                s = 0.0;
            } else if i == 3 * SIN_COS_TABS_SIZE / 4 {
                c = 0.0;
            }
            sin.push(s);
            cos.push(c);
        }
        let asin_max = 73.0f64.to_radians().sin();
        let asin_delta = asin_max / (ASIN_TABS_SIZE - 1) as f64;
        let asin = (0..ASIN_TABS_SIZE)
            .map(|i| {
                let x = i as f64 * asin_delta;
                let inv = 1.0 / (1.0 - x * x);
                let inv0_5 = inv.sqrt();
                let inv1_5 = inv0_5 * inv;
                let inv2_5 = inv1_5 * inv;
                let inv3_5 = inv2_5 * inv;
                [
                    x.asin(),
                    inv0_5,
                    (x * inv1_5) * ONE_DIV_F2,
                    ((1.0 + 2.0 * x * x) * inv2_5) * ONE_DIV_F3,
                    ((5.0 + 2.0 * x * (2.0 + x * (5.0 - 2.0 * x))) * inv3_5) * ONE_DIV_F4,
                ]
            })
            .collect();
        Tables {
            delta_hi,
            delta_lo,
            indexer,
            max_for_modulo,
            sin,
            cos,
            asin_max,
            asin_delta,
            asin_indexer: 1.0 / asin_delta,
            asin,
        }
    })
}

/// The reference search library's `SloppyMath.cos`.
fn cos(a: f64) -> f64 {
    let t = tables();
    let a = a.abs();
    if a > t.max_for_modulo {
        return a.cos();
    }
    let index = (a * t.indexer + 0.5) as usize;
    let delta = (a - index as f64 * t.delta_hi) - index as f64 * t.delta_lo;
    let index = index & (SIN_COS_TABS_SIZE - 2);
    let (c, s) = (t.cos[index], t.sin[index]);
    c + delta * (-s + delta * (-c * ONE_DIV_F2 + delta * (s * ONE_DIV_F3 + delta * c * ONE_DIV_F4)))
}

/// The reference search library's `SloppyMath.asin` for `0 <= a <= 1`.
fn asin(a: f64) -> f64 {
    let t = tables();
    if a > t.asin_max {
        return a.asin();
    }
    let index = (a * t.asin_indexer + 0.5) as usize;
    let delta = a - index as f64 * t.asin_delta;
    let [v, d1, d2, d3, d4] = t.asin[index];
    v + delta * (d1 + delta * (d2 + delta * (d3 + delta * d4)))
}

/// The meters between two points (latitude and longitude in degrees), as the reference search library measures them.
pub(super) fn haversin_meters(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let (x1, x2) = (lat1.to_radians(), lat2.to_radians());
    let h1 = 1.0 - cos(x1 - x2);
    let h2 = 1.0 - cos((lon1 - lon2).to_radians());
    let h = h1 + cos(x1) * cos(x2) * h2;
    TO_METERS * 2.0 * asin((h * 0.5).sqrt().min(1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sloppy_functions_stay_close_to_the_exact_ones() {
        for a in [0.0, 0.1, 1.0, 2.0, -3.0, 6.0, 1e6] {
            assert!((cos(a) - a.cos()).abs() < 1e-12, "{a}");
        }
        for a in [0.0, 0.2, 0.5, 0.95, 0.99, 1.0] {
            assert!((asin(a) - a.asin()).abs() < 1e-12, "{a}");
        }
        let d = haversin_meters(40.42, -3.70, 40.43, -3.70);
        assert!((d - 1111.95).abs() < 0.01, "{d}");
    }
}
