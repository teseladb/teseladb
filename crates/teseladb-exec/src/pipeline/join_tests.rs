//! Consecutive `$match`es joined into the first of their run (010 FR-029).

use teseladb_bson::{Document, RawDoc, RawValue};

use super::{Pipeline, Stage};
use crate::expr::Ctx;

fn parsed(stages: &[Document]) -> Pipeline {
    let bytes: Vec<Vec<u8>> = stages.iter().map(|s| s.to_bytes().expect("small")).collect();
    let raw: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document")))
        .collect();
    Pipeline::parse(&raw, &Ctx::checked(std::iter::empty())).expect("parses")
}

/// For each stage: `Some(true)` for a `$match` that filters, `Some(false)` for one that passes
/// every document, `None` for any other stage.
fn matches(p: &Pipeline) -> Vec<Option<bool>> {
    p.stages
        .iter()
        .map(|s| match s {
            Stage::Match(m) => Some(!m.passes_all()),
            _ => None,
        })
        .collect()
}

fn m(field: &str, v: i32) -> Document {
    Document::new().with("$match", Document::new().with(field, v))
}

#[test]
fn a_run_of_matches_filters_in_its_first_and_the_others_pass_everything() {
    let limit = Document::new().with("$limit", 5);
    let p = parsed(&[m("a", 1), m("b", 2), limit.clone(), m("c", 3), m("d", 4), m("e", 5)]);
    assert_eq!(
        matches(&p),
        vec![Some(true), Some(false), None, Some(true), Some(false), Some(false)]
    );
    // A single `$match` stays as it is.
    assert_eq!(matches(&parsed(&[m("a", 1), limit])), vec![Some(true), None]);
}

#[test]
fn the_stage_that_inhibits_optimisation_keeps_its_neighbours_apart() {
    let inhibit = Document::new().with("$_internalInhibitOptimization", Document::new());
    let p = parsed(&[m("a", 1), inhibit, m("b", 2)]);
    assert_eq!(matches(&p), vec![Some(true), Some(true), Some(true)]);
}
