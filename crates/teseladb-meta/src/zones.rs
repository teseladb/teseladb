//! Zones (018 FR-003): `addShardToZone`, `removeShardFromZone` and `updateZoneKeyRange` keep
//! them as Group 0 settings, so that one `SetSettings` entry changes them together with the
//! checks of what it read:
//!
//! - `zone.shard.<node>.<zone>` (node as a big-endian `u64`): the node is in the zone; its value
//!   orders the node's zones as they were added (`config.shards`' `tags`);
//! - `zone.range.<ns>\0<id>` ([`ZoneRange`]): a shard key interval of a collection and its zone,
//!   in the order they were added (the `ObjectId` of `config.tags`).
//!
//! The balancer reads them ([`Zones::placement`]): a range whose keys are inside a zone's
//! interval keeps its replicas on the zone's nodes, one that crosses the edge of an interval is
//! cut there first, and the others go anywhere.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_types::{DecodeError, Decoder, Encoder, NodeId};

/// Prefix of a node's membership of a zone.
pub const SHARD_PREFIX: &[u8] = b"zone.shard.";
/// Prefix of a zone's interval.
pub const RANGE_PREFIX: &[u8] = b"zone.range.";

/// A zone's interval of a collection, as `config.tags` shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ZoneRange {
    /// The namespace, `db.coll`.
    pub ns: String,
    /// `_id` of its `config.tags` document (an `ObjectId`).
    pub id: [u8; 12],
    /// `min` and `max` as BSON documents.
    pub min: Vec<u8>,
    /// `max`.
    pub max: Vec<u8>,
    /// The zone.
    pub zone: String,
    /// The keys of the interval, `[start, end)`, when the collection has a shard key.
    pub keys: Option<(Vec<u8>, Vec<u8>)>,
}

impl ZoneRange {
    /// Its setting's key.
    pub fn key(&self) -> Vec<u8> {
        range_key(&self.ns, self.id)
    }

    /// Its setting's value.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.str(&self.ns).bytes(&self.id).bytes(&self.min).bytes(&self.max).str(&self.zone);
        e.bool(self.keys.is_some());
        if let Some((s, end)) = &self.keys {
            e.bytes(s).bytes(end);
        }
        e.finish()
    }

    /// The interval of a setting's value.
    pub fn decode(v: &[u8]) -> Result<ZoneRange, DecodeError> {
        let mut d = Decoder::new(v);
        let ns = d.str()?.to_owned();
        let at = d.position();
        let id: [u8; 12] =
            d.bytes()?.try_into().map_err(|_| DecodeError::InvalidValue { offset: at })?;
        let (min, max, zone) = (d.bytes()?.to_vec(), d.bytes()?.to_vec(), d.str()?.to_owned());
        let keys = if d.bool()? { Some((d.bytes()?.to_vec(), d.bytes()?.to_vec())) } else { None };
        d.finish()?;
        Ok(ZoneRange { ns, id, min, max, zone, keys })
    }
}

/// The key of node `node`'s membership of `zone`.
pub fn shard_key(node: NodeId, zone: &str) -> Vec<u8> {
    [SHARD_PREFIX, &node.0.to_be_bytes(), b".", zone.as_bytes()].concat()
}

/// The key of the interval `id` of `ns`.
pub fn range_key(ns: &str, id: [u8; 12]) -> Vec<u8> {
    [RANGE_PREFIX, ns.as_bytes(), &[0], &id].concat()
}

/// Where the balancer may put a range's replicas.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Placement {
    /// On any node.
    Anywhere,
    /// On these nodes: the range is inside an interval of their zone.
    On(BTreeSet<NodeId>),
    /// The range crosses the edge of an interval at this key: it is cut there first.
    CutAt(Vec<u8>),
}

/// The zones of the cluster.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Zones {
    /// The zones of each node, in the order they were added.
    pub shards: BTreeMap<NodeId, Vec<String>>,
    /// The intervals, in the order they were added per namespace.
    pub ranges: Vec<ZoneRange>,
}

impl Zones {
    /// The zones in Group 0's settings; a value that does not decode is skipped.
    pub fn from_settings(settings: &BTreeMap<Vec<u8>, Vec<u8>>) -> Zones {
        let mut z = Zones::default();
        let mut added: BTreeMap<NodeId, Vec<(&[u8], String)>> = BTreeMap::new();
        let shards = settings.range(SHARD_PREFIX.to_vec()..);
        for (k, v) in shards.take_while(|(k, _)| k.starts_with(SHARD_PREFIX)) {
            let rest = &k[SHARD_PREFIX.len()..];
            let (Some(node), Some(b'.')) = (rest.get(..8), rest.get(8)) else { continue };
            let node = NodeId(u64::from_be_bytes(node.try_into().unwrap_or_default()));
            let zone = String::from_utf8_lossy(&rest[9..]).into_owned();
            added.entry(node).or_default().push((v.as_slice(), zone));
        }
        for (node, mut zones) in added {
            zones.sort();
            z.shards.insert(node, zones.into_iter().map(|(_, zone)| zone).collect());
        }
        let ranges = settings.range(RANGE_PREFIX.to_vec()..);
        z.ranges = ranges
            .take_while(|(k, _)| k.starts_with(RANGE_PREFIX))
            .filter_map(|(_, v)| ZoneRange::decode(v).ok())
            .collect();
        z
    }

    /// Whether some node is in `zone`.
    pub fn exists(&self, zone: &str) -> bool {
        self.shards.values().any(|z| z.iter().any(|x| x == zone))
    }

    /// The nodes of `zone`.
    pub fn nodes_of(&self, zone: &str) -> BTreeSet<NodeId> {
        self.shards.iter().filter(|(_, z)| z.iter().any(|x| x == zone)).map(|(n, _)| *n).collect()
    }

    /// Where the replicas of the range `[start, end)` may go (an empty `end`: no end).
    pub fn placement(&self, start: &[u8], end: &[u8]) -> Placement {
        let below_end = |k: &[u8]| end.is_empty() || k < end;
        for (zs, ze) in self.ranges.iter().filter_map(|r| r.keys.as_ref()) {
            let overlaps = below_end(zs) && start < ze.as_slice();
            if !overlaps {
                continue;
            }
            if zs.as_slice() > start {
                return Placement::CutAt(zs.clone());
            }
            if below_end(ze) {
                return Placement::CutAt(ze.clone());
            }
        }
        let inside = self.ranges.iter().find(|r| {
            r.keys.as_ref().is_some_and(|(zs, ze)| {
                zs.as_slice() <= start && !end.is_empty() && end <= ze.as_slice()
            })
        });
        match inside {
            Some(r) => Placement::On(self.nodes_of(&r.zone)),
            None => Placement::Anywhere,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn range(zone: &str, s: &[u8], e: &[u8], id: u8) -> ZoneRange {
        ZoneRange {
            ns: "db.c".into(),
            id: [id; 12],
            min: vec![5, 0, 0, 0, 0],
            max: vec![5, 0, 0, 0, 0],
            zone: zone.into(),
            keys: Some((s.to_vec(), e.to_vec())),
        }
    }

    fn settings(z: &[(NodeId, &str)], r: &[ZoneRange]) -> BTreeMap<Vec<u8>, Vec<u8>> {
        let mut s: BTreeMap<Vec<u8>, Vec<u8>> = z
            .iter()
            .enumerate()
            .map(|(i, (n, zone))| (shard_key(*n, zone), vec![u8::try_from(i).unwrap_or(0)]))
            .collect();
        s.extend(r.iter().map(|r| (r.key(), r.encode())));
        s.insert(b"zone.shard.short".to_vec(), Vec::new());
        s.insert(b"zone.range.bad".to_vec(), vec![1]);
        s.insert(b"zzz".to_vec(), Vec::new());
        s
    }

    #[test]
    fn placement_follows_the_intervals() {
        let eu = range("EU", b"\x10", b"\x20", 1);
        let unsharded = ZoneRange { keys: None, ..range("EU", b"", b"", 2) };
        let z = Zones::from_settings(&settings(
            &[(NodeId(1), "EU"), (NodeId(2), "EU"), (NodeId(3), "US")],
            &[eu.clone(), unsharded.clone()],
        ));
        assert_eq!(z.ranges, [eu.clone(), unsharded]);
        assert!(z.exists("US") && !z.exists("ASIA"));
        let both = Zones::from_settings(&settings(&[(NodeId(1), "b"), (NodeId(1), "a")], &[]));
        assert_eq!(both.shards[&NodeId(1)], ["b", "a"], "in the order they were added");
        let eu_nodes: BTreeSet<NodeId> = [NodeId(1), NodeId(2)].into();
        assert_eq!(z.placement(b"\x10", b"\x20"), Placement::On(eu_nodes.clone()));
        assert_eq!(z.placement(b"\x12", b"\x18"), Placement::On(eu_nodes));
        assert_eq!(z.placement(b"\x01", b"\x18"), Placement::CutAt(b"\x10".to_vec()));
        assert_eq!(z.placement(b"\x12", b""), Placement::CutAt(b"\x20".to_vec()));
        assert_eq!(z.placement(b"\x12", b"\x30"), Placement::CutAt(b"\x20".to_vec()));
        assert_eq!(z.placement(b"\x20", b"\x30"), Placement::Anywhere);
        assert_eq!(z.placement(b"\x01", b"\x10"), Placement::Anywhere);
        assert_eq!(ZoneRange::decode(&eu.encode()), Ok(eu));
        assert!(ZoneRange::decode(&[0]).is_err());
    }
}
