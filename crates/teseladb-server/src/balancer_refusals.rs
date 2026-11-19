//! The balancer's orders that Group 0 refuses, for the node's log (007 FR-010).
//!
//! Group 0's leader sends one order a round, and the same one again while its table asks for
//! it. The result of the order was dropped: a range that stayed short of a replica, with every
//! order to add it refused, had nothing saying why. The node says the first refusal, and that
//! they go on at the fifth in a row and at one of every thirty after it. An order that went to
//! the range that holds the descriptor (after the handoff, FR-014) comes back without its
//! result and counts as taken.

use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::Group0Result;
use teseladb_multiraft::{Done, OpError};
use teseladb_types::GroupId;

use crate::coresm::CoreOutput;
use crate::log::{Level, Pending};

/// The message of the log line.
const MESSAGE: &str = "Group 0 refused the balancer's order";
/// The refusal in a row at which the node says that they go on.
const GOES_ON_AT: u32 = 5;
/// After it, the node says one of every so many.
const TOLD_EVERY: u32 = 30;

/// The balancer's order in flight and the refusals in a row before it.
#[derive(Debug, Default)]
pub struct Refusals {
    /// The kind of the order sent and the range it changes, if it changes one.
    order: Option<(u16, Option<GroupId>)>,
    /// The epoch a change of replicas sent names.
    epoch: Option<u64>,
    /// The orders refused in a row.
    times: u32,
}

impl Refusals {
    /// The balancer sends `cmd`.
    pub fn sent(&mut self, cmd: &Group0Command) {
        self.order = Some((cmd.kind(), range_of(cmd)));
        self.epoch = match cmd {
            Group0Command::SetReplicas { epoch, .. } => Some(*epoch),
            _ => None,
        };
    }

    /// The range and the epoch of the change of replicas in flight, if the order is one: what
    /// Group 0's state is asked about when the change comes back refused.
    pub fn replicas_order(&self) -> Option<(GroupId, u64)> {
        let (_, range) = self.order?;
        range.zip(self.epoch)
    }

    /// The order sent ended with `result`: the line for the node's log when Group 0 refused it,
    /// or it did not get there, for the first time, the fifth in a row or one of every thirty
    /// after it. `busy` is what keeps the order's range busy, as Group 0's state tells it
    /// ([`teseladb_meta::group0::Group0Sm::busy_causes`]).
    pub fn answered(
        &mut self,
        result: &Result<Done<CoreOutput>, OpError>,
        busy: &[String],
    ) -> Option<(Level, Pending)> {
        let order = self.order.take();
        self.epoch = None;
        let error = match result {
            Ok(Done::Applied(CoreOutput::Group0(Group0Result::Err(e)))) => format!("{e:?}"),
            Err(e) => format!("{e:?}"),
            Ok(_) => {
                self.times = 0;
                return None;
            }
        };
        let (kind, range) = order?;
        self.times += 1;
        let times = self.times;
        let level = match times {
            1 => Level::Info,
            t if t == GOES_ON_AT || t.is_multiple_of(TOLD_EVERY) => Level::Warn,
            _ => return None,
        };
        let range = range.map_or_else(|| "null".to_owned(), |g| g.0.to_string());
        let causes: Vec<String> = busy.iter().map(|c| format!("\"{c}\"")).collect();
        let busy = match causes.is_empty() {
            true => String::new(),
            false => format!(",\"busy\":[{}]", causes.join(",")),
        };
        let attr = format!(
            "{{\"kind\":{kind},\"range\":{range},\"error\":\"{error}\",\"times\":{times}{busy}}}"
        );
        Some((level, Pending { id: 0, msg: MESSAGE.to_owned(), attr }))
    }
}

/// The range `cmd` changes, if it changes one.
fn range_of(cmd: &Group0Command) -> Option<GroupId> {
    match cmd {
        Group0Command::SetReplicas { group, .. } | Group0Command::SetLeaderHint { group, .. } => {
            Some(*group)
        }
        _ => None,
    }
}
