//! The collation bundles of the reference server's ICU data (root and the locales of its
//! `languages.txt`) read from ICU 57.1's `coll/*.txt`: the resolution of locale IDs
//! (`teseladb_collation::LocaleModel`, ICU's `CollationLoader`) and the rules text of each bundle's
//! collation types.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

pub use teseladb_collation::Resolved;
use teseladb_collation::{BundleInfo, LocaleModel};

use crate::resbundle::{self, Res};

/// The collations of one bundle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bundle {
    /// `collations/default`, if the bundle says it.
    pub default: Option<String>,
    /// Type and rules (`Sequence`) of each tailoring, in file order.
    pub collations: Vec<(String, String)>,
    /// Whether the bundle has a `collations` table at all.
    pub has_collations: bool,
}

/// The bundles: how locale IDs resolve over them, and their rules.
#[derive(Clone, Debug, Default)]
pub struct IcuCollations {
    model: LocaleModel,
    rules: BTreeMap<(String, String), String>,
}

/// A bundle from its resource table.
fn bundle(res: &Res) -> Result<Bundle, String> {
    if res.get("%%ALIAS").is_some() {
        return Err(
            "an alias bundle, which the reference server's ICU data does not include".into()
        );
    }
    let Some(Res::Table(entries)) = res.get("collations") else {
        return Ok(Bundle::default());
    };
    let mut out = Bundle { has_collations: true, ..Bundle::default() };
    for (key, value) in entries {
        if key == "default" {
            out.default = Some(value.as_str().ok_or("a default that is not a string")?.to_owned());
            continue;
        }
        let rules = value
            .get("Sequence")
            .and_then(Res::as_str)
            .ok_or_else(|| format!("collation {key} without its rules"))?;
        out.collations.push((key.clone(), rules.to_owned()));
    }
    Ok(out)
}

impl IcuCollations {
    /// The bundles `names` read from `dir/<name>.txt`.
    pub fn read(dir: &Path, names: &[String]) -> Result<IcuCollations, String> {
        let mut bundles = BTreeMap::new();
        for name in names {
            let path = dir.join(format!("{name}.txt"));
            let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let (key, res) =
                resbundle::parse(&text).map_err(|e| format!("{}: {}", path.display(), e.0))?;
            if &key != name {
                return Err(format!("{}: the bundle is named {key}", path.display()));
            }
            let b = bundle(&res).map_err(|e| format!("{}: {e}", path.display()))?;
            bundles.insert(key, b);
        }
        IcuCollations::new(bundles)
    }

    /// The collations of `bundles`, which must include a root with a `standard` collation.
    pub fn new(bundles: BTreeMap<String, Bundle>) -> Result<IcuCollations, String> {
        let mut rules = BTreeMap::new();
        let mut infos = BTreeMap::new();
        for (name, b) in bundles {
            let types = b.collations.iter().map(|(ty, _)| ty.clone()).collect();
            for (ty, text) in b.collations {
                rules.insert((name.clone(), ty), text);
            }
            let info = BundleInfo { has_collations: b.has_collations, default: b.default, types };
            infos.insert(name, info);
        }
        Ok(IcuCollations { model: LocaleModel::new(infos)?, rules })
    }

    /// How locale IDs resolve over the bundles.
    pub fn model(&self) -> &LocaleModel {
        &self.model
    }

    /// The bundle where the rules of `collation` are found looking from `locale` up, and the rules.
    pub fn rules(&self, locale: &str, collation: &str) -> Option<(&str, &str)> {
        let owner = self.model.owner(locale, collation)?;
        let text = &self.rules[&(owner.to_owned(), collation.to_owned())];
        Some((owner, text.as_str()))
    }

    /// What ICU opens for the locale ID `requested`.
    pub fn resolve(&self, requested: &str) -> Resolved {
        self.model.resolve(requested)
    }

    /// Whether the reference server accepts `requested`.
    pub fn accepts(&self, requested: &str) -> bool {
        self.model.accepts(requested)
    }

    /// Every locale ID the reference server accepts.
    pub fn accepted(&self) -> Vec<String> {
        self.model.accepted()
    }
}
