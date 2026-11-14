//! The per-locale tailorings committed in `data/locales.bin` (FR-016): read once, and each one's
//! mappings over the root built the first time a collation uses it.

use std::collections::HashMap;
use std::fmt;
use std::sync::OnceLock;

use crate::locale_data::{LocaleTables, TailoringData};
use crate::locales::LocaleModel;
use crate::options::Settings;
use crate::reorder::Reordering;
use crate::root::{Root, root};
use crate::tailored::Tailored;

/// The generated tables, in the binary.
const DATA: &[u8] = include_bytes!("../data/locales.bin");

/// One of the committed tailorings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TailoringId(usize);

/// A `locale` the reference server does not accept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocaleError {
    /// What was asked for.
    pub requested: String,
    /// The locale ID ICU would use instead, if it is not the root (the reference server's «Did you
    /// mean»).
    pub suggestion: Option<String>,
}

impl fmt::Display for LocaleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "locale '{}' is not supported", self.requested)?;
        if let Some(s) = &self.suggestion {
            write!(f, "; did you mean '{s}'?")?;
        }
        Ok(())
    }
}

impl std::error::Error for LocaleError {}

struct Locales {
    model: LocaleModel,
    index: HashMap<(String, String), usize>,
    data: Vec<TailoringData>,
    built: Vec<OnceLock<Tailored<'static, Root>>>,
    reorderings: Vec<OnceLock<Option<Reordering>>>,
}

fn locales() -> &'static Locales {
    static LOCALES: OnceLock<Locales> = OnceLock::new();
    LOCALES.get_or_init(|| {
        let tables = LocaleTables::from_bytes(DATA).expect("the tables the generator wrote");
        let model = tables.model().expect("the generator writes a root");
        let mut index = HashMap::new();
        let mut data = Vec::new();
        for (i, (bundle, ty, t)) in tables.tailorings.into_iter().enumerate() {
            index.insert((bundle, ty), i);
            data.push(t);
        }
        let built = data.iter().map(|_| OnceLock::new()).collect();
        let reorderings = data.iter().map(|_| OnceLock::new()).collect();
        Locales { model, index, data, built, reorderings }
    })
}

/// How locale IDs resolve over the committed bundles.
pub fn locale_model() -> &'static LocaleModel {
    &locales().model
}

/// The tailoring a locale ID the reference server accepts opens (`None` for the root collation
/// itself), or why it is refused.
pub(crate) fn tailoring_for(requested: &str) -> Result<Option<TailoringId>, LocaleError> {
    let l = locales();
    let r = l.model.resolve(requested);
    if !l.model.accepts(requested) {
        let suggestion = (!r.valid.is_empty() && r.valid != "root").then_some(r.valid);
        return Err(LocaleError { requested: requested.to_owned(), suggestion });
    }
    Ok(l.index.get(&(r.locale, r.collation)).map(|i| TailoringId(*i)))
}

/// What a tailoring's rules set.
pub(crate) fn settings(id: TailoringId) -> &'static Settings {
    &locales().data[id.0].settings
}

/// The mappings of a tailoring over the root. The committed mappings are what remained after
/// `[suppressContractions]`, so it applies first.
pub(crate) fn mappings(id: TailoringId) -> &'static Tailored<'static, Root> {
    let l = locales();
    l.built[id.0].get_or_init(|| {
        let t = &l.data[id.0];
        let mut m = Tailored::new(root());
        for (lo, hi) in &t.suppressed {
            m.suppress_contractions(*lo, *hi);
        }
        for (before, cps, ces) in &t.mappings {
            m.insert_context(before, cps, ces.clone());
        }
        m
    })
}

/// The script reordering a tailoring's `[reorder]` asks for, if any.
pub(crate) fn reordering(id: TailoringId) -> Option<&'static Reordering> {
    let l = locales();
    l.reorderings[id.0]
        .get_or_init(|| {
            let codes = l.data[id.0].settings.reorder.as_deref().unwrap_or_default();
            Reordering::new(codes, root().groups()).expect("the generator checks every [reorder]")
        })
        .as_ref()
}
