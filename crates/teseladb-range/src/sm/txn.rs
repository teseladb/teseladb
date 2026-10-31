//! Applying the commands of 008 (contract `txn-protocol.md`): `PutIntents` writes intents at the
//! entry's mark, retracting the transaction's own earlier intent on each key; `EndTxn` and
//! `PushTxn` write the record the leader evaluated; `ResolveIntents` retracts intents and writes
//! their commits. Every version of an entry goes into one memtable, with the range's last mark
//! (and its live bytes for a resolution) at the entry's mark.

use teseladb_lsm::{Kind, VersionWrite};

use super::{ApplyCtx, ApplyError, RangeSm, encode_hlc, last_mark_key, live_key};
use crate::command::{IntentPut, Resolution};
use crate::intents::{Intent, IntentOp, TxnId, TxnRecord, TxnStatus, record_key};

impl RangeSm {
    /// Fails the entry if the range is frozen or a key is outside it.
    fn check_txn_keys<'k>(
        &self,
        keys: impl IntoIterator<Item = &'k [u8]>,
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        if self.frozen != 0 {
            return Err(ApplyError(format!(
                "range {}: entry {} writes while the range is frozen for merge {}",
                self.range.0, ctx.index, self.frozen
            )));
        }
        match keys.into_iter().find(|k| !self.inside(k)) {
            Some(key) => Err(ApplyError(format!(
                "range {}: entry {} writes {key:?} outside the range",
                self.range.0, ctx.index
            ))),
            None => Ok(()),
        }
    }

    /// Writes `versions` and the range's last mark as entry `ctx.index`.
    fn write_txn_entry(
        &mut self,
        versions: Vec<VersionWrite<'_>>,
        live: Option<u64>,
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        let (mark_key, mark) = (last_mark_key(self.range), encode_hlc(ctx.ts));
        let (live_key, live_value) =
            (live_key(self.range), live.unwrap_or(self.live).to_be_bytes());
        let mut versions: Vec<VersionWrite<'_>> = versions.into_iter().collect();
        versions.push(VersionWrite { key: &mark_key, ts: ctx.ts, kind: Kind::Put, value: &mark });
        if live.is_some() {
            versions.push(VersionWrite {
                key: &live_key,
                ts: ctx.ts,
                kind: Kind::Put,
                value: &live_value,
            });
        }
        self.lsm
            .borrow_mut()
            .write_versions(self.range, ctx.index, &versions)
            .map_err(|e| ApplyError(format!("range {}: entry {}: {e}", self.range.0, ctx.index)))?;
        self.last_write = ctx.index;
        self.last_mark = ctx.ts;
        if let Some(live) = live {
            self.live = live;
        }
        Ok(())
    }

    pub(super) fn apply_put_intents(
        &mut self,
        txn: TxnId,
        record_key: &[u8],
        intents: &[IntentPut],
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        self.check_txn_keys(intents.iter().map(|i| i.key.as_slice()), ctx)?;
        let payloads: Vec<Vec<u8>> = intents
            .iter()
            .map(|i| Intent { txn, record_key: record_key.to_vec(), op: i.op.clone() }.encode())
            .collect();
        let mut versions = Vec::with_capacity(intents.len() * 2 + 1);
        for (i, payload) in intents.iter().zip(&payloads) {
            if let Some(old) = i.replaces {
                versions.push(VersionWrite {
                    key: &i.key,
                    ts: old,
                    kind: Kind::Retract,
                    value: &[],
                });
            }
            versions.push(VersionWrite {
                key: &i.key,
                ts: ctx.ts,
                kind: Kind::Intent,
                value: payload,
            });
        }
        self.write_txn_entry(versions, None, ctx)
    }

    pub(super) fn apply_record(
        &mut self,
        record: &TxnRecord,
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        let key = record_key(record.txn);
        self.check_txn_keys([key.as_slice()], ctx)?;
        let value = record.encode();
        let versions = vec![VersionWrite { key: &key, ts: ctx.ts, kind: Kind::Put, value: &value }];
        self.write_txn_entry(versions, None, ctx)
    }

    pub(super) fn apply_resolve(
        &mut self,
        (status, commit_ts): (TxnStatus, teseladb_hlc::Hlc),
        resolutions: &[Resolution],
        live_delta: i64,
        ctx: &ApplyCtx,
    ) -> Result<(), ApplyError> {
        self.check_txn_keys(resolutions.iter().map(|r| r.key.as_slice()), ctx)?;
        let committed = status == TxnStatus::Committed;
        if status == TxnStatus::Pending
            || resolutions.iter().any(|r| r.commit.is_some() != committed)
        {
            return Err(ApplyError(format!(
                "range {}: entry {} resolves intents of a transaction in state {status:?} with \
                 commits that do not match it",
                self.range.0, ctx.index
            )));
        }
        let stored: Vec<Option<(Kind, Vec<u8>)>> = resolutions
            .iter()
            .map(|r| {
                r.commit.as_ref().map(|op| match op {
                    IntentOp::Put { value_type, value } => {
                        (Kind::Put, [&[*value_type][..], value].concat())
                    }
                    IntentOp::Delete => (Kind::Delete, Vec::new()),
                })
            })
            .collect();
        let mut versions = Vec::with_capacity(resolutions.len() * 2 + 2);
        for (r, commit) in resolutions.iter().zip(&stored) {
            // A commit at the intent's own mark replaces it; otherwise the intent is retracted.
            if commit.is_none() || r.intent_ts != commit_ts {
                versions.push(VersionWrite {
                    key: &r.key,
                    ts: r.intent_ts,
                    kind: Kind::Retract,
                    value: &[],
                });
            }
            if let Some((kind, value)) = commit {
                versions.push(VersionWrite { key: &r.key, ts: commit_ts, kind: *kind, value });
            }
        }
        // The counter never goes below zero: a delta the leader got wrong cannot wrap it.
        let live = self.live.saturating_add_signed(live_delta);
        self.write_txn_entry(versions, Some(live), ctx)
    }
}
