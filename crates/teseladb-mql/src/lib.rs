//! TeselaDB's MQL (006): query filters and their matcher, projections, update operators and
//! upserts, schema validation and the regular expressions of `$regex`.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod arith;
mod canonical;
pub mod decimal_math;
pub mod decimal_trig;
mod error;
mod explain;
mod ext;
mod fmt;
mod geo_filter;
mod int64;
pub mod jsonschema;
mod matcher;
pub mod ops;
mod parse;
pub mod projection;
pub mod regex;
pub mod timeseries_write;
pub mod types;
pub mod update;
pub mod validation;
mod value;
pub mod vars;

pub use arith::{DecOp, dec_binary, decimal_to_f64, to_decimal128};
pub use error::{ErrorCode, MqlError, Result, codes};
pub use ext::{Extensions, NoExtensions};
pub use fmt::{double, shell, stream_double};
pub use int64::{decimal_exact, decimal_saturating, exact_int64};
pub use matcher::{Filter, MatchContext};
pub use parse::{BitsOp, CmpOp, Expr, FilterUse, GeoQuery, InItem, Pred, TextQuery};
pub use value::{OwnedDoc, OwnedValue};
