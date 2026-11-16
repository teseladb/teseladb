//! The node's HTTP port (019) and administration port (007) on its engine (023 FR-022, FR-023):
//! started from the node's start-up thread on shard 0, which listens and has every other shard
//! listen too, and stopped before the engine stops.
//!
//! The node's services ([`NodeHttp`], the administration handler) wait for the cores on the
//! engine's answer channel: each of their calls runs in an engine thread of the shard that took
//! the request ([`crate::engine_jobs`], 023 T049), as a client's command does. The port itself
//! (connections, TLS, the protocols, deadlines, limits, CORS) runs on the shards.

use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use teseladb_http::metrics::HttpMetrics;

use crate::admin_http::{AdminTls, Handler};
use crate::http_listener::HttpPortConfig;
use crate::http_service::NodeHttp;

pub use on_linux::{AdminPort, HttpPort};

/// The port the nodes talk to each other on (005), in the errors of a port that cannot open.
pub const PEERS: &str = "peer port";
/// The administration port (007).
pub const ADMIN: &str = "administration port";
/// The HTTP port of the REST API (019).
pub const HTTP: &str = "HTTP port";

/// The error of `port` that could not be opened at `addr`: it keeps the kind of `e` and names
/// the port and its address, so whoever runs the node knows which one another process holds (as
/// the reference server names the address it could not listen on).
#[must_use]
pub fn port_error(port: &str, addr: SocketAddr, e: &io::Error) -> io::Error {
    io::Error::new(e.kind(), format!("{port} {addr}: {e}"))
}

/// Starts the administration port at `at`, answering with `handler`, over TLS with `tls`; the
/// port's TLS is rotated with the node's certificates, `certs`.
///
/// # Errors
///
/// When the address does not listen, or outside Linux.
pub fn start_admin(
    at: SocketAddr,
    handler: Handler,
    tls: Option<AdminTls>,
    certs: Option<&crate::certs::Certificates>,
) -> io::Result<AdminPort> {
    AdminPort::start(at, handler, tls, certs)
}

/// Starts the HTTP port of `cfg` with `service`, counting its requests in `metrics`; its TLS is
/// rotated with the node's certificates, `certs`.
///
/// # Errors
///
/// When an address does not listen, or outside Linux.
pub fn start_http(
    cfg: HttpPortConfig,
    service: Arc<NodeHttp>,
    metrics: Arc<Mutex<HttpMetrics>>,
    certs: Option<&crate::certs::Certificates>,
) -> io::Result<HttpPort> {
    HttpPort::start(cfg, service, metrics, certs)
}

mod on_linux {
    use std::cell::RefCell;
    use std::io;
    use std::net::SocketAddr;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use teseladb::engine::core::future::{Future, make_ready_future};
    use teseladb::engine::core::reactor::internal_misc::run_in_background;
    use teseladb_http::http1::Request;
    use teseladb_http::metrics::HttpMetrics;
    use teseladb_wire::commands::http_parameters::{HttpOptions, http_options};

    use crate::admin_http::shard as admin;
    use crate::admin_http::{AdminTls, Caller as AdminCaller, Handler, HttpRequest};
    use crate::engine_jobs;
    use crate::http_listener::shard::{self as http, Cancel, MakeService, Reply};
    use crate::http_listener::{self as threaded, Caller, HttpPortConfig, HttpService};
    use crate::http_service::NodeHttp;
    use crate::shard_client::block_on;

    /// How long a stop waits for the calls of the ports still running.
    const DRAIN: Duration = Duration::from_secs(10);
    /// How long a shard keeps the options it read.
    const OPTIONS_TTL: Duration = Duration::from_secs(1);

    /// The administration port of the node.
    #[derive(Debug)]
    pub struct AdminPort(admin::AdminPort);

    impl AdminPort {
        pub(super) fn start(
            at: SocketAddr,
            handler: Handler,
            tls: Option<AdminTls>,
            certs: Option<&crate::certs::Certificates>,
        ) -> io::Result<AdminPort> {
            let make: admin::MakeHandler = Arc::new(move || -> admin::Handler {
                let handler = handler.clone();
                Rc::new(move |req: HttpRequest, caller: AdminCaller| {
                    let h = handler.clone();
                    engine_jobs::run(move || h(&req, &caller))
                })
            });
            let port_tls = tls.as_ref().map(|t| t.port.clone());
            let port = block_on(move || admin::AdminPort::start(at, make, tls))?;
            if let (Some(certs), Some(slot), Some(p)) = (certs, port.tls(), port_tls) {
                certs.add_shard_port(slot.clone(), p);
            }
            Ok(AdminPort(port))
        }

        /// The address it listens on.
        #[must_use]
        pub fn local_addr(&self) -> SocketAddr {
            self.0.local_addr()
        }

        /// Stops it and waits for its calls.
        pub fn stop(self) {
            let port = self.0;
            let _ = block_on(move || port.stop());
            let _ = block_on(|| engine_jobs::drain(DRAIN));
        }
    }

    /// The HTTP port of the node.
    #[derive(Debug)]
    pub struct HttpPort(http::HttpPort);

    impl HttpPort {
        pub(super) fn start(
            cfg: HttpPortConfig,
            service: Arc<NodeHttp>,
            metrics: Arc<Mutex<HttpMetrics>>,
            certs: Option<&crate::certs::Certificates>,
        ) -> io::Result<HttpPort> {
            let port_tls = cfg.tls.clone();
            let make: MakeService = Arc::new(move || -> Rc<dyn http::HttpService> {
                Rc::new(Bridged::new(service.clone()))
            });
            let port = block_on(move || http::HttpPort::start(cfg, make, metrics))?;
            if let (Some(certs), Some(p)) = (certs, port_tls) {
                certs.add_shard_port(port.tls().clone(), p);
            }
            Ok(HttpPort(port))
        }

        /// The addresses it listens on.
        #[must_use]
        pub fn addrs(&self) -> &[SocketAddr] {
            self.0.addrs()
        }

        /// Stops it and waits for its calls.
        pub fn stop(self) {
            let port = self.0;
            let _ = block_on(move || port.stop());
            let _ = block_on(|| engine_jobs::drain(DRAIN));
        }
    }

    /// The node's service on a shard: its calls run in engine threads of the shard.
    struct Bridged {
        service: Arc<NodeHttp>,
        /// The options last read, and when.
        options: Rc<RefCell<(Option<Instant>, HttpOptions)>>,
        /// A read of the options is running.
        reading: Rc<RefCell<bool>>,
    }

    impl Bridged {
        fn new(service: Arc<NodeHttp>) -> Bridged {
            Bridged {
                service,
                options: Rc::new(RefCell::new((None, http_options(None)))),
                reading: Rc::new(RefCell::new(false)),
            }
        }
    }

    impl http::HttpService for Bridged {
        fn serve(&self, request: Request, caller: Caller, cancel: Cancel) -> Future<Reply> {
            let (service, flag) = (self.service.clone(), cancel.flag());
            engine_jobs::run(move || service.serve(&request, &caller, &flag)).then(
                move |reply| -> Result<Reply, teseladb::engine::core::future::ExceptionPtr> {
                    Ok(match reply {
                        threaded::Reply::Whole(r) => Reply::Whole(r),
                        threaded::Reply::Stream { status, headers, pieces } => {
                            Reply::Stream { status, headers, pieces: bridged_pieces(pieces) }
                        }
                    })
                },
            )
        }

        /// The options this shard read last; read again in an engine thread, in the background,
        /// once they are older than a second (the service reads `httpOptions` from Group 0).
        fn options(&self) -> HttpOptions {
            let (at, current) = self.options.borrow().clone();
            let stale = at.is_none_or(|t| t.elapsed() >= OPTIONS_TTL);
            if stale && !self.reading.replace(true) {
                let (service, slot, reading) =
                    (self.service.clone(), self.options.clone(), self.reading.clone());
                let read = engine_jobs::run(move || service.options());
                run_in_background(read.then_wrapped(move |f: Future<HttpOptions>| {
                    if let Ok(o) = f.get() {
                        *slot.borrow_mut() = (Some(Instant::now()), o);
                    }
                    *reading.borrow_mut() = false;
                    make_ready_future(())
                }));
            }
            current
        }

        fn route(&self, request: &Request) -> String {
            self.service.route(request)
        }
    }

    /// The pieces of a stream of the node's service, each read in an engine thread of the shard.
    /// The port asks for a piece once the one before it was sent, so no two reads of the stream
    /// run at once.
    fn bridged_pieces(pieces: threaded::Pieces) -> http::Pieces {
        let pieces = Rc::new(RefCell::new(pieces));
        Box::new(move |cancel: &Cancel| {
            let (pieces, flag) = (pieces.clone(), cancel.flag());
            engine_jobs::run(move || {
                // Taken out while it reads: nothing else holds the stream across a wait.
                let Ok(mut p) = pieces.try_borrow_mut() else { return None };
                p(&flag)
            })
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_port_that_cannot_open_is_named_in_its_error() {
        let addr: SocketAddr = "127.0.0.1:27018".parse().expect("an address");
        let busy = io::Error::from(io::ErrorKind::AddrInUse);
        let e = port_error(PEERS, addr, &busy);
        assert_eq!(e.kind(), io::ErrorKind::AddrInUse);
        assert!(e.to_string().starts_with("peer port 127.0.0.1:27018: "), "{e}");
        let e = port_error(HTTP, addr, &io::Error::other("no such address"));
        assert_eq!(e.kind(), io::ErrorKind::Other);
        assert_eq!(e.to_string(), "HTTP port 127.0.0.1:27018: no such address");
        assert!(port_error(ADMIN, addr, &busy).to_string().starts_with("administration port "));
    }
}
