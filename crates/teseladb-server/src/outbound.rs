//! The engine work of the node's outbound clients (023 FR-024, FR-025): the LDAP, KMIP and HTTPS
//! clients talk to their servers over engine connections, with futures, on a shard. A shard awaits
//! their futures; a thread that is not a shard (the node's start-up, a command that still runs on
//! a thread of its own) hands the work to the node's engine with [`call`] and waits for its
//! result, and starts background work with [`spawn`].
//!
//! The node installs its engine's [`Runner`] once the engine runs (D4: one engine per process)
//! and takes it out before the engine stops; before the node's engine runs, the start-up asks its
//! KMIP server inside [`with_engine`]. Without a runner, [`call`] and [`spawn`] fail with
//! [`NO_ENGINE`].

use std::sync::mpsc;
use std::sync::{Arc, PoisonError, RwLock};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::reactor::engine_is_ready;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb_io::engine::{EngineConfig, EnginePost, NodeEngine};

/// The error of [`call`] and [`spawn`] when no engine runs in the process.
pub const NO_ENGINE: &str = "the node's engine is not running";
/// The error of [`call`] from a shard, which has to await the future instead of blocking.
pub const ON_SHARD: &str = "a shard cannot wait for engine work: it awaits the future";
/// The error of [`call`] when the engine stops before the work ends.
pub const STOPPED: &str = "the node's engine stopped";

/// Where the work runs: the foreground of the node (a login, the start-up) or its background
/// (a periodic check), each in the engine's scheduling group for that class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Work a client or the start-up waits for.
    Foreground,
    /// Work nobody waits for, which yields to the foreground.
    Background,
}

/// Runs closures on a shard of the node's engine, from any thread.
pub trait Runner: Send + Sync {
    /// Queues `task` on a shard, to run in the scheduling group of `class`; `false` (and `task`
    /// dropped) once the engine stopped.
    fn post(&self, task: Box<dyn FnOnce() + Send>, class: Class) -> bool;
}

static RUNNER: RwLock<Option<Arc<dyn Runner>>> = RwLock::new(None);

/// The node's engine as a [`Runner`]: each task on the next shard in turn. The engine has no
/// scheduling groups of the node's classes yet, so both classes run in its default group.
#[derive(Debug)]
pub struct NodeRunner(pub EnginePost);

impl Runner for NodeRunner {
    fn post(&self, task: Box<dyn FnOnce() + Send>, _class: Class) -> bool {
        self.0.post(task)
    }
}

/// Memory of the engine [`with_engine`] starts: the TLS and the buffers of a few connections.
const START_ENGINE_MEMORY: u64 = 128 << 20;

/// Runs `f` with an engine for the outbound clients: the one installed, or else an engine of one
/// shard of its own for the duration of `f` (the node's start-up asks its KMIP server before the
/// node's engine runs; D4 allows one engine at a time, and this one stops before the node's
/// starts).
///
/// # Errors
///
/// When the engine cannot start.
pub fn with_engine<T>(f: impl FnOnce() -> T) -> Result<T, String> {
    if runner().is_ok() {
        return Ok(f());
    }
    let cfg =
        EngineConfig { shards: 1, memory: Some(START_ENGINE_MEMORY), ..EngineConfig::default() };
    let engine = NodeEngine::start(&cfg).map_err(|e| format!("the engine: {e}"))?;
    install(Arc::new(NodeRunner(engine.poster())));
    let out = f();
    uninstall();
    engine.stop();
    Ok(out)
}

/// Installs the engine's runner; returns the one it replaces.
pub fn install(runner: Arc<dyn Runner>) -> Option<Arc<dyn Runner>> {
    RUNNER.write().unwrap_or_else(PoisonError::into_inner).replace(runner)
}

/// Takes the runner out, before the engine stops.
pub fn uninstall() -> Option<Arc<dyn Runner>> {
    RUNNER.write().unwrap_or_else(PoisonError::into_inner).take()
}

fn runner() -> Result<Arc<dyn Runner>, String> {
    let r = RUNNER.read().unwrap_or_else(PoisonError::into_inner);
    r.clone().ok_or_else(|| NO_ENGINE.to_owned())
}

/// The text of an engine error: an I/O error's own message, without the type name the engine
/// adds.
pub fn text(e: &ExceptionPtr) -> String {
    match e.downcast_ref::<std::io::Error>() {
        Some(io) => io.to_string(),
        None => e.to_string(),
    }
}

/// Runs the future `f` makes on a shard of the node's engine and waits for its result. In an
/// engine thread (a command, a handler of the node's ports) the future runs on its own shard and
/// the wait parks the thread on the engine's answer channel (023 FR-018), with the command's
/// state saved meanwhile.
///
/// # Errors
///
/// [`NO_ENGINE`] without an engine, [`ON_SHARD`] on a shard outside an engine thread, [`STOPPED`]
/// when the engine stops first, and the text of the future's own error.
pub fn call<T: Send + 'static>(
    f: impl FnOnce() -> Future<T> + Send + 'static,
) -> Result<T, String> {
    if teseladb_io::engine::wait::in_engine_thread() {
        return call_here(f());
    }
    if engine_is_ready() {
        return Err(ON_SHARD.to_owned());
    }
    let (tx, rx) = mpsc::sync_channel::<Result<T, ExceptionPtr>>(1);
    let task = Box::new(move || {
        let _ = f().then_wrapped(move |r: Future<T>| {
            let _ = tx.send(r.get());
            make_ready_future(())
        });
    });
    if !runner()?.post(task, Class::Foreground) {
        return Err(STOPPED.to_owned());
    }
    match rx.recv() {
        Ok(r) => r.map_err(|e| text(&e)),
        Err(_) => Err(STOPPED.to_owned()),
    }
}

/// Waits in an engine thread for `work`, which runs on this shard.
fn call_here<T: Send + 'static>(work: Future<T>) -> Result<T, String> {
    let (tx, rx) = teseladb_io::engine::answer::channel::<Result<T, ExceptionPtr>>();
    run_in_background(work.then_wrapped(move |r: Future<T>| {
        let _ = tx.send(r.get());
        make_ready_future(())
    }));
    match rx.recv() {
        Ok(r) => r.map_err(|e| text(&e)),
        Err(_) => Err(STOPPED.to_owned()),
    }
}

/// The engine error of a client's failure `msg`; [`text`] gives `msg` back.
pub fn fail(msg: String) -> ExceptionPtr {
    ExceptionPtr::from(std::io::Error::other(msg))
}

/// The engine future of a client's work that fails with a message.
pub fn future<T: 'static>(
    work: impl std::future::Future<Output = Result<T, String>> + 'static,
) -> Future<T> {
    make_coroutine(async move { work.await.map_err(fail) })
}

/// Background work started by [`spawn`].
#[derive(Debug)]
pub struct Spawned {
    done: mpsc::Receiver<()>,
}

impl Spawned {
    /// Waits until the work ended (or the engine dropped it).
    pub fn join(self) {
        let _ = self.done.recv();
    }
}

/// Starts the future `f` makes on a shard of the node's engine, in its background group.
///
/// # Errors
///
/// [`NO_ENGINE`] without an engine, [`STOPPED`] once it stopped.
pub fn spawn(f: impl FnOnce() -> Future<()> + Send + 'static) -> Result<Spawned, String> {
    let (tx, done) = mpsc::sync_channel::<()>(1);
    let task = Box::new(move || {
        let _ = f().then_wrapped(move |_: Future<()>| {
            let _ = tx.send(());
            make_ready_future(())
        });
    });
    if !runner()?.post(task, Class::Background) {
        return Err(STOPPED.to_owned());
    }
    Ok(Spawned { done })
}

/// The engine of a test process, as the runner of the clients of the tests that run no node.
///
/// It is not the engine's test runner: that runner's reactor waits for its next job inside a
/// task (as the reference's `test_runner` does), so the futures a task started would only go on
/// while another job runs, and a client's exchange would wait forever. It is a node engine of one
/// shard: for a test binary, [`install`] starts one that runs until the process ends (D4: one
/// engine per process); the unit tests of this crate, which share one process with tests that
/// need no engine or start their own, take turns instead ([`scoped`], [`exclusive`]).
pub mod test_engine {
    use std::sync::{Arc, OnceLock};

    use teseladb_io::engine::{EngineConfig, NodeEngine};

    use super::{NodeRunner, START_ENGINE_MEMORY};

    fn config() -> EngineConfig {
        EngineConfig { shards: 1, memory: Some(START_ENGINE_MEMORY), ..EngineConfig::default() }
    }

    static ENGINE: OnceLock<NodeEngine> = OnceLock::new();

    /// The engine of the test process, started the first time.
    ///
    /// # Panics
    ///
    /// When it does not start (another engine runs in the process).
    pub fn engine() -> &'static NodeEngine {
        ENGINE.get_or_init(|| NodeEngine::start(&config()).expect("the test engine starts"))
    }

    /// Installs the engine of the test process as the runner, started the first time.
    pub fn install() {
        super::install(Arc::new(NodeRunner(engine().poster())));
    }

    /// The turn of the unit tests that depend on the process's engine or on what a node's
    /// start-up takes from the whole process (its pending log lines).
    #[cfg(test)]
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Waits for the turn of a unit test that starts its own engine, needs none to run, or
    /// reads the process's pending log lines; the turn ends when the guard goes.
    #[cfg(test)]
    pub(crate) fn exclusive() -> std::sync::MutexGuard<'static, ()> {
        TURN.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// An engine of one shard installed as the runner for one unit test, in its turn; it stops
    /// when the guard goes.
    #[cfg(test)]
    pub(crate) struct Scoped {
        engine: Option<NodeEngine>,
        _turn: std::sync::MutexGuard<'static, ()>,
    }

    #[cfg(test)]
    impl Scoped {
        /// The engine.
        pub(crate) fn engine(&self) -> &NodeEngine {
            self.engine.as_ref().expect("the engine runs until the guard goes")
        }
    }

    #[cfg(test)]
    impl Drop for Scoped {
        fn drop(&mut self) {
            super::uninstall();
            if let Some(e) = self.engine.take() {
                e.stop();
            }
        }
    }

    /// Starts an engine for this unit test once it has its turn, and installs it as the runner.
    #[cfg(test)]
    pub(crate) fn scoped() -> Scoped {
        let turn = exclusive();
        let engine = NodeEngine::start(&config()).expect("the test engine starts");
        super::install(Arc::new(NodeRunner(engine.poster())));
        Scoped { engine: Some(engine), _turn: turn }
    }
}
