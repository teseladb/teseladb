//! The node's log says the balancer's orders that Group 0 refuses (007 FR-010): the first one at
//! `info`, and that they go on, at `warn`, at the fifth in a row and at one of every thirty
//! after it, with the order's kind, its range and the error. Before, the result of the order was
//! dropped and the same order went out every two seconds: the range a partition cut leaves stayed
//! with two voters for thirty seconds in one run of six of `rest_idempotency`, and no log said
//! why.
#![cfg(target_os = "linux")]

use teseladb_meta::command::{Group0Command, kind};
use teseladb_meta::group0::{Group0Error, Group0Result};
use teseladb_multiraft::{Done, OpError};
use teseladb_server::balancer_refusals::Refusals;
use teseladb_server::coresm::CoreOutput;
use teseladb_server::log::Level;
use teseladb_types::{GroupId, NodeId};

const RANGE: u64 = 2;

fn order() -> Group0Command {
    Group0Command::SetReplicas {
        group: GroupId(RANGE),
        epoch: 2,
        add: vec![NodeId(3)],
        remove: Vec::new(),
    }
}

fn refused() -> Result<Done<CoreOutput>, OpError> {
    Ok(Done::Applied(CoreOutput::Group0(Group0Result::Err(Group0Error::RangeBusy))))
}

fn taken() -> Result<Done<CoreOutput>, OpError> {
    Ok(Done::Applied(CoreOutput::Group0(Group0Result::Ok)))
}

/// The attributes of the line of the `times`-th refusal of [`order`] with `error`.
fn attr(error: &str, times: u32) -> String {
    let kind = kind::SET_REPLICAS;
    format!("{{\"kind\":{kind},\"range\":{RANGE},\"error\":\"{error}\",\"times\":{times}}}")
}

#[test]
fn a_refused_order_is_said_the_first_time_the_fifth_and_one_of_every_thirty() {
    let mut r = Refusals::default();
    let mut told = Vec::new();
    for time in 1..=65u32 {
        r.sent(&order());
        if let Some((level, line)) = r.answered(&refused(), &[]) {
            told.push((time, level, line));
        }
    }
    let times: Vec<u32> = told.iter().map(|(t, ..)| *t).collect();
    assert_eq!(times, [1, 5, 30, 60]);
    let (_, level, line) = &told[0];
    assert_eq!(*level, Level::Info);
    assert_eq!(line.msg, "Group 0 refused the balancer's order");
    assert_eq!(line.attr, attr("RangeBusy", 1));
    assert!(told[1..].iter().all(|(_, level, _)| *level == Level::Warn), "{told:?}");
    assert_eq!(told[1].2.attr, attr("RangeBusy", 5));
}

#[test]
fn an_order_group0_takes_leaves_no_line_and_the_count_starts_again() {
    let mut r = Refusals::default();
    r.sent(&order());
    assert!(r.answered(&refused(), &[]).is_some());
    r.sent(&order());
    assert_eq!(r.answered(&taken(), &[]), None);
    // The next refusal is a first one again.
    r.sent(&order());
    let (level, line) = r.answered(&refused(), &[]).expect("a first refusal");
    assert_eq!((level, line.attr), (Level::Info, attr("RangeBusy", 1)));
}

#[test]
fn an_order_that_did_not_get_to_group0_is_said_too() {
    let mut r = Refusals::default();
    r.sent(&order());
    let (_, line) = r.answered(&Err(OpError::Overloaded), &[]).expect("its line");
    assert_eq!(line.attr, attr("Overloaded", 1));
}

#[test]
fn an_answer_without_an_order_sent_says_nothing() {
    let mut r = Refusals::default();
    assert_eq!(r.answered(&refused(), &[]), None);
}

#[test]
fn a_refusal_of_a_busy_range_says_what_keeps_it_busy() {
    // `RangeBusy` covers a core change, a replica change that has not settled, another epoch
    // and a pending cut: the line carries the causes Group 0's state gives for the order's
    // range and epoch.
    let mut r = Refusals::default();
    assert_eq!(r.replicas_order(), None);
    r.sent(&order());
    assert_eq!(r.replicas_order(), Some((GroupId(RANGE), 2)));
    let busy = ["it moves to core 0".to_owned(), "it waits for its cut".to_owned()];
    let (_, line) = r.answered(&refused(), &busy).expect("a first refusal");
    let causes = "\"busy\":[\"it moves to core 0\",\"it waits for its cut\"]";
    assert_eq!(line.attr, format!("{},{causes}}}", attr("RangeBusy", 1).trim_end_matches('}')));
    assert_eq!(r.replicas_order(), None, "answered");
}
