//! Transports for the HTTP client.
//!
//! The client asks a [`ConnectionFactory`](experimental::ConnectionFactory) for a new
//! connected socket whenever it needs another connection to the server.
//! [`BasicConnectionFactory`](experimental::BasicConnectionFactory) opens plain TCP
//! connections to one address and
//! [`TlsConnectionFactory`](experimental::TlsConnectionFactory) opens TLS connections with
//! the given credentials and server name.

/// The connection factories of the HTTP client (interface still subject to change).
pub mod experimental {
    use crate::engine::core::abort_source::AbortSource;
    use crate::engine::core::future::Future;
    use crate::engine::core::shared_ptr::SharedPtr;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::net::api::{ConnectedSocket, Transport};
    use crate::engine::net::socket_defs::SocketAddress;
    use crate::engine::net::tls::{CertificateCredentials, TlsOptions};

    /// A factory of transport sockets for the HTTP client.
    ///
    /// It lets callers provide their own transport: the client calls the factory whenever
    /// it needs another connection to the server.
    pub trait ConnectionFactory {
        /// Returns a ready-to-use [`ConnectedSocket`], which the client uses as the
        /// transport of one HTTP connection.
        ///
        /// `as_` is an abort source that can abort the connection attempt, or null.
        fn make(&mut self, as_: *mut AbortSource) -> Future<ConnectedSocket>;
    }

    /// A factory of plain tcp connections to one address.
    pub struct BasicConnectionFactory {
        _addr: SocketAddress,
    }

    impl BasicConnectionFactory {
        /// Connections to `addr`.
        pub fn new(addr: SocketAddress) -> Self {
            BasicConnectionFactory { _addr: addr }
        }
    }

    impl ConnectionFactory for BasicConnectionFactory {
        fn make(&mut self, _as: *mut AbortSource) -> Future<ConnectedSocket> {
            crate::engine::core::api::connect_from(
                self._addr,
                SocketAddress::default(),
                Transport::TCP,
            )
        }
    }

    /// A factory of tls connections to one address.
    pub struct TlsConnectionFactory {
        _addr: SocketAddress,
        _creds: SharedPtr<CertificateCredentials>,
        _host: Sstring,
    }

    impl TlsConnectionFactory {
        /// Tls connections to `addr` with `creds`, naming `host` to the server.
        pub fn new(
            addr: SocketAddress,
            creds: SharedPtr<CertificateCredentials>,
            host: Sstring,
        ) -> Self {
            TlsConnectionFactory { _addr: addr, _creds: creds, _host: host }
        }
    }

    impl ConnectionFactory for TlsConnectionFactory {
        fn make(&mut self, _as: *mut AbortSource) -> Future<ConnectedSocket> {
            crate::engine::net::tls::connect(
                self._creds.clone(),
                self._addr,
                TlsOptions { server_name: self._host.clone(), ..TlsOptions::default() },
            )
        }
    }
}
