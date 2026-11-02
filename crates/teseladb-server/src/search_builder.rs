//! The builder of the node's search indexes (011 FR-014, T040): a thread with its own backend
//! that fills each new index, or each new definition version, over the documents already written,
//! outside the command that created it. Meanwhile `$listSearchIndexes` shows the index
//! `BUILDING` and its queries are refused as `refsearch` refuses them in `INITIAL_SYNC` (an older
//! definition keeps serving while a new one builds); at the end the index is `READY`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use teseladb_wire::commands::search_indexes::build_pending;
use teseladb_wire::commands::{Backend, Shared, lock};

use crate::backend::ServerBackend;

/// How often the builder looks at the catalog for indexes to build: other nodes create indexes
/// too, so it cannot wait only for this node's commands.
pub const BUILD_EVERY: Duration = Duration::from_millis(250);

/// Starts the builder over `backend`, until `stop`; from now on the node's commands leave the
/// building to it.
pub fn spawn(
    backend: ServerBackend,
    shared: Arc<Shared>,
    stop: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    lock(&shared).search.set_background();
    thread::Builder::new().name("search-builder".into()).spawn(move || run(backend, &shared, &stop))
}

fn run(mut backend: ServerBackend, shared: &Shared, stop: &AtomicBool) {
    while !stop.load(Ordering::Acquire) {
        // A pass that fails (no leader for a range) goes again at the next one.
        let _ = build_pending(&mut backend, shared);
        // Its reads of the indexes no longer hold back their changes (009 FR-006).
        backend.end_operation();
        thread::park_timeout(BUILD_EVERY);
    }
}
