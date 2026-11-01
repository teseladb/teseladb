//! Tenants (007 FR-025, D-014): Group 0 keeps their names and numbers. Tenant 0 is the default
//! one, which every cluster has without creating it. A tenant's databases live in the same
//! catalog as everyone's, with their names qualified by the tenant ([`qualified_db`]): RefDb's
//! database names never hold a zero byte, so the default tenant's names stay as they are and no
//! name of one tenant meets another's. The collection numbers are shared by every tenant, and a
//! collection's keys carry its tenant.

use std::collections::BTreeMap;

use teseladb_bson::{RawDoc, RawValue};
use teseladb_types::{DecodeError, Decoder, Encoder};

/// The default tenant: a deployment without tenants runs in it.
pub const DEFAULT_TENANT: u32 = 0;
/// The field of a user document with the user's tenant, when it is not the default one.
pub const TENANT_FIELD: &str = "tenantId";
/// The longest name of a tenant, in bytes.
pub const MAX_NAME: usize = 64;
/// What opens and closes the tenant of a qualified database name.
const MARK: char = '\0';

/// The name database `db` of tenant `tenant` has in the catalog.
pub fn qualified_db(tenant: u32, db: &str) -> String {
    match tenant {
        DEFAULT_TENANT => db.to_owned(),
        t => format!("{MARK}{t}{MARK}{db}"),
    }
}

/// The tenant and the name of qualified database name `db`.
pub fn split_db(db: &str) -> (u32, &str) {
    let parsed = db.strip_prefix(MARK).and_then(|rest| {
        let (number, name) = rest.split_once(MARK)?;
        Some((number.parse::<u32>().ok()?, name))
    });
    match parsed {
        Some((t, name)) if t != DEFAULT_TENANT => (t, name),
        _ => (DEFAULT_TENANT, db),
    }
}

/// The tenant of the user whose stored document is `doc`: its [`TENANT_FIELD`], or the default
/// one without it.
pub fn of_user(doc: &[u8]) -> u32 {
    match RawDoc::from_bytes(doc).ok().and_then(|d| d.get(TENANT_FIELD)) {
        Some(RawValue::Int32(t)) => u32::try_from(t).unwrap_or(DEFAULT_TENANT),
        Some(RawValue::Int64(t)) => u32::try_from(t).unwrap_or(DEFAULT_TENANT),
        _ => DEFAULT_TENANT,
    }
}

/// Whether `name` may name a tenant: not empty, at most [`MAX_NAME`] bytes, no zero byte.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_NAME && !name.contains(MARK)
}

/// The tenants Group 0 knows, by number; the default one is not among them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tenants {
    names: BTreeMap<u32, String>,
    last: u32,
}

impl Tenants {
    /// The tenants, by number.
    pub fn list(&self) -> Vec<(u32, &str)> {
        self.names.iter().map(|(id, n)| (*id, n.as_str())).collect()
    }

    /// The number of tenant `name`.
    pub fn id_of(&self, name: &str) -> Option<u32> {
        self.names.iter().find(|(_, n)| n.as_str() == name).map(|(id, _)| *id)
    }

    /// Adds tenant `name` with the next number, never given before.
    pub fn add(&mut self, name: &str) -> Option<u32> {
        let id = self.last.checked_add(1)?;
        self.last = id;
        self.names.insert(id, name.to_owned());
        Some(id)
    }

    /// Takes tenant `id` out; its number is not given again.
    pub fn remove(&mut self, id: u32) {
        self.names.remove(&id);
    }

    /// Writes the table into Group 0's snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.u32(self.last).var(self.names.len() as u64);
        for (id, n) in &self.names {
            e.u32(*id).str(n);
        }
    }

    /// Reads what [`Tenants::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Tenants, DecodeError> {
        let mut t = Tenants { last: d.u32()?, ..Tenants::default() };
        for _ in 0..d.var()? {
            let id = d.u32()?;
            t.names.insert(id, d.str()?.to_owned());
        }
        Ok(t)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_table_reads_back_as_written() {
        let mut t = Tenants::default();
        assert_eq!((t.add("a"), t.add("b")), (Some(1), Some(2)));
        t.remove(1);
        let mut e = Encoder::new();
        t.encode(&mut e);
        let bytes = e.finish();
        assert_eq!(Tenants::decode(&mut Decoder::new(&bytes)), Ok(t.clone()));
        // The numbers run out rather than come back.
        let mut full = Tenants { last: u32::MAX, ..Tenants::default() };
        assert_eq!(full.add("z"), None);
        assert!(valid_name("a") && !valid_name(""));
    }
}
