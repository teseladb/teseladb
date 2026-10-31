//! TeselaDB's collation (003 FR-016 to FR-018): the Unicode Collation Algorithm (UTS #10) with the
//! CLDR root collation of ICU 57.1, the version RefDb 9.0 bundles (D-043): NFD, collation elements
//! with contractions, contexts and implicit weights, the tailorings of the locales RefDb accepts,
//! and sort keys whose bytes compare as the strings do.
//!
//! ```
//! use teseladb_collation::{Collation, Strength};
//!
//! let c = Collation { strength: Strength::Primary, ..Collation::default() };
//! assert_eq!(c.compare("canción", "CANCION"), std::cmp::Ordering::Equal);
//! let t = Collation::default();
//! assert!(t.sort_key("a") < t.sort_key("A") && t.sort_key("A") < t.sort_key("b"));
//! // Swedish sorts ö after z; the root, with o.
//! let sv = Collation::for_locale("sv").unwrap();
//! assert!(sv.sort_key("z") < sv.sort_key("ö") && t.sort_key("ö") < t.sort_key("z"));
//! assert!(Collation::for_locale("sv_SE").is_err());
//! ```
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod data;
mod elements;
mod fxhash;
mod locale_data;
mod locales;
mod nfd;
mod options;
mod reorder;
mod root;
mod sortkey;
mod spec;
mod tailored;
mod tailoring;

use std::cmp::Ordering;

pub use data::{COMMON_WEIGHT16, Ce, DataError, MERGE_SEPARATOR_PRIMARY, Tables, implicit_primary};
pub use elements::{Mappings, elements, elements_numeric};
pub use locale_data::{LocaleTables, TailoringData};
pub use locales::{BundleInfo, LocaleModel, Resolved};
pub use options::{CaseFirst, MaxVariable, Settings};
pub use reorder::Reordering;
pub use root::{Root, root};
pub use sortkey::{Alternate, Strength};
pub use spec::{CollationSpec, ICU_VERSION, ParsedCollation, SpecError, parse_collation};
pub use tailored::Tailored;
pub use tailoring::{LocaleError, TailoringId, locale_model};

/// A collation: RefDb's options and the locale's tailoring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collation {
    /// Levels compared (`strength`).
    pub strength: Strength,
    /// How spaces and punctuation weigh (`alternate`).
    pub alternate: Alternate,
    /// Which case sorts first (`caseFirst`).
    pub case_first: CaseFirst,
    /// A level for case between accents and the tertiary differences (`caseLevel`).
    pub case_level: bool,
    /// Runs of digits compare as numbers (`numericOrdering`).
    pub numeric_ordering: bool,
    /// The last group `shifted` ignores (`maxVariable`).
    pub max_variable: MaxVariable,
    /// Canonical reordering of combining marks before comparing (`normalization`).
    pub normalization: bool,
    /// Accents compared from the end (`backwards`).
    pub backwards: bool,
    /// The locale's tailoring; `None` is the root collation.
    pub tailoring: Option<TailoringId>,
}

impl Default for Collation {
    /// RefDb's defaults for the root collation.
    fn default() -> Collation {
        Collation {
            strength: Strength::Tertiary,
            alternate: Alternate::NonIgnorable,
            case_first: CaseFirst::Off,
            case_level: false,
            numeric_ordering: false,
            max_variable: MaxVariable::Punct,
            normalization: false,
            backwards: false,
            tailoring: None,
        }
    }
}

impl Collation {
    /// The collation of a `locale` RefDb accepts, with the defaults its tailoring sets.
    pub fn for_locale(locale: &str) -> Result<Collation, LocaleError> {
        let tailoring = tailoring::tailoring_for(locale)?;
        let mut c = Collation { tailoring, ..Collation::default() };
        if let Some(id) = tailoring {
            let s = tailoring::settings(id);
            c.strength = s.strength.unwrap_or(c.strength);
            c.alternate = s.alternate.unwrap_or(c.alternate);
            c.case_first = s.case_first.unwrap_or(c.case_first);
            c.case_level = s.case_level.unwrap_or(c.case_level);
            c.numeric_ordering = s.numeric_ordering.unwrap_or(c.numeric_ordering);
            c.max_variable = s.max_variable.unwrap_or(c.max_variable);
            c.normalization = s.normalization.unwrap_or(c.normalization);
            c.backwards = s.backwards;
        }
        Ok(c)
    }

    /// What the locale's tailoring sets (nothing for the root).
    pub fn settings(&self) -> Settings {
        self.tailoring.map(|id| tailoring::settings(id).clone()).unwrap_or_default()
    }

    /// The sort key of a string.
    pub fn sort_key(&self, s: &str) -> Vec<u8> {
        self.sort_key_code_points(&s.chars().map(u32::from).collect::<Vec<_>>())
    }

    /// The sort key of any code points, lone surrogates included.
    pub fn sort_key_code_points(&self, cps: &[u32]) -> Vec<u8> {
        let root = root::root();
        match self.tailoring {
            Some(id) => {
                self.sort_key_in(tailoring::mappings(id), root, tailoring::reordering(id), cps)
            }
            None => self.sort_key_in(root, root, None, cps),
        }
    }

    /// The sort key of code points with the elements of `mappings` (the root or a tailoring over
    /// `root`) and a script reordering.
    pub fn sort_key_in<M: Mappings<Element = Ce>>(
        &self,
        mappings: &M,
        root: &Root,
        reorder: Option<&Reordering>,
        cps: &[u32],
    ) -> Vec<u8> {
        let text = if self.normalization {
            root.nfd(cps.iter().copied())
        } else {
            root.decompose(cps.iter().copied())
        };
        let ces = elements::elements_numeric(mappings, &text, self.numeric_ordering);
        let nfd = if self.strength == Strength::Identical && !self.normalization {
            root.nfd(cps.iter().copied())
        } else {
            text
        };
        let options = sortkey::KeyOptions {
            strength: self.strength,
            alternate: self.alternate,
            variable_top: root.variable_top_for(self.max_variable),
            case_first: self.case_first,
            case_level: self.case_level,
            backwards: self.backwards,
            reorder,
        };
        sortkey::sort_key(&ces, &nfd, &options)
    }

    /// Compares two strings.
    pub fn compare(&self, a: &str, b: &str) -> Ordering {
        self.sort_key(a).cmp(&self.sort_key(b))
    }
}

/// Strings in index keys and comparisons take this collation's order (FR-017).
impl teseladb_bson::Collator for Collation {
    fn sort_key(&self, s: &str) -> Vec<u8> {
        Collation::sort_key(self, s)
    }
}

/// The UCA version of the tables.
pub fn uca_version() -> &'static str {
    &root::root().version
}
