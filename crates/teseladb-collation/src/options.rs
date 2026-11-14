//! The options a tailoring's rules set (`[strength 2]`, `[caseFirst upper]`, `[reorder Grek]`…),
//! the same ones the reference server lets a collation document set (FR-017). A tailoring's setting
//! replaces the default; a value the document gives replaces both.

use crate::sortkey::{Alternate, Strength};

/// Which case sorts first at the tertiary level: the reference server's `caseFirst`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseFirst {
    /// As the weights say (the reference server's default).
    Off,
    /// Lowercase first.
    Lower,
    /// Uppercase first.
    Upper,
}

/// The last group of characters `shifted` ignores: the reference server's `maxVariable`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaxVariable {
    /// Spaces.
    Space,
    /// Spaces and punctuation (the reference server's default).
    Punct,
    /// Also symbols.
    Symbol,
    /// Also currency symbols.
    Currency,
}

/// What a tailoring sets; `None` leaves the default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// `[strength n]`
    pub strength: Option<Strength>,
    /// `[alternate …]`
    pub alternate: Option<Alternate>,
    /// `[maxVariable …]`
    pub max_variable: Option<MaxVariable>,
    /// `[caseFirst …]`
    pub case_first: Option<CaseFirst>,
    /// `[caseLevel on|off]`
    pub case_level: Option<bool>,
    /// `[normalization on|off]`
    pub normalization: Option<bool>,
    /// `[numericOrdering on|off]`
    pub numeric_ordering: Option<bool>,
    /// `[backwards 2]`: secondary weights compared from the end (the reference server's
    /// `backwards`).
    pub backwards: bool,
    /// `[reorder …]`: script codes and group names, in order.
    pub reorder: Option<Vec<String>>,
}
