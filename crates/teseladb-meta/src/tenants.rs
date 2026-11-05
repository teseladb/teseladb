//! Tenants (007 FR-025, D-014): Group 0 keeps their names and numbers. Tenant 0 is the default
//! one, which every cluster has without creating it. A tenant's databases live in the same
//! catalog as everyone's, with their names qualified by the tenant ([`qualified_db`]): RefDb's
//! database names never hold a zero byte, so the default tenant's names stay as they are and no
//! name of one tenant meets another's. The collection numbers are shared by every tenant, and a
//! collection's keys carry its tenant. Each tenant but the default one also has its limits, its
//! switches, its service user, its access list and whether it is suspended ([`TenantState`],
//! 007 FR-030, FR-034 to FR-036).

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

/// The tenant of the user or role whose stored document is `doc`: its [`TENANT_FIELD`], or the default
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

/// The service user of a tenant (007 FR-036): its user and the database it is defined in. Its
/// connections and operations do not count against the tenant's limits nor its activity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceUser {
    /// The user's name.
    pub user: String,
    /// The database the user is defined in.
    pub db: String,
}

/// The limits and switches of a tenant (007 FR-030, FR-036): 0 is no limit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantLimits {
    /// Bytes of data and indexes in the cluster.
    pub storage_bytes: u64,
    /// Authenticated connections on each node.
    pub max_connections: u32,
    /// Commands per second on each node.
    pub ops_per_second: u32,
    /// API keys.
    pub max_api_keys: u32,
    /// The longest `maxTimeMS` of an operation, and the one an operation without it gets.
    pub max_time_ms: u32,
    /// Operations running at once on each node.
    pub max_concurrent_ops: u32,
    /// Bytes written per second on each node.
    pub max_write_bytes_per_second: u64,
    /// Databases.
    pub max_databases: u32,
    /// Collections and views.
    pub max_collections: u32,
    /// Search indexes.
    pub max_search_indexes: u32,
    /// Whether the REST API of 019 serves the tenant.
    pub http_enabled: bool,
    /// Whether the MCP server of 019 serves the tenant.
    pub mcp_enabled: bool,
    /// The service user, if it has one.
    pub service_user: Option<ServiceUser>,
}

impl Default for TenantLimits {
    fn default() -> Self {
        TenantLimits {
            storage_bytes: 0,
            max_connections: 0,
            ops_per_second: 0,
            max_api_keys: 0,
            max_time_ms: 0,
            max_concurrent_ops: 0,
            max_write_bytes_per_second: 0,
            max_databases: 0,
            max_collections: 0,
            max_search_indexes: 0,
            http_enabled: true,
            mcp_enabled: true,
            service_user: None,
        }
    }
}

impl TenantLimits {
    /// Writes the limits into an entry or a snapshot.
    pub fn encode(&self, e: &mut Encoder) {
        e.u64(self.storage_bytes)
            .u32(self.max_connections)
            .u32(self.ops_per_second)
            .u32(self.max_api_keys)
            .u32(self.max_time_ms)
            .u32(self.max_concurrent_ops)
            .u64(self.max_write_bytes_per_second)
            .u32(self.max_databases)
            .u32(self.max_collections)
            .u32(self.max_search_indexes)
            .bool(self.http_enabled)
            .bool(self.mcp_enabled)
            .bool(self.service_user.is_some());
        if let Some(u) = &self.service_user {
            e.str(&u.user).str(&u.db);
        }
    }

    /// Reads what [`TenantLimits::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<TenantLimits, DecodeError> {
        Ok(TenantLimits {
            storage_bytes: d.u64()?,
            max_connections: d.u32()?,
            ops_per_second: d.u32()?,
            max_api_keys: d.u32()?,
            max_time_ms: d.u32()?,
            max_concurrent_ops: d.u32()?,
            max_write_bytes_per_second: d.u64()?,
            max_databases: d.u32()?,
            max_collections: d.u32()?,
            max_search_indexes: d.u32()?,
            http_enabled: d.bool()?,
            mcp_enabled: d.bool()?,
            service_user: if d.bool()? {
                Some(ServiceUser { user: d.str()?.to_owned(), db: d.str()?.to_owned() })
            } else {
                None
            },
        })
    }
}

/// What Group 0 keeps of a tenant besides its name (007 FR-030, FR-034, FR-035).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TenantState {
    /// Its limits and switches.
    pub limits: TenantLimits,
    /// Whether support suspended it: no user of it signs in and its connections close.
    pub suspended: bool,
    /// The sources its users may connect from, as CIDR blocks; empty is from anywhere.
    pub allowed_sources: Vec<String>,
}

impl TenantState {
    fn encode(&self, e: &mut Encoder) {
        self.limits.encode(e);
        e.bool(self.suspended).var(self.allowed_sources.len() as u64);
        for s in &self.allowed_sources {
            e.str(s);
        }
    }

    fn decode(d: &mut Decoder<'_>) -> Result<TenantState, DecodeError> {
        let limits = TenantLimits::decode(d)?;
        let suspended = d.bool()?;
        let mut allowed_sources = Vec::new();
        for _ in 0..d.var()? {
            allowed_sources.push(d.str()?.to_owned());
        }
        Ok(TenantState { limits, suspended, allowed_sources })
    }
}

/// The tenants Group 0 knows, by number; the default one is not among them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tenants {
    names: BTreeMap<u32, String>,
    states: BTreeMap<u32, TenantState>,
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

    /// The name of tenant `id`.
    pub fn name_of(&self, id: u32) -> Option<&str> {
        self.names.get(&id).map(String::as_str)
    }

    /// The limits, access list and suspension of tenant `id`; none for the default tenant or one
    /// that does not exist.
    pub fn state(&self, id: u32) -> Option<&TenantState> {
        self.states.get(&id)
    }

    /// The same, to change.
    pub(crate) fn state_mut(&mut self, id: u32) -> Option<&mut TenantState> {
        self.states.get_mut(&id)
    }

    /// Adds tenant `name` with the next number, never given before, without limits.
    pub fn add(&mut self, name: &str) -> Option<u32> {
        let id = self.last.checked_add(1)?;
        self.last = id;
        self.names.insert(id, name.to_owned());
        self.states.insert(id, TenantState::default());
        Some(id)
    }

    /// Takes tenant `id` out; its number is not given again.
    pub fn remove(&mut self, id: u32) {
        self.names.remove(&id);
        self.states.remove(&id);
    }

    /// Writes the table into Group 0's snapshot, with each tenant's state unless `with_state` is
    /// false (the format before it, 007 FR-030).
    pub fn encode_with(&self, e: &mut Encoder, with_state: bool) {
        e.u32(self.last).var(self.names.len() as u64);
        for (id, n) in &self.names {
            e.u32(*id).str(n);
            if with_state {
                self.states.get(id).cloned().unwrap_or_default().encode(e);
            }
        }
    }

    /// Writes the table with each tenant's state.
    pub fn encode(&self, e: &mut Encoder) {
        self.encode_with(e, true);
    }

    /// Reads what [`Tenants::encode_with`] wrote; without the state, every tenant has none.
    pub fn decode_with(d: &mut Decoder<'_>, with_state: bool) -> Result<Tenants, DecodeError> {
        let mut t = Tenants { last: d.u32()?, ..Tenants::default() };
        for _ in 0..d.var()? {
            let id = d.u32()?;
            t.names.insert(id, d.str()?.to_owned());
            let state = if with_state { TenantState::decode(d)? } else { TenantState::default() };
            t.states.insert(id, state);
        }
        Ok(t)
    }

    /// Reads what [`Tenants::encode`] wrote.
    pub fn decode(d: &mut Decoder<'_>) -> Result<Tenants, DecodeError> {
        Tenants::decode_with(d, true)
    }
}

impl crate::group0::Group0Sm {
    /// `SetTenantLimits`, `SetTenantSuspended` and `SetTenantAccess`: `change` on the state of
    /// tenant `tenant`, which is not the default one, once every node reads these entries.
    pub(crate) fn change_tenant(
        &mut self,
        tenant: u32,
        change: impl FnOnce(&mut TenantState),
    ) -> Result<crate::group0::Group0Result, crate::group0::Group0Error> {
        use crate::group0::{Group0Error, Group0Result, SNAPSHOT_VERSION};
        if !crate::node_versions::all_run(self, SNAPSHOT_VERSION) {
            return Err(Group0Error::NotEveryNodeRunsIt);
        }
        if tenant == DEFAULT_TENANT {
            return Err(Group0Error::InvalidOptions);
        }
        change(self.tenants.state_mut(tenant).ok_or(Group0Error::TenantNotFound)?);
        Ok(Group0Result::Ok)
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
