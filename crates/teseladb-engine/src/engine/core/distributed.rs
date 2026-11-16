//! Alias of the sharded-service container under its older name.
//!
//! [`Distributed`] is exactly [`Sharded`](crate::engine::core::sharded::Sharded): one instance of a
//! service per shard, each owned by its shard's reactor. New code should name `Sharded` directly.

/// Another name for [`Sharded`](crate::engine::core::sharded::Sharded), kept for callers that use
/// the older term; the two types are identical.
pub type Distributed<Service> = crate::engine::core::sharded::Sharded<Service>;
