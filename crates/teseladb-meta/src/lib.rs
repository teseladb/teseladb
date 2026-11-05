//! TeselaDB's Group 0 (specification 001, FR-007), sans-I/O: the cluster's nodes and their joins,
//! range descriptors with opaque bounds and their replicas, cluster settings and time ([`group0`],
//! a [`teseladb_multiraft::StateMachine`]), node liveness and clock reports on Group 0's leader
//! ([`liveness`], [`clock`]), the balancer that decides replica and leader moves ([`balancer`])
//! the cluster's first data range ([`bootstrap`], 005 FR-016) and the catalog of databases and
//! collections ([`catalog`], 006 FR-027) with its index definitions ([`indexes`], 007 FR-001),
//! the ranges by key ([`descriptors`], 007 FR-002), idempotent commands ([`request`]) and the
//! users and roles ([`security`], 013 FR-009), and the meta-ranges that take the descriptors over
//! at scale ([`meta2`], 007 FR-014).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod balancer;
pub mod bootstrap;
pub mod catalog;
pub mod changelog;
pub mod clock;
pub mod clustertime_keys;
pub mod collection_types;
pub mod command;
pub mod config_settings;
pub mod ddl_log;
pub mod descriptors;
pub mod drain;
pub mod enroll_tokens;
pub mod group0;
pub mod indexes;
pub mod liveness;
pub mod load;
pub mod meta2;
pub mod node_versions;
pub mod request;
pub mod security;
pub mod tenant_quota;
pub mod tenants;
pub mod zones;
