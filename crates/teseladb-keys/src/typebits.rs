//! Type bits (FR-009): what a key does not keep about its values, outside the key and in the order
//! the values are encoded. A number keeps its type (and what its value alone does not say: the
//! sign of a zero double, a NaN's bits, a decimal's exponent); a string, whether it was a symbol.

use teseladb_bson::{Decimal, Decimal128, RawValue};

/// Tags of the type bits.
pub(crate) const INT32: u8 = 0;
pub(crate) const INT64: u8 = 1;
pub(crate) const DOUBLE: u8 = 2;
/// A finite decimal with a canonical coefficient: its exponent and, for zero, its sign follow.
pub(crate) const DECIMAL: u8 = 3;
/// Any other decimal (NaN, infinities, coefficients past 34 digits): its 16 bytes follow.
pub(crate) const DECIMAL_RAW: u8 = 4;
pub(crate) const STRING: u8 = 5;
pub(crate) const SYMBOL: u8 = 6;

/// The type bits of a key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TypeBits {
    bytes: Vec<u8>,
}

impl TypeBits {
    /// From their bytes.
    pub fn from_bytes(bytes: Vec<u8>) -> TypeBits {
        TypeBits { bytes }
    }

    /// Their bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn string(&mut self, symbol: bool) {
        self.bytes.push(if symbol { SYMBOL } else { STRING });
    }

    pub(crate) fn number(&mut self, v: &RawValue<'_>) {
        match *v {
            RawValue::Int32(_) => self.bytes.push(INT32),
            RawValue::Int64(_) => self.bytes.push(INT64),
            RawValue::Double(x) => {
                self.bytes.push(DOUBLE);
                if x == 0.0 {
                    self.bytes.push(u8::from(x.is_sign_negative()));
                } else if x.is_nan() {
                    self.bytes.extend_from_slice(&x.to_bits().to_be_bytes());
                }
            }
            RawValue::Decimal128(d) => match d.classify() {
                Decimal::Finite { negative, coefficient, exponent }
                    if Decimal128::from_parts(negative, coefficient, exponent) == Some(d) =>
                {
                    self.bytes.push(DECIMAL);
                    self.bytes.extend_from_slice(&(exponent as i16).to_be_bytes());
                    if coefficient == 0 {
                        self.bytes.push(u8::from(negative));
                    }
                }
                _ => {
                    self.bytes.push(DECIMAL_RAW);
                    self.bytes.extend_from_slice(&d.to_le_bytes());
                }
            },
            _ => unreachable!("only numbers"),
        }
    }
}
