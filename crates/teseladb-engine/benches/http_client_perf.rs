//! HTTP client request throughput over an in-process loopback connection.
//!
//! A client sends requests to a minimal server through loopback sockets (no kernel
//! networking) and the request rate is printed.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "../tests/common/loopback_socket.rs"]
mod loopback_socket;

use std::pin::Pin;
use std::time::Instant;

use loopback_socket::{LoopbackConnectionFactory, LoopbackSocketImpl};
use teseladb::engine::core::abort_source::AbortSource;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::memory;
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::when_all::when_all;
use teseladb::engine::http::client::experimental::{Client, ReplyHandler, RetryRequests};
use teseladb::engine::http::connection_factory::experimental::ConnectionFactory;
use teseladb::engine::http::reply::{Reply, StatusType};
use teseladb::engine::http::request::Request;
use teseladb::engine::net::api::{AcceptResult, ConnectedSocket, ServerSocket};
use teseladb::engine::net::socket_defs::{Ipv4Addr, SocketAddress, Transport};
use teseladb::engine::net::stack::SocketImpl;
use teseladb::engine::testing::linux_perf_event::LinuxPerfEvent;

struct Server {
    ss: ServerSocket,
    cs: ConnectedSocket,
    in_: InputStream<u8>,
    out: OutputStream<u8>,
    req: Sstring,
}

impl Server {
    fn new(lcf: &mut LoopbackConnectionFactory) -> Self {
        Server {
            ss: lcf.get_server_socket(),
            cs: ConnectedSocket::new(),
            in_: InputStream::default(),
            out: OutputStream::default(),
            req: Sstring::new(),
        }
    }

    fn run_serve_loop(&mut self) -> Future<()> {
        let this: *mut Self = self;
        make_coroutine(async move {
            // SAFETY: the server outlives serve(), which is waited for in main.
            let me = unsafe { &mut *this };
            loop {
                let buf = me.in_.read().await?;
                if buf.empty() {
                    return Ok(());
                }
                me.req += &Sstring::from_bytes(buf.as_slice());
                if me.req.as_str().ends_with("\r\n\r\n") {
                    let r200 = Sstring::from("HTTP/1.1 200 OK\r\nHost: test\r\n\r\n");
                    me.out.write_str(r200.as_str()).await?;
                    me.out.flush().await?;
                    me.req = Sstring::new();
                }
            }
        })
    }

    fn serve(&mut self) -> Future<()> {
        let this: *mut Self = self;
        self.ss.accept().then(move |ar: AcceptResult| {
            // SAFETY: as above.
            let me = unsafe { &mut *this };
            me.cs = ar.connection;
            me.in_ = me.cs.input(Default::default());
            me.out = me.cs.output(8192);
            me.run_serve_loop().finally(move || {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                when_all((me.in_.close(), me.out.close())).discard_result()
            })
        })
    }
}

struct LoopbackHttpFactory {
    lsi: LoopbackSocketImpl,
}

impl LoopbackHttpFactory {
    fn new(f: &mut LoopbackConnectionFactory) -> Self {
        LoopbackHttpFactory { lsi: LoopbackSocketImpl::new(f, None) }
    }
}

impl ConnectionFactory for LoopbackHttpFactory {
    fn make(&mut self, _as: *mut AbortSource) -> Future<ConnectedSocket> {
        self.lsi.connect(
            SocketAddress::from_ipv4_addr(Ipv4Addr::default()),
            SocketAddress::from_ipv4_addr(Ipv4Addr::default()),
            Transport::TCP,
        )
    }
}

struct Stats {
    ts: Instant,
    mallocs: u64,
    tasks: u64,
    instructions: u64,
    cpu_cycles: u64,
}

struct HttpClient {
    cln: Pin<Box<Client>>,
    warmup_limit: u32,
    limit: u32,
    instructions: LinuxPerfEvent,
    cpu_cycles: LinuxPerfEvent,
}

impl HttpClient {
    fn new(lcf: &mut LoopbackConnectionFactory, ops: u32, warmup: u32) -> Self {
        HttpClient {
            cln: Box::pin(Client::with_factory(
                Box::new(LoopbackHttpFactory::new(lcf)),
                Client::DEFAULT_MAX_CONNECTIONS,
                RetryRequests::NO,
                Client::DEFAULT_MAX_BYTES_TO_DRAIN,
            )),
            warmup_limit: warmup,
            limit: ops,
            instructions: LinuxPerfEvent::user_instructions_retired(),
            cpu_cycles: LinuxPerfEvent::user_cpu_cycles_retired(),
        }
    }

    fn stats_snapshot(&mut self) -> Stats {
        Stats {
            ts: Instant::now(),
            mallocs: memory::stats().mallocs(),
            // SAFETY: the reactor of this shard is running the benchmark.
            tasks: unsafe { (*engine()).get_sched_stats() }.tasks_processed,
            instructions: self.instructions.read(),
            cpu_cycles: self.cpu_cycles.read(),
        }
    }

    fn make_requests(&mut self, nr: u32) -> Future<()> {
        let this: *mut Self = self;
        make_coroutine(async move {
            for _ in 0..nr {
                let req = Request::make(
                    Sstring::from("GET"),
                    Sstring::from("test"),
                    Sstring::from("/test"),
                );
                // SAFETY: the client outlives work(), which is waited for in main.
                unsafe { &mut *this }
                    .cln
                    .as_mut()
                    .make_request(
                        req,
                        ReplyHandler::new(|_rep: &Reply, _in: InputStream<u8>| {
                            make_ready_future(())
                        }),
                        Some(StatusType::OK),
                    )
                    .await?;
            }
            Ok(())
        })
    }

    fn work(&mut self) -> Future<()> {
        let this: *mut Self = self;
        println!("Warming up with {} requests", self.warmup_limit);
        self.make_requests(self.warmup_limit)
            .then(move |()| {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                println!("Warmup finished, making {} requests", me.limit);
                let start_stats = me.stats_snapshot();
                me.instructions.enable();
                me.cpu_cycles.enable();
                me.make_requests(me.limit).then(move |()| {
                    // SAFETY: as above.
                    let me = unsafe { &mut *this };
                    me.instructions.disable();
                    me.cpu_cycles.disable();
                    let end_stats = me.stats_snapshot();
                    let limit = me.limit;
                    let delta = (end_stats.ts - start_stats.ts).as_secs_f64() * 1e6 / f64::from(limit);
                    let allocs = (end_stats.mallocs - start_stats.mallocs) as f64 / f64::from(limit);
                    let tasks = (end_stats.tasks - start_stats.tasks) as f64 / f64::from(limit);
                    let insns = (end_stats.instructions - start_stats.instructions) / u64::from(limit);
                    let cycles = (end_stats.cpu_cycles - start_stats.cpu_cycles) / u64::from(limit);
                    println!(
                        "Made {} requests, {:.3} usec/op, {:.1} allocs/op, {:.1} tasks/op, {} insns/op, {} cycles/op",
                        limit, delta, allocs, tasks, insns, cycles
                    );
                })
            })
            .finally(move || {
                // SAFETY: as above.
                unsafe { (*this).cln.as_mut().close() }
            })
    }
}

pub(crate) fn main() {
    let mut at = AppTemplate::new(Default::default());
    at.add_options()
        .opt("total-ops", bpo::value::<u32>().default_value(1000000), "Total requests to make")
        .opt("warmup-ops", bpo::value::<u32>().default_value(10000), "Requests to warm up");
    let atp: *mut AppTemplate = &mut *at;
    let args: Vec<String> = std::env::args().collect();
    let status = at.run(args, move || {
        // SAFETY: the app outlives its run().
        let cfg = unsafe { (*atp).configuration() };
        let total_ops = *cfg.get::<u32>("total-ops").expect("default");
        let warmup_ops = *cfg.get::<u32>("warmup-ops").expect("default");
        async_(move || -> Result<(), ExceptionPtr> {
            let mut lcf = LoopbackConnectionFactory::new(1);
            let mut srv = Server::new(&mut lcf);
            let mut cln = HttpClient::new(&mut lcf, total_ops, warmup_ops);
            when_all((srv.serve(), cln.work())).discard_result().get()?;
            Ok(())
        })
    });
    std::process::exit(status);
}
