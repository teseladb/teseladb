//! Reads of several keys of a transaction at one mark sent together (017 SC-002): each key's read
//! goes to its range at once and the answers are awaited after, so the reads take one round
//! instead of one round each. A read that does not come back with an answer (an intent, a range
//! that moved, a leader that changed) goes again on its own with every rule of a single read.

use std::sync::mpsc;
use std::time::Instant;

use teseladb_hlc::Hlc;
use teseladb_io::runtime::CoreInput;
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::{Backend, point_end};
use teseladb_wire::error::CommandError;

use super::ServerBackend;
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;

/// What a reader of a transaction knows: its mark, the transactions it knows about and the end of
/// its uncertainty window.
pub(super) type Mark<'a> = (Hlc, &'a TxnView, Option<Hlc>);

impl ServerBackend {
    /// Sends the read `scan` asks of each key (a point scan with a limit of one, or a point read)
    /// to the range the table names for it, then waits for every answer: the answer of each key,
    /// `None` for one that has to go again on its own.
    fn read_together(
        &mut self,
        keys: &[Vec<u8>],
        (at, view, uncertain_to): Mark<'_>,
        scan: bool,
    ) -> Vec<Option<RemoteReply>> {
        let until = self.until();
        let waiting: Vec<_> = keys
            .iter()
            .map(|key| {
                let entry = self.h.routes.lookup(key)?;
                let req = RemoteReq::ReadWith {
                    key: key.clone(),
                    scan: scan.then(|| (point_end(key), 1)),
                    at: Some(at),
                    concern: ReadConcern::Majority,
                    view: view.clone(),
                    uncertain_to,
                };
                let (tx, rx) = mpsc::channel();
                let msg = NodeMsg::Data(DataReq::Remote(entry.group, req), tx);
                self.cores.get(usize::from(entry.core))?.send(CoreInput::App(msg)).ok()?;
                Some(rx)
            })
            .collect();
        waiting
            .into_iter()
            .map(|rx| match rx?.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Ok(DataReply::Remote(r))) => Some(r),
                _ => None,
            })
            .collect()
    }

    /// [`Backend::txn_gets`]: the reads together, and on its own each one without an answer.
    pub(super) fn gets_together(
        &mut self,
        keys: &[Vec<u8>],
        mark: Mark<'_>,
    ) -> Result<Vec<Option<(u8, Vec<u8>)>>, CommandError> {
        let answers = self.read_together(keys, mark, false);
        keys.iter()
            .zip(answers)
            .map(|(key, answer)| match answer {
                Some(RemoteReply::Value { value, .. }) => Ok(value),
                _ => self.txn_get(key, mark.0, mark.1, mark.2),
            })
            .collect()
    }

    /// [`Backend::txn_points`]: the point scans together, and on its own each one without an
    /// answer.
    pub(super) fn points_together(
        &mut self,
        keys: &[Vec<u8>],
        mark: Mark<'_>,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        let answers = self.read_together(keys, mark, true);
        keys.iter()
            .zip(answers)
            .map(|(key, answer)| match answer {
                Some(RemoteReply::Rows(rows)) => Ok(rows.into_iter().next().map(|(_, v)| v)),
                _ => {
                    let rows = self.txn_scan(key, &point_end(key), 1, mark.0, mark.1, mark.2)?;
                    Ok(rows.into_iter().next().map(|(_, v)| v))
                }
            })
            .collect()
    }
}
