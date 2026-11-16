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
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb::engine::core::alien;
use teseladb::engine::core::app_template::{AppTemplate, Config};
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;

use super::core_loop::{CoreLogic, CoreSender};
use super::disk_port::{CoreDisk, DiskWaker};
use super::driver::Driver;
use super::peers::{self, AcceptCtx, Net};
use super::post::{self, CoreIo, Gate, Inbox, ShardPost, Wake};
use crate::device_log::LogPort;
use crate::handshake::AcceptorOptions;
use crate::peer::Hello;
use crate::secure::PeerTls;

/// How long a start waits for the engine's shards to come up.
const START_LIMIT: Duration = Duration::from_secs(60);

/// How a node's engine starts.
#[derive(Clone, Debug, Default)]
pub struct EngineConfig {
    /// Shards: the node's cores.
    pub shards: u16,
    /// The CPU of each shard, in shard order; empty: the shards are not pinned.
    pub cpus: Vec<usize>,
    /// Memory for all the shards, in bytes; `None`: the engine's default.
    pub memory: Option<u64>,
}

impl EngineConfig {
    /// The engine's command line.
    fn args(&self) -> Vec<String> {
        let mut a = vec!["teseladb".to_owned(), "--smp".to_owned(), self.shards.max(1).to_string()];
        match self.cpus.is_empty() {
            true => a.extend(["--thread-affinity".to_owned(), "0".to_owned()]),
            false => {
                let list: Vec<String> = self.cpus.iter().map(ToString::to_string).collect();
                a.extend(["--cpuset".to_owned(), list.join(",")]);
            }
        }
        if let Some(m) = self.memory {
            a.extend(["--memory".to_owned(), m.to_string()]);
        }
        // The node writes its own log; the engine says only what goes wrong.
        a.extend(["--default-log-level".to_owned(), "error".to_owned()]);
        a
    }
}

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
        let (ready_tx, ready_rx) = mpsc::sync_channel::<()>(1);
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
                let _ = ready_tx.send(());
                make_coroutine(async move {
                    wake.wait().await;
                    Ok(())
                })
            });
            drop(app);
            // The next engine of the process registers its own instance.
            alien::internal::DEFAULT_INSTANCE.store(std::ptr::null_mut(), Ordering::Release);
            code
        })?;
        match ready_rx.recv_timeout(START_LIMIT) {
            Ok(()) => Ok(NodeEngine {
                thread: Some(thread),
                gate: Arc::default(),
                shards: cfg.shards.max(1),
            }),
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

    fn post(&self, shard: u16) -> ShardPost {
        ShardPost::new(u32::from(shard), self.gate.clone())
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

    /// Opens a core on `shard`: `build` makes its logic and disk executor there (its state is
    /// built on its shard, so it need not be `Send`), the task drives it with `cfg` and `log`
    /// (its part of the node's device log, 012 FR-035) until a stop, and `finish` turns the logic
    /// and the executor into what [`CoreHandle::join`] gives back.
    ///
    /// # Errors
    ///
    /// What `build` failed with, or an error of `E` made from the engine's when it stopped.
    pub fn start_core<L, X, R, E, B, F>(
        &self,
        shard: u16,
        cfg: CoreConfig,
        log: Option<LogPort>,
        build: B,
        finish: F,
    ) -> Result<CoreHandle<L::Msg, R>, E>
    where
        L: CoreLogic + 'static,
        X: CoreDisk,
        R: Send + 'static,
        E: Send + 'static + From<io::Error>,
        B: FnOnce() -> Result<(L, X), E> + Send + 'static,
        F: FnOnce(L, X, bool) -> R + Send + 'static,
    {
        let (built_tx, built_rx) = mpsc::sync_channel::<Result<(), E>>(1);
        let (done_tx, done_rx) = mpsc::sync_channel::<R>(1);
        let post = self.post(shard);
        let gate = self.gate.clone();
        let sent = post.clone().post(move || {
            let (logic, mut disk) = match build() {
                Ok(v) => v,
                Err(e) => return drop(built_tx.send(Err(e))),
            };
            let io = Rc::new(CoreIo::default());
            let inbox = Rc::new(Inbox::<L::Msg>::new());
            disk.attach(DiskWaker::new(io.clone(), post.clone()));
            let mut log = log;
            if let Some(port) = log.as_mut() {
                let back = post.clone();
                port.attach(Box::new(move |d| back.post_log(Instant::now(), d)));
            }
            let net = Net::new(cfg.hello, cfg.tls, cfg.seed, io.clone(), gate, cfg.origin);
            let mut driver =
                Driver::new(logic, disk, net, io.clone(), inbox.clone(), log, cfg.tick, cfg.origin);
            post::install(io, inbox);
            let _ = built_tx.send(Ok(()));
            drop(make_coroutine(async move {
                driver.run().await;
                post::uninstall();
                let Driver { logic, disk, disk_gone, .. } = driver;
                let _ = done_tx.send(finish(logic, disk, disk_gone));
                Ok::<(), ExceptionPtr>(())
            }));
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
        // Shard 0 owns the listening socket.
        let c = ctx(0);
        let bound = self
            .run_on(0, move || peers::listen_here(at, true, cores, c))??
            .ok_or_else(|| io::Error::other("the listening socket has no address"))?;
        for shard in base..base + cores {
            if shard == 0 {
                continue;
            }
            let c = ctx(shard);
            self.run_on(shard, move || peers::listen_here(at, false, cores, c))??;
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
