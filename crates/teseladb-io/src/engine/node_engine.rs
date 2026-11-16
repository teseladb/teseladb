//! The engine of a node (023 FR-008): one shard per core, core `k` on shard `k`. A node starts it,
//! opens each core on its shard ([`NodeEngine::start_core`]), listens for its peers on the
//! engine's network ([`NodeEngine::listen_peers`]) and stops it once its cores stopped.
//!
//! The engine runs on a thread of its own, which becomes shard 0; the node's other threads reach
//! the shards through `alien` while the engine runs (D4: one engine per process).

use std::cell::RefCell;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb::engine::core::alien;
use teseladb::engine::core::app_template::{AppTemplate, Config};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::thread::async_ as in_thread;

use super::core_loop::{CoreLogic, CoreSender};
use super::disk_port::{CoreDisk, DiskWaker};
use super::driver::Driver;
use super::options::EngineConfig;
use super::peers::{self, AcceptCtx, Net};
use super::post::{self, CoreIo, Gate, Inbox, ShardPost, Wake};
use super::stats::EngineProbe;
use crate::device_log::LogPort;
use crate::handshake::AcceptorOptions;
use crate::peer::Hello;
use crate::secure::PeerTls;

/// How long a start waits for the engine's shards to come up.
const START_LIMIT: Duration = Duration::from_secs(60);

/// What a core's task needs besides its logic and disk.
#[derive(Clone, Debug)]
pub struct CoreConfig {
    /// The core's `Hello` (with its core).
    pub hello: Hello,
    /// Tick period (10 ms in production).
    pub tick: Duration,
    /// Seed of the backoff jitter.
    pub seed: u64,
    /// TLS for the connections the core dials (013 FR-006); `None` in clear.
    pub tls: Option<PeerTls>,
    /// Where the monotonic time of the node's cores starts (005 FR-028).
    pub origin: Instant,
}

thread_local! {
    /// Shard 0's wait for the stop of the engine.
    static EXIT: RefCell<Option<Rc<Wake>>> = const { RefCell::new(None) };
}

/// A running engine.
pub struct NodeEngine {
    thread: Option<JoinHandle<i32>>,
    gate: Arc<Gate>,
    shards: u16,
}

impl std::fmt::Debug for NodeEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeEngine").field("shards", &self.shards).finish()
    }
}

/// Queues closures on the shards of a running engine, from any thread, each on the next shard in
/// turn; its clones share the turn.
#[derive(Clone, Debug)]
pub struct EnginePost {
    gate: Arc<Gate>,
    shards: u16,
    next: Arc<AtomicU16>,
}

impl EnginePost {
    /// Queues `f` on a shard; `false` (and `f` dropped) once the engine stopped.
    pub fn post(&self, f: impl FnOnce() + Send + 'static) -> bool {
        let shard = self.next.fetch_add(1, Ordering::Relaxed) % self.shards.max(1);
        ShardPost::new(u32::from(shard), self.gate.clone()).post(f)
    }
}

/// A core running on its shard.
pub struct CoreHandle<M, R> {
    sender: CoreSender<M>,
    done: Receiver<R>,
}

impl<M: Send + 'static, R> CoreHandle<M, R> {
    /// The core's sender.
    pub fn sender(&self) -> CoreSender<M> {
        self.sender.clone()
    }

    /// Waits for the core's task to end (after a [`crate::engine::CoreInput::Stop`]) and returns
    /// what its `finish` made; `None` if the engine stopped first.
    pub fn join(self) -> Option<R> {
        self.done.recv().ok()
    }
}

impl NodeEngine {
    /// Starts the engine with `cfg` and waits for its shards.
    ///
    /// # Errors
    ///
    /// When another engine runs in the process, or the engine cannot start (its options, its
    /// memory or its CPUs).
    pub fn start(cfg: &EngineConfig) -> io::Result<NodeEngine> {
        if !alien::internal::default_instance().is_null() {
            return Err(io::Error::other("an engine already runs in this process"));
        }
        let args = cfg.args();
        let (ready_tx, ready_rx) = mpsc::sync_channel::<io::Result<()>>(1);
        let thread = thread::Builder::new().name("engine".into()).spawn(move || {
            let config = Config {
                name: "teseladb".into(),
                auto_handle_sigint_sigterm: false,
                ..Config::default()
            };
            let mut app = AppTemplate::new(config);
            let code = app.run(args, move || {
                let wake = Rc::new(Wake::default());
                EXIT.with(|e| *e.borrow_mut() = Some(wake.clone()));
                // SIGINT and SIGTERM through shard 0's reactor (023 FR-031).
                let _ = ready_tx.send(super::signals::install_here());
                make_coroutine(async move {
                    wake.wait().await;
                    // The disk classes are this engine's: the next engine of the process makes
                    // its own (their numbers are free again).
                    super::disk::IoGroups::release().await
                })
            });
            drop(app);
            // The next engine of the process registers its own instance.
            alien::internal::DEFAULT_INSTANCE.store(std::ptr::null_mut(), Ordering::Release);
            code
        })?;
        match ready_rx.recv_timeout(START_LIMIT) {
            Ok(Ok(())) => Ok(NodeEngine {
                thread: Some(thread),
                gate: Arc::default(),
                shards: cfg.shards.max(1),
            }),
            Ok(Err(e)) => {
                // The engine runs: it is stopped like any other before the error goes back.
                let failed = NodeEngine {
                    thread: Some(thread),
                    gate: Arc::default(),
                    shards: cfg.shards.max(1),
                };
                failed.stop();
                Err(e)
            }
            Err(_) => {
                let code = thread.join().unwrap_or(-1);
                alien::internal::DEFAULT_INSTANCE.store(std::ptr::null_mut(), Ordering::Release);
                Err(io::Error::other(format!("the engine did not start (exit code {code})")))
            }
        }
    }

    /// Its shards.
    pub fn shards(&self) -> u16 {
        self.shards
    }

    /// A way to its shards' metrics and allocator counters from other threads (FR-028, FR-030);
    /// it reaches nothing once the engine stops.
    pub fn probe(&self) -> EngineProbe {
        EngineProbe::new(self.gate.clone(), self.shards)
    }

    fn post(&self, shard: u16) -> ShardPost {
        ShardPost::new(u32::from(shard), self.gate.clone())
    }

    /// What posts to `shard` while the engine runs (the device log's writer on shard 0).
    pub(crate) fn post_to(&self, shard: u16) -> ShardPost {
        self.post(shard)
    }

    /// A handle that queues closures on the shards from any thread while the engine runs (the
    /// node's outbound clients, 023 FR-024).
    pub fn poster(&self) -> EnginePost {
        EnginePost {
            gate: self.gate.clone(),
            shards: self.shards,
            next: Arc::new(AtomicU16::new(0)),
        }
    }

    /// Runs `f` on `shard` and waits for its result (from a thread that is not a shard).
    ///
    /// # Errors
    ///
    /// When the engine stopped.
    pub fn run_on<T: Send + 'static>(
        &self,
        shard: u16,
        f: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let (tx, rx) = mpsc::sync_channel(1);
        if !self.post(shard).post(move || drop(tx.send(f()))) {
            return Err(io::Error::other("the engine stopped"));
        }
        rx.recv().map_err(|_| io::Error::other("the engine stopped"))
    }

    /// Opens a core on `shard`: `build` makes its logic, its disk executor and its part of the
    /// node's device log (012 FR-035) there, after the recovery of its log and LSM (its state is
    /// built on its shard, so it need not be `Send`, in an engine thread, so it may wait on the
    /// shard's futures with `get()`); the task drives it with `cfg` until a stop,
    /// and `finish` turns the logic and the executor (and whether the executor died) into what
    /// [`CoreHandle::join`] gives back.
    ///
    /// # Errors
    ///
    /// What `build` failed with, or an error of `E` made from the engine's when it stopped.
    pub fn start_core<L, X, R, E, B, F>(
        &self,
        shard: u16,
        cfg: CoreConfig,
        build: B,
        finish: F,
    ) -> Result<CoreHandle<L::Msg, R>, E>
    where
        L: CoreLogic + 'static,
        X: CoreDisk,
        R: Send + 'static,
        E: Send + 'static + From<io::Error>,
        B: FnOnce() -> Result<(L, X, Option<LogPort>), E> + Send + 'static,
        F: FnOnce(L, X, bool) -> R + Send + 'static,
    {
        let (built_tx, built_rx) = mpsc::sync_channel::<Result<(), E>>(1);
        let (done_tx, done_rx) = mpsc::sync_channel::<R>(1);
        let post = self.post(shard);
        let gate = self.gate.clone();
        // `build` runs in an engine thread of the shard: the recovery of the core's log and LSM
        // waits on the shard's disk executor through the synchronous `Disk` (023 FR-010).
        let sent = post.clone().post(move || {
            drop(in_thread(move || {
                let (logic, mut disk, mut log) = match build() {
                    Ok(v) => v,
                    Err(e) => return drop(built_tx.send(Err(e))),
                };
                let io = Rc::new(CoreIo::default());
                let inbox = Rc::new(Inbox::<L::Msg>::new());
                disk.attach(DiskWaker::new(io.clone()));
                if let Some(port) = log.as_mut() {
                    let back = post.clone();
                    port.attach(Box::new(move |d| back.post_log(Instant::now(), d)));
                }
                let net = Net::new(cfg.hello, cfg.tls, cfg.seed, io.clone(), gate, cfg.origin);
                let mut driver = Driver::new(
                    logic,
                    disk,
                    net,
                    io.clone(),
                    inbox.clone(),
                    log,
                    cfg.tick,
                    cfg.origin,
                );
                post::install(io, inbox);
                let _ = built_tx.send(Ok(()));
                drop(make_coroutine(async move {
                    driver.run().await;
                    post::uninstall();
                    let Driver { logic, disk, disk_gone, .. } = driver;
                    let _ = done_tx.send(finish(logic, disk, disk_gone));
                    Ok::<(), ExceptionPtr>(())
                }));
            }))
        });
        if !sent {
            return Err(E::from(io::Error::other("the engine stopped")));
        }
        match built_rx.recv() {
            Ok(Ok(())) => {
                Ok(CoreHandle { sender: CoreSender::on_shard(self.post(shard)), done: done_rx })
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Err(E::from(io::Error::other("the engine stopped"))),
        }
    }

    /// Reserves the node's peer port at `at` on shard 0 before its cores run (023 FR-015, T055):
    /// the engine listens there, so the address (a port 0 picks one) is known before the node
    /// joins, and [`NodeEngine::listen_peers`] at that address takes the socket over. Returns the
    /// address taken.
    ///
    /// # Errors
    ///
    /// When the address cannot be listened on (its kind says why: an address in use is
    /// [`io::ErrorKind::AddrInUse`]), or the engine stopped.
    pub fn reserve_peers(&self, at: SocketAddr, base: u16, cores: u16) -> io::Result<SocketAddr> {
        self.run_on(0, move || peers::reserve_here(at, cores, u32::from(base)))?
    }

    /// Listens for the node's peers at `at` (023 FR-015): the shards `base..base + cores` hold
    /// the node's cores, and the engine hands each connection to the shard of the core the
    /// dialer's source port names (one core: to `base`). Returns the address listened on.
    ///
    /// # Errors
    ///
    /// When the address cannot be listened on, or the engine stopped.
    pub fn listen_peers(
        &self,
        at: SocketAddr,
        local: &Hello,
        opts: &AcceptorOptions,
        base: u16,
        cores: u16,
    ) -> io::Result<SocketAddr> {
        let ctx = |shard: u16| AcceptCtx {
            local: local.clone(),
            opts: opts.clone(),
            base: u32::from(base),
            gate: self.gate.clone(),
            shard: u32::from(shard),
        };
        // Shard 0 owns the listening socket; the others listen on the concrete address it took,
        // by which it hands their connections over.
        let c = ctx(0);
        let bound = self
            .run_on(0, move || peers::listen_here(at, None, cores, c))??
            .ok_or_else(|| io::Error::other("the listening socket has no address"))?;
        for shard in base..base + cores {
            if shard == 0 {
                continue;
            }
            let c = ctx(shard);
            self.run_on(shard, move || peers::listen_here(at, Some(bound), cores, c))??;
        }
        Ok(bound)
    }

    /// Stops listening for peers on every shard: at `at` (as given to
    /// [`NodeEngine::listen_peers`]), or everywhere with `None`.
    pub fn stop_peers(&self, at: Option<SocketAddr>) {
        for shard in 0..self.shards {
            let _ = self.run_on(shard, move || peers::stop_listening_here(at));
        }
    }

    /// Stops the engine and waits for its thread. The node stopped its cores first.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else { return };
        // A peer port reserved and never listened on goes while shard 0 still runs: queued
        // before the stop below, it runs first.
        let _ = self.post(0).post(peers::release_here);
        // No thread that is not a shard enters the engine any more.
        self.gate.close();
        #[allow(deprecated)]
        alien::run_on_default(0, || {
            if let Some(w) = EXIT.with(|e| e.borrow_mut().take()) {
                w.wake();
            }
        });
        let _ = thread.join();
    }
}

impl Drop for NodeEngine {
    fn drop(&mut self) {
        self.shutdown();
    }
}
