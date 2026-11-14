//! The names a write brings to its collection's dictionary of field names (003 FR-022): before a
//! write goes to its range, the node gives a number to each name of the documents it stores that
//! the dictionary does not have yet, and the write carries the numbers of all its names, which
//! the tables of the versions it stores use.

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};
use teseladb_keys::fields::{FieldNumbers, numberable};
use teseladb_keys::stored::MIN_TABLE_FIELDS;
use teseladb_meta::catalog::CatalogCommand;

use super::Backend;
use super::mutation::{Bound, DocMutation};

/// The numbers of the names of `bound` in its collection's dictionary, after the node asked
/// Group 0 to number those it lacks; `None` when the collection has no dictionary.
///
/// The write goes on when the names do not get their numbers (a node of an earlier version
/// still runs, or the dictionary is full): its tables then compare those names, which every
/// reader finds the same, so the refusal is not an error of the write.
pub(crate) fn numbers_for<B: Backend + ?Sized>(
    backend: &mut B,
    bound: &Bound,
) -> Option<FieldNumbers> {
    let collection = bound.rules().collection;
    if collection == 0 {
        return None;
    }
    let names = names_of(&bound.mutation);
    if names.is_empty() {
        return None;
    }
    let mut dict = backend.field_numbers(collection);
    let lacking: Vec<Vec<u8>> = names
        .iter()
        .filter(|n| numberable(n) && dict.as_ref().is_none_or(|d| d.get(n).is_none()))
        .cloned()
        .collect();
    if !lacking.is_empty() && dict.as_ref().is_none_or(|d| !d.is_full()) {
        let dictionary = u32::try_from(backend.random() >> 32).unwrap_or(1).max(1);
        let cmd = CatalogCommand::AddFieldNames { collection, dictionary, names: lacking };
        if backend.catalog_write(cmd).is_ok() {
            dict = backend.field_numbers(collection);
        }
    }
    dict.map(|d| d.subset(names.iter().map(Vec::as_slice)))
}

/// The field names a mutation stores: those of the documents with a table that it inserts,
/// replaces or upserts, and those of the paths its update operators write.
fn names_of(m: &DocMutation) -> BTreeSet<Vec<u8>> {
    let mut out = BTreeSet::new();
    match m {
        DocMutation::Insert { docs, .. } => docs.iter().for_each(|d| doc_names(d, &mut out)),
        DocMutation::Update { update, upsert, .. } => {
            if let Value::Document(u) = update {
                update_names(u, &mut out);
            }
            if let Some(d) = upsert {
                doc_names(d, &mut out);
            }
        }
        DocMutation::Delete { .. } | DocMutation::Backfill { .. } | DocMutation::Erase { .. } => {}
    }
    out
}

/// The names of `d` and of the documents in it, through documents: those of each document with
/// enough fields for a table (003 FR-021).
fn doc_names(d: &Document, out: &mut BTreeSet<Vec<u8>>) {
    let wide = d.len() >= MIN_TABLE_FIELDS;
    for (k, v) in d.iter() {
        if wide {
            out.insert(k.as_bytes().to_vec());
        }
        if let Value::Document(inner) = v {
            doc_names(inner, out);
        }
    }
}

/// The names of an update: a replacement's as a document's, and for operators the segments of
/// the paths they write (positions and positional operators left out) and the names of the
/// documents they set.
fn update_names(u: &Document, out: &mut BTreeSet<Vec<u8>>) {
    if !u.iter().next().is_some_and(|(k, _)| k.starts_with('$')) {
        doc_names(u, out);
        return;
    }
    for (_, args) in u.iter() {
        let Value::Document(args) = args else { continue };
        for (path, v) in args.iter() {
            let segments = path.split('.');
            let named = segments.filter(|s| !s.starts_with('$') && s.parse::<u64>().is_err());
            out.extend(named.map(|s| s.as_bytes().to_vec()));
            if let Value::Document(inner) = v {
                doc_names(inner, out);
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn wide(prefix: &str) -> Document {
        let mut d = Document::new();
        for i in 0..MIN_TABLE_FIELDS {
            d.push(format!("{prefix}{i}"), i as i32);
        }
        d
    }

    fn names(m: &DocMutation) -> Vec<String> {
        names_of(m).into_iter().map(|n| String::from_utf8(n).unwrap()).collect()
    }

    #[test]
    fn an_insert_brings_the_names_of_its_documents_with_a_table() {
        let mut doc = wide("a");
        doc.push("small", Document::new().with("s", 1));
        doc.push("big", wide("b"));
        doc.push("list", vec![Value::from(wide("c"))]);
        let m = DocMutation::Insert { docs: vec![doc], ordered: true, rules: Default::default() };
        let got = names(&m);
        assert!(got.contains(&"a0".to_owned()) && got.contains(&"big".to_owned()));
        assert!(got.contains(&"b3".to_owned()));
        // A narrow document and an array's elements have no table.
        assert!(!got.iter().any(|n| n == "s" || n.starts_with('c')));
        let narrow = Document::new().with("x", 1);
        let m =
            DocMutation::Insert { docs: vec![narrow], ordered: true, rules: Default::default() };
        assert!(names(&m).is_empty());
    }

    #[test]
    fn an_update_brings_the_segments_of_its_paths_and_its_documents() {
        let ops = Document::new()
            .with("$set", Document::new().with("a.0.b", 1).with("c.$[x].d", wide("w")))
            .with("$inc", Document::new().with("n", 1));
        let m = DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(ops),
            array_filters: None,
            multi: false,
            upsert: Some(wide("u")),
            image: super::super::mutation::Image::None,
            rules: Default::default(),
        };
        let got = names(&m);
        for n in ["a", "b", "c", "d", "n", "w0", "u1"] {
            assert!(got.contains(&n.to_owned()), "{n} in {got:?}");
        }
        assert!(!got.iter().any(|n| n == "0" || n.starts_with('$')));
        let replace = DocMutation::Update {
            filter: Document::new(),
            update: Value::Document(wide("r")),
            array_filters: None,
            multi: false,
            upsert: None,
            image: super::super::mutation::Image::None,
            rules: Default::default(),
        };
        assert_eq!(names(&replace).len(), MIN_TABLE_FIELDS);
        let pipeline = DocMutation::Update {
            filter: Document::new(),
            update: Value::Array(vec![]),
            array_filters: None,
            multi: false,
            upsert: None,
            image: super::super::mutation::Image::None,
            rules: Default::default(),
        };
        assert!(names(&pipeline).is_empty());
    }
}
