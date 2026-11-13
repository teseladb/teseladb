//! The registers of a batch as a column, for the accumulators of `$group` (010 FR-031, FR-028):
//! one `LoadField` per document of the batch, each register handed over as a [`Cell`] that holds
//! the machine value of a number, or the BSON of any other value, without building documents.

use teseladb_bson::{RawDoc, RawValue};

use super::run::load;
use super::value::Tag;

/// What a path holds in one document.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cell<'d> {
    /// The path is missing.
    Missing,
    /// A 32-bit integer.
    Int32(i32),
    /// A 64-bit integer.
    Int64(i64),
    /// A double.
    Double(f64),
    /// Any other value.
    Value(RawValue<'d>),
    /// The path crosses an array before its end: what it gives is the expression's to work out.
    ThroughArray,
}

/// The cells of dotted path `path` in each of `docs`, in their order, into `out` (cleared first).
pub fn load_column<'d>(docs: &[RawDoc<'d>], path: &str, out: &mut Vec<Cell<'d>>) {
    let segs: Vec<String> = path.split('.').map(str::to_owned).collect();
    out.clear();
    out.extend(docs.iter().map(|d| {
        let bytes = d.as_bytes();
        let r = load(bytes, &segs);
        match r.tag {
            Tag::Missing => Cell::Missing,
            Tag::ThroughArray => Cell::ThroughArray,
            Tag::I32 => Cell::Int32(r.payload as i32),
            Tag::I64 => Cell::Int64(r.payload as i64),
            Tag::F64 => Cell::Double(f64::from_bits(r.payload)),
            // A value the register cannot hand over as BSON is the expression's to read.
            _ => r.raw(bytes).map_or(Cell::ThroughArray, Cell::Value),
        }
    }));
}
