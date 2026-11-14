//! The `granularity` of `$bucketAuto` (measured on a reference router 9.0.4): bucket boundaries rounded
//! to a series of preferred numbers (Renard R5–R80, `1-2-5`, IEC E6–E192) or to powers of two.
//!
//! A series is kept as whole numbers (R5 as `10, 16, 25, 40, 63`) and scaled by a power of ten
//! built by repeated division or multiplication by 10, as the reference server does; the rounding of doubles
//! that gives (`63 × 0.1` is `6.300000000000001`) is part of the result. Decimals go through the
//! same steps in decimal, each series value converted as a double of 15 digits.

use teseladb_bson::{Decimal, Decimal128, Value};
use teseladb_mql::{DecOp, MqlError, dec_binary, to_decimal128};

use super::errors::error;
use crate::expr::arith::type_name_of;
use crate::expr::order;

/// `Unknown rounding granularity`.
const UNKNOWN: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 40257, name: "Location40257" };
/// A value that is not a number.
const NOT_NUMBER: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 40258, name: "Location40258" };
/// A negative value.
const NEGATIVE: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 40260, name: "Location40260" };

const R5: [f64; 5] = [10.0, 16.0, 25.0, 40.0, 63.0];
const R10: [f64; 10] = [100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0];
const R20: [f64; 20] = [
    100.0, 112.0, 125.0, 140.0, 160.0, 180.0, 200.0, 224.0, 250.0, 280.0, 315.0, 355.0, 400.0,
    450.0, 500.0, 560.0, 630.0, 710.0, 800.0, 900.0,
];
const R40: [f64; 40] = [
    100.0, 106.0, 112.0, 118.0, 125.0, 132.0, 140.0, 150.0, 160.0, 170.0, 180.0, 190.0, 200.0,
    212.0, 224.0, 236.0, 250.0, 265.0, 280.0, 300.0, 315.0, 335.0, 355.0, 375.0, 400.0, 425.0,
    450.0, 475.0, 500.0, 530.0, 560.0, 600.0, 630.0, 670.0, 710.0, 750.0, 800.0, 850.0, 900.0,
    950.0,
];
const R80: [f64; 80] = [
    100.0, 103.0, 106.0, 109.0, 112.0, 115.0, 118.0, 122.0, 125.0, 128.0, 132.0, 136.0, 140.0,
    145.0, 150.0, 155.0, 160.0, 165.0, 170.0, 175.0, 180.0, 185.0, 190.0, 195.0, 200.0, 206.0,
    212.0, 218.0, 224.0, 230.0, 236.0, 243.0, 250.0, 258.0, 265.0, 272.0, 280.0, 290.0, 300.0,
    307.0, 315.0, 325.0, 335.0, 345.0, 355.0, 365.0, 375.0, 387.0, 400.0, 412.0, 425.0, 437.0,
    450.0, 462.0, 475.0, 487.0, 500.0, 515.0, 530.0, 545.0, 560.0, 580.0, 600.0, 615.0, 630.0,
    650.0, 670.0, 690.0, 710.0, 730.0, 750.0, 775.0, 800.0, 825.0, 850.0, 875.0, 900.0, 925.0,
    950.0, 975.0,
];
const ONE_TWO_FIVE: [f64; 3] = [10.0, 20.0, 50.0];
const E6: [f64; 6] = [10.0, 15.0, 22.0, 33.0, 47.0, 68.0];
const E12: [f64; 12] = [10.0, 12.0, 15.0, 18.0, 22.0, 27.0, 33.0, 39.0, 47.0, 56.0, 68.0, 82.0];
const E24: [f64; 24] = [
    10.0, 11.0, 12.0, 13.0, 15.0, 16.0, 18.0, 20.0, 22.0, 24.0, 27.0, 30.0, 33.0, 36.0, 39.0, 43.0,
    47.0, 51.0, 56.0, 62.0, 68.0, 75.0, 82.0, 91.0,
];
const E48: [f64; 48] = [
    100.0, 105.0, 110.0, 115.0, 121.0, 127.0, 133.0, 140.0, 147.0, 154.0, 162.0, 169.0, 178.0,
    187.0, 196.0, 205.0, 215.0, 226.0, 237.0, 249.0, 261.0, 274.0, 287.0, 301.0, 316.0, 332.0,
    348.0, 365.0, 383.0, 402.0, 422.0, 442.0, 464.0, 487.0, 511.0, 536.0, 562.0, 590.0, 619.0,
    649.0, 681.0, 715.0, 750.0, 787.0, 825.0, 866.0, 909.0, 953.0,
];
const E96: [f64; 96] = [
    100.0, 102.0, 105.0, 107.0, 110.0, 113.0, 115.0, 118.0, 121.0, 124.0, 127.0, 130.0, 133.0,
    137.0, 140.0, 143.0, 147.0, 150.0, 154.0, 158.0, 162.0, 165.0, 169.0, 174.0, 178.0, 182.0,
    187.0, 191.0, 196.0, 200.0, 205.0, 210.0, 215.0, 221.0, 226.0, 232.0, 237.0, 243.0, 249.0,
    255.0, 261.0, 267.0, 274.0, 280.0, 287.0, 294.0, 301.0, 309.0, 316.0, 324.0, 332.0, 340.0,
    348.0, 357.0, 365.0, 374.0, 383.0, 392.0, 402.0, 412.0, 422.0, 432.0, 442.0, 453.0, 464.0,
    475.0, 487.0, 499.0, 511.0, 523.0, 536.0, 549.0, 562.0, 576.0, 590.0, 604.0, 619.0, 634.0,
    649.0, 665.0, 681.0, 698.0, 715.0, 732.0, 750.0, 768.0, 787.0, 806.0, 825.0, 845.0, 866.0,
    887.0, 909.0, 931.0, 953.0, 976.0,
];
const E192: [f64; 192] = [
    100.0, 101.0, 102.0, 104.0, 105.0, 106.0, 107.0, 109.0, 110.0, 111.0, 113.0, 114.0, 115.0,
    117.0, 118.0, 120.0, 121.0, 123.0, 124.0, 126.0, 127.0, 129.0, 130.0, 132.0, 133.0, 135.0,
    137.0, 138.0, 140.0, 142.0, 143.0, 145.0, 147.0, 149.0, 150.0, 152.0, 154.0, 156.0, 158.0,
    160.0, 162.0, 164.0, 165.0, 167.0, 169.0, 172.0, 174.0, 176.0, 178.0, 180.0, 182.0, 184.0,
    187.0, 189.0, 191.0, 193.0, 196.0, 198.0, 200.0, 203.0, 205.0, 208.0, 210.0, 213.0, 215.0,
    218.0, 221.0, 223.0, 226.0, 229.0, 232.0, 234.0, 237.0, 240.0, 243.0, 246.0, 249.0, 252.0,
    255.0, 258.0, 261.0, 264.0, 267.0, 271.0, 274.0, 277.0, 280.0, 284.0, 287.0, 291.0, 294.0,
    298.0, 301.0, 305.0, 309.0, 312.0, 316.0, 320.0, 324.0, 328.0, 332.0, 336.0, 340.0, 344.0,
    348.0, 352.0, 357.0, 361.0, 365.0, 370.0, 374.0, 379.0, 383.0, 388.0, 392.0, 397.0, 402.0,
    407.0, 412.0, 417.0, 422.0, 427.0, 432.0, 437.0, 442.0, 448.0, 453.0, 459.0, 464.0, 470.0,
    475.0, 481.0, 487.0, 493.0, 499.0, 505.0, 511.0, 517.0, 523.0, 530.0, 536.0, 542.0, 549.0,
    556.0, 562.0, 569.0, 576.0, 583.0, 590.0, 597.0, 604.0, 612.0, 619.0, 626.0, 634.0, 642.0,
    649.0, 657.0, 665.0, 673.0, 681.0, 690.0, 698.0, 706.0, 715.0, 723.0, 732.0, 741.0, 750.0,
    759.0, 768.0, 777.0, 787.0, 796.0, 806.0, 816.0, 825.0, 835.0, 845.0, 856.0, 866.0, 876.0,
    887.0, 898.0, 909.0, 920.0, 931.0, 942.0, 953.0, 965.0, 976.0, 988.0,
];

/// How boundaries are rounded.
#[derive(Clone, Copy, Debug)]
pub enum Granularity {
    /// A series of preferred numbers.
    Series(&'static [f64]),
    /// Powers of two.
    PowersOf2,
}

impl Granularity {
    /// The granularity named `name`, or the reference server's 40257.
    pub fn named(name: &str) -> Result<Granularity, MqlError> {
        Ok(match name {
            "R5" => Granularity::Series(&R5),
            "R10" => Granularity::Series(&R10),
            "R20" => Granularity::Series(&R20),
            "R40" => Granularity::Series(&R40),
            "R80" => Granularity::Series(&R80),
            "1-2-5" => Granularity::Series(&ONE_TWO_FIVE),
            "E6" => Granularity::Series(&E6),
            "E12" => Granularity::Series(&E12),
            "E24" => Granularity::Series(&E24),
            "E48" => Granularity::Series(&E48),
            "E96" => Granularity::Series(&E96),
            "E192" => Granularity::Series(&E192),
            "POWERSOF2" => Granularity::PowersOf2,
            other => {
                return Err(error(UNKNOWN, format!("Unknown rounding granularity '{other}'")));
            }
        })
    }

    /// Checks that `v` can be rounded: a number that is not negative.
    pub fn check(v: &Value) -> Result<(), MqlError> {
        let negative = match v {
            Value::Int32(n) => *n < 0,
            Value::Int64(n) => *n < 0,
            Value::Double(x) => *x < 0.0,
            Value::Decimal128(d) => {
                matches!(
                    d.classify(),
                    Decimal::Finite { negative: true, coefficient, .. } if coefficient != 0
                ) || matches!(d.classify(), Decimal::Infinity { negative: true })
            }
            other => {
                return Err(error(
                    NOT_NUMBER,
                    format!(
                        "$bucketAuto can specify a 'granularity' with numeric boundaries only, but found a value with type: {}",
                        type_name_of(other)
                    ),
                ));
            }
        };
        if negative {
            return Err(error(
                NEGATIVE,
                "$bucketAuto can specify a 'granularity' with non-negative numbers only, but found a negative number",
            ));
        }
        Ok(())
    }

    /// The least boundary greater than `v` (`up`), or the greatest less than it.
    pub fn round(self, v: &Value, up: bool) -> Result<Value, MqlError> {
        match (self, v) {
            (_, Value::Int32(0) | Value::Int64(0)) => Ok(v.clone()),
            (Granularity::Series(s), Value::Decimal128(d)) => {
                Ok(Value::Decimal128(series_decimal(s, *d, up)?))
            }
            (Granularity::Series(s), other) => {
                let x = crate::expr::values::f64_of(other).unwrap_or(0.0);
                Ok(Value::Double(if x == 0.0 { 0.0 } else { series_double(s, x, up) }))
            }
            (Granularity::PowersOf2, other) => Ok(power_of_two(other, up)),
        }
    }
}

/// A series value scaled to bracket `x` and rounded up or down, in doubles.
fn series_double(s: &[f64], x: f64, up: bool) -> f64 {
    let (first, last) = (s[0], s[s.len() - 1]);
    let mut m = 1.0;
    if up {
        while x < first * m {
            m /= 10.0;
        }
        while x >= last * m {
            m *= 10.0;
        }
        s.iter().map(|v| v * m).find(|v| *v > x).unwrap_or(first * m * 10.0)
    } else {
        while x > last * m {
            m *= 10.0;
        }
        while x <= first * m {
            m /= 10.0;
        }
        s.iter().rev().map(|v| v * m).find(|v| *v < x).unwrap_or(last * m / 10.0)
    }
}

fn dec(op: DecOp, a: Decimal128, b: Decimal128) -> Decimal128 {
    dec_binary(op, &Value::Decimal128(a), &Value::Decimal128(b)).unwrap_or_else(Decimal128::nan)
}

fn less(a: Decimal128, b: Decimal128) -> Result<bool, MqlError> {
    Ok(order(&Some(Value::Decimal128(a)), &Some(Value::Decimal128(b)), None)?
        == std::cmp::Ordering::Less)
}

/// [`series_double`] in decimal.
fn series_decimal(s: &[f64], x: Decimal128, up: bool) -> Result<Decimal128, MqlError> {
    // The multiplier is an exact power of ten, 10^k.
    let power = |k: i32| Decimal128::from_parts(false, 1, k).unwrap_or_else(Decimal128::nan);
    if matches!(x.classify(), Decimal::Finite { coefficient: 0, .. }) {
        return Ok(x);
    }
    let series: Vec<Decimal128> = s
        .iter()
        .map(|v| match to_decimal128(&Value::Double(*v)) {
            Some(d) => d,
            None => Decimal128::nan(),
        })
        .collect();
    let (first, last) = (series[0], series[series.len() - 1]);
    let mut k = 0;
    if up {
        while less(x, dec(DecOp::Mul, first, power(k)))? {
            k -= 1;
        }
        while !less(x, dec(DecOp::Mul, last, power(k)))? {
            k += 1;
        }
        for v in &series {
            let c = dec(DecOp::Mul, *v, power(k));
            if less(x, c)? {
                return Ok(c);
            }
        }
        Ok(dec(DecOp::Mul, first, power(k + 1)))
    } else {
        while less(dec(DecOp::Mul, last, power(k)), x)? {
            k += 1;
        }
        while !less(dec(DecOp::Mul, first, power(k)), x)? {
            k -= 1;
        }
        for v in series.iter().rev() {
            let c = dec(DecOp::Mul, *v, power(k));
            if less(c, x)? {
                return Ok(c);
            }
        }
        Ok(dec(DecOp::Mul, last, power(k - 1)))
    }
}

/// The power of two above (`up`) or below `v`: an integer when `v` is one and the power is at
/// least 1, a double otherwise.
fn power_of_two(v: &Value, up: bool) -> Value {
    let x = crate::expr::values::f64_of(v).unwrap_or(0.0);
    let mut p = 1.0_f64;
    if up {
        while p <= x {
            p *= 2.0;
        }
        while p / 2.0 > x {
            p /= 2.0;
        }
    } else {
        while p >= x {
            p /= 2.0;
        }
        while p * 2.0 < x {
            p *= 2.0;
        }
    }
    let integer = matches!(v, Value::Int32(_) | Value::Int64(_)) && p >= 1.0;
    match (integer, i32::try_from(p as i64)) {
        (true, Ok(n)) => Value::Int32(n),
        (true, Err(_)) => Value::Int64(p as i64),
        _ => Value::Double(p),
    }
}
