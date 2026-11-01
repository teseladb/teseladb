//! Text analysis (FR-007, FR-019): Unicode tables, word segmentation and normalization, and the
//! character filters, tokenizers and token filters of custom analyzers.

pub mod analyzer;
pub mod charfilters;
pub mod jregex;
pub mod language;
pub mod morfologik;
pub mod morph;
pub mod normalize;
pub mod props;
pub mod smartcn;
pub mod snowball;
pub mod thai;
pub mod tokenfilters;
pub mod tokenizers;
pub mod uax29;
pub mod unicode;
pub mod urlemail;
