//! Text analysis (FR-007, FR-019): Unicode tables, word segmentation and normalization, and the
//! character filters, tokenizers and token filters of custom analyzers.

/// The version of the analyzers of this build. Analysis is deterministic: the same text gives the
/// same tokens on every replica of one version (FR-011, D-019); a change to any analyzer's output
/// raises it, and the segments of another version are rebuilt.
pub const ANALYZERS_VERSION: u32 = 1;

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
