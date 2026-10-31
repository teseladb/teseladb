//! Locale IDs as ICU 57.1's `CollationLoader` (`ucol_res.cpp`) resolves them over the collation
//! bundles RefDb packages (root and the locales of its `languages.txt`), and which of them
//! RefDb accepts: those whose valid locale is the requested string (`validateLocaleID`).
//!
//! The bundle is the first that exists cutting the requested base name at its last `_` (down to
//! the root); it is the valid locale. The `collations` table, its `default` type and the requested
//! type are looked up in that bundle and then its parents; a missing type falls back to `search`
//! (for `search…` types), then to the default type, then to `standard`. The valid locale carries
//! `@collation=type` when the type found is not the default. `root` itself (and the empty name)
//! is the root collator, whose valid locale is empty.

use std::collections::BTreeMap;

/// What a bundle says about collation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BundleInfo {
    /// Whether it has a `collations` table at all.
    pub has_collations: bool,
    /// `collations/default`, if it says it.
    pub default: Option<String>,
    /// The collation types it defines.
    pub types: Vec<String>,
}

/// What a requested locale opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// ICU's valid locale: RefDb accepts the request only if it is this.
    pub valid: String,
    /// The bundle whose rules apply.
    pub locale: String,
    /// The collation type whose rules apply.
    pub collation: String,
}

/// The bundles by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocaleModel {
    bundles: BTreeMap<String, BundleInfo>,
}

/// The parent bundle of `name`: cut at the last `_`, the root after a bare language.
fn parent(name: &str) -> Option<&str> {
    match name.rfind('_') {
        _ if name == "root" => None,
        Some(i) => Some(&name[..i]),
        None => Some("root"),
    }
}

impl LocaleModel {
    /// The model of `bundles`, which must include a root with a `standard` collation.
    pub fn new(bundles: BTreeMap<String, BundleInfo>) -> Result<LocaleModel, String> {
        let root = bundles.get("root");
        if !root.is_some_and(|b| b.types.iter().any(|t| t == "standard")) {
            return Err("no root bundle with a standard collation".into());
        }
        Ok(LocaleModel { bundles })
    }

    /// The bundles, by name.
    pub fn bundles(&self) -> &BTreeMap<String, BundleInfo> {
        &self.bundles
    }

    /// `name` and its parents that exist, nearest first.
    fn chain(&self, name: &str) -> Vec<(&str, &BundleInfo)> {
        std::iter::successors(Some(name), |n| parent(n))
            .filter_map(|n| self.bundles.get_key_value(n).map(|(k, b)| (k.as_str(), b)))
            .collect()
    }

    /// The bundle that defines `collation` looking from `locale` up.
    pub fn owner(&self, locale: &str, collation: &str) -> Option<&str> {
        let defines = |b: &BundleInfo| b.types.iter().any(|t| t == collation);
        self.chain(locale).into_iter().find(|(_, b)| defines(b)).map(|(n, _)| n)
    }

    /// What ICU opens for the locale ID `requested`.
    pub fn resolve(&self, requested: &str) -> Resolved {
        if requested.is_empty() || requested == "root" {
            let s = |s: &str| s.to_owned();
            return Resolved { valid: String::new(), locale: s("root"), collation: s("standard") };
        }
        let (base, keywords) = requested.split_once('@').unwrap_or((requested, ""));
        let asked = keywords
            .split(';')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("collation"))
            .map(|(_, v)| v.trim().to_ascii_lowercase())
            .filter(|v| !v.is_empty() && v != "default");
        let (bundle, _) = *self.chain(base).first().expect("the chain ends at the root");
        // The `collations` table: in the bundle or the nearest parent that has one.
        let (owner, _) = self
            .chain(bundle)
            .into_iter()
            .find(|(_, b)| b.has_collations)
            .expect("the root has collations");
        let default = self
            .chain(owner)
            .into_iter()
            .find_map(|(_, b)| b.default.clone())
            .unwrap_or_else(|| "standard".to_owned());
        let first = asked.unwrap_or_else(|| default.clone());
        let mut candidates = vec![first.clone()];
        if first.len() > "search".len() && first.starts_with("search") {
            candidates.push("search".to_owned());
        }
        candidates.push(default.clone());
        candidates.push("standard".to_owned());
        let (found, collation) = candidates
            .into_iter()
            .find_map(|t| self.owner(owner, &t).map(|found| (found.to_owned(), t)))
            .expect("the root has a standard collation");
        let valid = if collation == default {
            bundle.to_owned()
        } else {
            format!("{bundle}@collation={collation}")
        };
        Resolved { valid, locale: found, collation }
    }

    /// Whether RefDb accepts `requested` (`validateLocaleID`): not empty, and ICU's valid
    /// locale for it is the same string.
    pub fn accepts(&self, requested: &str) -> bool {
        !requested.is_empty() && self.resolve(requested).valid == requested
    }

    /// Every locale ID RefDb accepts: each bundle but the root, alone and with each collation
    /// type it reaches other than its default.
    pub fn accepted(&self) -> Vec<String> {
        let mut out = Vec::new();
        for name in self.bundles.keys().filter(|n| *n != "root") {
            let mut types: Vec<&str> = self
                .chain(name)
                .into_iter()
                .flat_map(|(_, b)| b.types.iter().map(String::as_str))
                .collect();
            types.sort_unstable();
            types.dedup();
            let ids = std::iter::once(name.clone())
                .chain(types.iter().map(|t| format!("{name}@collation={t}")));
            out.extend(ids.filter(|id| self.accepts(id)));
        }
        out
    }
}
