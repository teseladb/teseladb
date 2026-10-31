//! Generator of the collation tables (003 FR-018): reads the official Unicode 8.0, CLDR 29 and
//! ICU 57.1 files that `scripts/fetch-pinned.sh` downloads (D-043) and writes
//! `crates/teseladb-collation/data/uca.bin`.
//! `--check` only compares with the file in the repository, so that CI notices data and generator
//! drifting apart.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod builder;
pub mod locales;
pub mod parse;
pub mod resbundle;
pub mod root_elements;
pub mod rules;

use std::fs;
use std::path::{Path, PathBuf};

use teseladb_collation::{Ce, LocaleTables, Root, Tables, TailoringData};

use crate::builder::Builder;
use crate::root_elements::RootElements;

/// The generated root tables, under the repository.
pub const OUTPUT: &str = "crates/teseladb-collation/data/uca.bin";
/// The generated per-locale tables, under the repository.
pub const LOCALES: &str = "crates/teseladb-collation/data/locales.bin";

/// The directory a pinned list downloads into (its `# dest` line) and the text of the list.
fn pinned(root: &Path, list: &str) -> Result<(PathBuf, String), String> {
    let path = root.join("scripts").join(list);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let dest = text
        .lines()
        .find_map(|l| l.strip_prefix("# dest "))
        .ok_or_else(|| format!("{list} has no '# dest'"))?;
    Ok((root.join(dest.trim()), text))
}

fn dest_of(root: &Path, list: &str) -> Result<PathBuf, String> {
    pinned(root, list).map(|(dest, _)| dest)
}

fn read(dir: &Path, name: &str) -> Result<String, String> {
    let path = dir.join(name);
    fs::read_to_string(&path).map_err(|e| {
        format!(
            "{}: {e}; run scripts/fetch-pinned.sh on the scripts/unicode-*.sha256 lists",
            path.display()
        )
    })
}

/// The collation bundles of ICU 57.1 that `scripts/icu-coll.sha256` lists: root and RefDb's
/// locales (D-043).
pub fn icu_collations(root: &Path) -> Result<locales::IcuCollations, String> {
    let (dest, text) = pinned(root, "icu-coll.sha256")?;
    let names: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once("  source/data/coll/"))
        .filter_map(|(_, f)| f.strip_suffix(".txt"))
        .map(str::to_owned)
        .collect();
    locales::IcuCollations::read(&dest.join("source/data/coll"), &names)
}

/// The first primary of the Han group: that of the root's script marker `FDD1 x` whose `x` is an
/// ideograph (ICU's `[last regular]`).
pub fn han_first_primary(t: &Tables) -> Option<u32> {
    let ideograph = |cp: u32| t.unified_ideographs.iter().any(|(lo, hi)| (*lo..=*hi).contains(&cp));
    t.contractions
        .iter()
        .find(|(cps, _, _)| cps.len() == 2 && cps[0] == 0xFDD1 && ideograph(cps[1]))
        .map(|(_, start, _)| t.ces[*start as usize].primary)
}

/// The tables built from the official files under `root`.
pub fn build(root: &Path) -> Result<Tables, String> {
    build_root(root).map(|(tables, _)| tables)
}

/// The tables and the root's elements that are no string's (the `FDD0 …` lines).
pub fn build_root(root: &Path) -> Result<(Tables, Vec<Ce>), String> {
    let cldr = dest_of(root, "unicode-cldr.sha256")?;
    let ucd = dest_of(root, "unicode-ucd.sha256")?;
    let mut t = Tables::default();
    let err = |e: parse::ParseError| e.0;
    parse::unicode_data(&read(&ucd, "UnicodeData.txt")?, &mut t).map_err(err)?;
    parse::prop_list(&read(&ucd, "PropList.txt")?, &mut t).map_err(err)?;
    parse::blocks(&read(&ucd, "Blocks.txt")?, &mut t).map_err(err)?;
    let homeless = parse::fractional_uca(&read(&cldr, "common/uca/FractionalUCA.txt")?, &mut t)
        .map_err(err)?;
    let unassigned = teseladb_collation::implicit_primary(0, &[], &[]);
    let (scripts, aliases) = (read(&ucd, "Scripts.txt")?, read(&ucd, "PropertyValueAliases.txt")?);
    parse::groups(&scripts, &aliases, unassigned, &mut t).map_err(err)?;
    Ok((t, homeless))
}

/// The tailorings of every locale ID RefDb accepts, built over the root under `root` (the root's
/// own `standard` collation is the root itself and has none).
pub fn build_locales(root: &Path) -> Result<LocaleTables, String> {
    let (tables, homeless) = build_root(root)?;
    let elements = RootElements::new(&tables, &homeless);
    let han = han_first_primary(&tables).ok_or("no Han script marker in the root")?;
    let variable_top = tables.variable_top;
    let icu = icu_collations(root)?;
    let base = Root::new(tables);
    let mut built = std::collections::BTreeSet::new();
    let mut tailorings = Vec::new();
    for id in icu.accepted() {
        let r = icu.resolve(&id);
        let key = (r.locale, r.collation);
        if key == ("root".into(), "standard".into()) || !built.insert(key.clone()) {
            continue;
        }
        let (_, text) = icu.rules(&key.0, &key.1).expect("a resolved collation has rules");
        let t = Builder::new(&base, &elements, &icu, variable_top, han)
            .build(text)
            .map_err(|e| format!("{} {}: {e}", key.0, key.1))?;
        let codes = t.settings.reorder.as_deref().unwrap_or_default();
        teseladb_collation::Reordering::new(codes, base.groups())
            .map_err(|e| format!("{} {}: {e}", key.0, key.1))?;
        let data =
            TailoringData { settings: t.settings, mappings: t.mappings, suppressed: t.suppressed };
        tailorings.push((key.0, key.1, data));
    }
    Ok(LocaleTables { bundles: icu.model().bundles().clone(), tailorings })
}

/// Runs the generator with its arguments; returns the exit code and what to print.
pub fn run(args: &[String], root: &Path) -> (i32, String) {
    let check = match args {
        [] => false,
        [a] if a == "--check" => true,
        _ => return (2, "usage: teseladb-unicodegen [--check]".into()),
    };
    let outputs = match build(root)
        .and_then(|t| Ok([(OUTPUT, t.to_bytes()), (LOCALES, build_locales(root)?.to_bytes())]))
    {
        Ok(o) => o,
        Err(e) => return (1, format!("teseladb-unicodegen: {e}")),
    };
    let mut said = Vec::new();
    for (name, bytes) in outputs {
        let out = root.join(name);
        if check {
            match fs::read(&out) {
                Ok(old) if old == bytes => said.push(format!("{name} is up to date")),
                _ => {
                    return (
                        1,
                        format!(
                            "teseladb-unicodegen: {name} is not what the generator writes; run it"
                        ),
                    );
                }
            }
            continue;
        }
        if let Err(e) = fs::write(&out, &bytes) {
            return (1, format!("teseladb-unicodegen: {}: {e}", out.display()));
        }
        said.push(format!("{name}, {} bytes", bytes.len()));
    }
    (0, format!("teseladb-unicodegen: {}", said.join("; ")))
}
