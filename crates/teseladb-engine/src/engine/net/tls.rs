//! TLS over connected sockets, implemented with GnuTLS.
//!
//! [`CertificateCredentials`] and [`ServerCredentials`] hold the certificates, keys, trust
//! anchors and Diffie-Hellman parameters ([`DhParams`], [`X509Cert`]); a
//! [`CredentialsBuilder`] collects them and builds credentials, optionally reloadable: the
//! credentials then watch their files and rebuild themselves when they change. A session
//! wraps a connected socket of any stack and runs the handshake, encryption and peer
//! verification on its shard; [`connect`], [`wrap_client`], [`wrap_server`] and [`listen`]
//! give TLS sockets and listeners. Server credentials can be shared with every shard
//! through [`ServerCredentials::share_with_shards`].
//!
//! GnuTLS is declared with `extern "C"` and requires version 3.6 or later; the feature
//! `gnutls_legacy` selects the code paths for versions before 3.5.6. Linux only
//! (`st_mtim`, errno values).

use std::any::Any;
use std::cell::UnsafeCell;
use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::fmt;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use crate::engine::core::do_with::do_with;
use crate::engine::core::file::{File, OpenFlags, open_file_dma};
use crate::engine::core::future::InvalidArgument;
use crate::engine::core::future::{
    ExceptionPtr, Future, LogicError, SystemError, make_exception_future, make_ready_future,
};
use crate::engine::core::semaphore::{Semaphore, with_semaphore};
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::{
    EnableSharedFromThis, SharedFromThis, SharedPtr, make_shared, make_shared_esft,
};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::core::thread::async_;
use crate::engine::net::api::{ConnectedSocket, ListenOptions, ServerSocket, SessionDn, Socket};
use crate::engine::net::inet_address::{InetAddress, RuntimeError};
use crate::engine::net::socket_defs::SocketAddress;
use crate::engine::net::stack::ConnectedSocketImpl;
use crate::engine::util::noncopyable_function::NoncopyableFunction;

// ---------------------------------------------------------------------------
// GnuTLS declarations
// ---------------------------------------------------------------------------

/// `gnutls_datum_t`
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct GnutlsDatumT {
    /// The bytes.
    pub data: *mut u8,
    /// Their number.
    pub size: c_uint,
}

#[allow(non_camel_case_types)]
type gnutls_session_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_certificate_credentials_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_dh_params_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_priority_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_x509_crt_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_transport_ptr_t = *mut c_void;
#[allow(non_camel_case_types)]
type gnutls_sec_param_t = c_int;

const GNUTLS_E_SUCCESS: c_int = 0;
const GNUTLS_E_AGAIN: c_int = -28;
const GNUTLS_E_INTERRUPTED: c_int = -52;
const GNUTLS_E_SHORT_MEMORY_BUFFER: c_int = -51;
const GNUTLS_E_REHANDSHAKE: c_int = -37;
const GNUTLS_E_PREMATURE_TERMINATION: c_int = -110;
const GNUTLS_E_NO_CERTIFICATE_FOUND: c_int = -49;
const GNUTLS_E_CERTIFICATE_ERROR: c_int = -43;
const GNUTLS_E_INVALID_SESSION: c_int = -10;
const GNUTLS_E_REQUESTED_DATA_NOT_AVAILABLE: c_int = -56;

const GNUTLS_SEC_PARAM_LEGACY: gnutls_sec_param_t = 30;
const GNUTLS_SEC_PARAM_MEDIUM: gnutls_sec_param_t = 35;
const GNUTLS_SEC_PARAM_HIGH: gnutls_sec_param_t = 40;
const GNUTLS_SEC_PARAM_ULTRA: gnutls_sec_param_t = 45;
#[cfg(feature = "gnutls_legacy")]
const GNUTLS_PK_DH: c_int = 3;

/// `gnutls_alloc_function`
#[allow(non_camel_case_types)]
type gnutls_alloc_function = unsafe extern "C" fn(size: usize) -> *mut c_void;
/// `gnutls_free_function`
#[allow(non_camel_case_types)]
type gnutls_free_function = unsafe extern "C" fn(ptr: *mut c_void);

/// The opaque `gnutls_hash_hd_t` (used by the ISN generator of `net/tcp.rs`).
pub(crate) type GnutlsHashHdT = *mut c_void;
/// `GNUTLS_DIG_MD5` (`= GNUTLS_MAC_MD5`)
pub(crate) const GNUTLS_DIG_MD5: c_int = 2;

// Every gnutls function of the crate is declared here, in the two blocks of this file, with
// the library named once per block.
#[link(name = "gnutls")]
unsafe extern "C" {
    // `gnutls_malloc` and `gnutls_free` are not functions: gnutls.h declares them as
    // `extern gnutls_alloc_function gnutls_malloc; extern gnutls_free_function gnutls_free;`,
    // global variables holding the allocator in use. They are read and called through.
    static gnutls_malloc: gnutls_alloc_function;
    static gnutls_free: gnutls_free_function;

    pub(crate) fn gnutls_hash_init(dig: *mut GnutlsHashHdT, algorithm: c_int) -> c_int;
    pub(crate) fn gnutls_hash(handle: GnutlsHashHdT, text: *const c_void, textlen: usize) -> c_int;
    pub(crate) fn gnutls_hash_deinit(handle: GnutlsHashHdT, digest: *mut c_void);
    pub(crate) fn gnutls_hash_get_len(algorithm: c_int) -> c_uint;

    fn gnutls_global_init() -> c_int;
    fn gnutls_global_deinit();
    fn gnutls_strerror(error: c_int) -> *const c_char;
    fn gnutls_dh_params_init(dh_params: *mut gnutls_dh_params_t) -> c_int;
    fn gnutls_dh_params_deinit(dh_params: gnutls_dh_params_t);
    #[cfg(feature = "gnutls_legacy")]
    fn gnutls_sec_param_to_pk_bits(algo: c_int, param: gnutls_sec_param_t) -> c_uint;
    #[cfg(feature = "gnutls_legacy")]
    fn gnutls_dh_params_generate2(params: gnutls_dh_params_t, bits: c_uint) -> c_int;
    fn gnutls_dh_params_import_pkcs3(
        params: gnutls_dh_params_t,
        pkcs3_params: *const GnutlsDatumT,
        format: c_int,
    ) -> c_int;
    fn gnutls_dh_params_cpy(dst: gnutls_dh_params_t, src: gnutls_dh_params_t) -> c_int;
    fn gnutls_x509_crt_init(cert: *mut gnutls_x509_crt_t) -> c_int;
    fn gnutls_x509_crt_deinit(cert: gnutls_x509_crt_t);
    fn gnutls_x509_crt_import(
        cert: gnutls_x509_crt_t,
        data: *const GnutlsDatumT,
        format: c_int,
    ) -> c_int;
    fn gnutls_memset(data: *mut c_void, c: c_int, size: usize);
    fn gnutls_certificate_allocate_credentials(res: *mut gnutls_certificate_credentials_t)
    -> c_int;
    fn gnutls_certificate_free_credentials(sc: gnutls_certificate_credentials_t);
    fn gnutls_certificate_set_x509_trust_mem(
        res: gnutls_certificate_credentials_t,
        ca: *const GnutlsDatumT,
        type_: c_int,
    ) -> c_int;
    fn gnutls_certificate_set_x509_crl_mem(
        res: gnutls_certificate_credentials_t,
        crl: *const GnutlsDatumT,
        type_: c_int,
    ) -> c_int;
    fn gnutls_certificate_set_x509_key_mem(
        res: gnutls_certificate_credentials_t,
        cert: *const GnutlsDatumT,
        key: *const GnutlsDatumT,
        type_: c_int,
    ) -> c_int;
    fn gnutls_certificate_set_x509_simple_pkcs12_mem(
        res: gnutls_certificate_credentials_t,
        p12blob: *const GnutlsDatumT,
        type_: c_int,
        password: *const c_char,
    ) -> c_int;
    #[cfg(not(feature = "gnutls_legacy"))]
    fn gnutls_certificate_set_known_dh_params(
        res: gnutls_certificate_credentials_t,
        sec_param: gnutls_sec_param_t,
    ) -> c_int;
    fn gnutls_certificate_set_dh_params(
        res: gnutls_certificate_credentials_t,
        dh_params: gnutls_dh_params_t,
    );
    fn gnutls_certificate_set_x509_system_trust(cred: gnutls_certificate_credentials_t) -> c_int;
    fn gnutls_session_ticket_key_generate(key: *mut GnutlsDatumT) -> c_int;
    fn gnutls_priority_init(
        priority_cache: *mut gnutls_priority_t,
        priorities: *const c_char,
        err_pos: *mut *const c_char,
    ) -> c_int;
    fn gnutls_priority_deinit(priority_cache: gnutls_priority_t);
}

// ---------------------------------------------------------------------------
// Public types
//
// A relatively thin TLS wrapper for socket I/O. The underlying mechanism is GnuTLS, but the
// public interfaces do not expose it, so it could be replaced by another TLS library.
// ---------------------------------------------------------------------------

/// The encoding of certificates and keys.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum X509CrtFormat {
    /// Binary DER.
    DER = 0,
    /// Base64 PEM.
    PEM = 1,
}

/// The bytes of a certificate, key or parameter file, borrowed.
pub type Blob<'a> = &'a [u8];

/// Key strength of `dh_params`.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DhLevel {
    /// Legacy strength.
    #[default]
    LEGACY = 2,
    /// Medium strength.
    MEDIUM = 3,
    /// High strength.
    HIGH = 4,
    /// Ultra strength.
    ULTRA = 5,
}

/// Diffie-Hellman parameters for wire encryption.
pub struct DhParams {
    pub(crate) _impl: Box<DhParamsImpl>,
}

/// An X.509 certificate.
pub struct X509Cert {
    _impl: SharedPtr<X509CertImpl>,
}

/// What every credentials holder can be given: trust anchors, revocation lists, a key pair
/// and DH parameters, as blobs or as files read asynchronously.
pub trait AbstractCredentials {
    /// Sets the trusted CAs.
    fn set_x509_trust(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr>;
    /// Sets the certificate revocation list.
    fn set_x509_crl(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr>;
    /// Sets our certificate and key.
    fn set_x509_key(
        &mut self,
        cert: Blob<'_>,
        key: Blob<'_>,
        fmt: X509CrtFormat,
    ) -> Result<(), ExceptionPtr>;

    /// Sets our certificate and key from a PKCS#12 bundle.
    fn set_simple_pkcs12(
        &mut self,
        b: Blob<'_>,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Result<(), ExceptionPtr>;

    /// Loads the trusted CAs from a file.
    fn set_x509_trust_file(&mut self, cafile: &Sstring, fmt: X509CrtFormat) -> Future<()>
    where
        Self: Sized + 'static,
    {
        abstract_set_x509_trust_file(self, cafile, fmt)
    }
    /// Loads the certificate revocation list from a file.
    fn set_x509_crl_file(&mut self, crlfile: &Sstring, fmt: X509CrtFormat) -> Future<()>
    where
        Self: Sized + 'static,
    {
        abstract_set_x509_crl_file(self, crlfile, fmt)
    }
    /// Loads our certificate and key from files.
    fn set_x509_key_file(&mut self, cf: &Sstring, kf: &Sstring, fmt: X509CrtFormat) -> Future<()>
    where
        Self: Sized + 'static,
    {
        abstract_set_x509_key_file(self, cf, kf, fmt)
    }

    /// Loads our certificate and key from a PKCS#12 file.
    fn set_simple_pkcs12_file(
        &mut self,
        pkcs12file: &Sstring,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Future<()>
    where
        Self: Sized + 'static,
    {
        abstract_set_simple_pkcs12_file(self, pkcs12file, fmt, password)
    }
}

/// Our own role in a TLS session, independent of GnuTLS. Its encoding differs from the
/// internal session type, which carries the GnuTLS values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionType {
    /// We are the client.
    CLIENT,
    /// We are the server.
    SERVER,
}

/// A callback that receives the Distinguished Name (DN) information of the peer
/// certificate: our own role in the handshake (client or server), the subject DN and the
/// issuer DN.
///
/// An error returned by the callback fails the peer verification, and with it the
/// handshake. A callback that cannot fail may return `()`.
pub struct DnCallback(NoncopyableFunction<DnCallbackSignature>);

/// The signature a [`DnCallback`] stores: role, subject and issuer, returning the
/// verification result.
type DnCallbackSignature = fn(SessionType, Sstring, Sstring) -> Result<(), ExceptionPtr>;

/// What a [`DnCallback`] may return: `()` (it never fails) or `Result<(), ExceptionPtr>`
/// (an error fails the verification).
pub trait DnCallbackResult {
    /// The result as the verification sees it.
    fn into_result(self) -> Result<(), ExceptionPtr>;
}

impl DnCallbackResult for () {
    #[inline]
    fn into_result(self) -> Result<(), ExceptionPtr> {
        Ok(())
    }
}

impl DnCallbackResult for Result<(), ExceptionPtr> {
    #[inline]
    fn into_result(self) -> Result<(), ExceptionPtr> {
        self
    }
}

impl DnCallback {
    /// Wraps `func` (a small closure is stored inline, without allocating).
    pub fn new<F, R>(mut func: F) -> Self
    where
        F: FnMut(SessionType, Sstring, Sstring) -> R + 'static,
        R: DnCallbackResult,
    {
        DnCallback(NoncopyableFunction::<
            fn(SessionType, Sstring, Sstring) -> Result<(), ExceptionPtr>,
        >::new(move |t: SessionType, subject: Sstring, issuer: Sstring| {
            func(t, subject, issuer).into_result()
        }))
    }

    /// Calls the callback.
    ///
    /// # Errors
    ///
    /// The callback's error, which fails the peer verification.
    #[inline]
    pub fn call(
        &mut self,
        t: SessionType,
        subject: Sstring,
        issuer: Sstring,
    ) -> Result<(), ExceptionPtr> {
        self.0.call(t, subject, issuer)
    }
}

/// Certificates and keys used by TLS sessions.
///
/// Credentials are typically shared by many client or server sessions, and a change to
/// them affects every session created with them, so set them up once, before starting
/// connections.
pub struct CertificateCredentials {
    pub(crate) _impl: SharedPtr<CertificateCredentialsImpl>,
    /// The reloading state of reloadable credentials: set only on credentials built by
    /// `build_reloadable_*`, whose file watches it owns.
    pub(crate) _reloadable: Option<Box<ReloadableCredentialsBase>>,
}

/// The error raised when certificate validation fails; holds the reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerificationError(pub String);

impl fmt::Display for VerificationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for VerificationError {}

/// Whether the server asks for client certificates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ClientAuth {
    /// No client certificate.
    #[default]
    NONE,
    /// Ask for one.
    REQUEST,
    /// Require one.
    REQUIRE,
}

/// Session resumption mode.
///
/// Only TLS 1.3 session tickets are supported.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
// The variants keep the lowercase names of the configuration values.
#[allow(non_camel_case_types)]
pub enum SessionResumeMode {
    /// No resumption.
    #[default]
    NONE,
    /// TLS 1.3 session tickets.
    TLS13_SESSION_TICKET,
}

/// Certificate credentials extended with the settings only a server uses (client
/// authentication, session resumption) and with sharing across shards.
pub struct ServerCredentials {
    /// The certificate credentials this extends.
    pub base: CertificateCredentials,
    /// The copies of these credentials on the shards of a server, which a reload of these
    /// updates; set by [`ServerCredentials::share_with_shards`] (and on the copies).
    _shards: Option<Arc<CredentialsShards>>,
    /// Whether this object is a shard copy, registered in its shard's slot of `_shards`.
    _shard_copy: bool,
}

/// The settings of server credentials that a shard copy takes from the owner, with the
/// shared gnutls objects. `Send`, unlike the shard-local credentials.
#[derive(Clone)]
pub struct SharedCredentialsSettings {
    g: Arc<GnutlsCredentialsState>,
    client_auth: ClientAuth,
    session_resume_mode: SessionResumeMode,
    enable_certificate_verification: bool,
}

impl SharedCredentialsSettings {
    /// The settings of `impl_`, if they can go to other shards: not with a DN verification
    /// callback (a function of its shard) nor with system trust still to load (the load
    /// changes the gnutls credentials, which shared ones must not).
    fn of(impl_: &CertificateCredentialsImpl) -> Result<Self, ExceptionPtr> {
        if impl_._dn_callback.is_some() {
            return Err(ExceptionPtr::from(InvalidArgument(
                "tls credentials with a DN verification callback cannot be shared between shards"
                    .to_string(),
            )));
        }
        if impl_._load_system_trust {
            return Err(ExceptionPtr::from(InvalidArgument(
                "tls credentials must load the system trust before they are shared between shards"
                    .to_string(),
            )));
        }
        Ok(SharedCredentialsSettings {
            g: impl_._g.clone(),
            client_auth: impl_._client_auth,
            session_resume_mode: impl_._session_resume_mode,
            enable_certificate_verification: impl_._enable_certificate_verification,
        })
    }
}

/// Per shard, the shard copies of one set of server credentials (see
/// [`ServerCredentials::share_with_shards`]).
pub struct CredentialsShards {
    _slots: Box<[UnsafeCell<Vec<*mut ServerCredentials>>]>,
}

// SAFETY: each slot is read and written only on its own shard: by `SharedServerCredentials::
// into_local` and the `Drop` of a copy, which run there, and by the reload work sent there.
unsafe impl Send for CredentialsShards {}
// SAFETY: as above.
unsafe impl Sync for CredentialsShards {}

impl CredentialsShards {
    /// This shard's slot.
    ///
    /// # Safety
    /// No other reference to the slot may be in use (each is used for a single call).
    #[allow(clippy::mut_from_ref)]
    unsafe fn local(&self) -> &mut Vec<*mut ServerCredentials> {
        // SAFETY: only this shard touches its slot (see the Sync impl); guaranteed by the caller.
        unsafe { &mut *self._slots[this_shard_id() as usize].get() }
    }

    /// Gives the copies on this shard the gnutls objects and settings of a reload.
    fn update_local(&self, s: &SharedCredentialsSettings) {
        // SAFETY: the slot is used for this loop only; nothing in it reaches the slot again.
        for &p in unsafe { self.local() }.iter() {
            // SAFETY: a registered copy is alive (its Drop unregisters it) and lives in its
            // shared pointer node; no reference to it is in use while this reactor task runs.
            unsafe { (*p).base._impl = make_shared(CertificateCredentialsImpl::for_shard(s)) };
        }
    }
}

impl Drop for ServerCredentials {
    fn drop(&mut self) {
        if !self._shard_copy {
            return;
        }
        if let Some(shards) = &self._shards {
            let me: *const ServerCredentials = &raw const *self;
            // SAFETY: a copy is dropped on its shard; the slot is used for this call only.
            let slot = unsafe { shards.local() };
            if let Some(i) = slot.iter().position(|&p| std::ptr::addr_eq(p, me)) {
                slot.swap_remove(i);
            }
        }
    }
}

/// Server credentials on their way to another shard (`Send`): the shared gnutls objects and
/// settings, and the registry of shard copies. Made by
/// [`ServerCredentials::share_with_shards`]; each clone costs one atomic increment per `Arc`.
#[derive(Clone)]
pub struct SharedServerCredentials {
    _settings: SharedCredentialsSettings,
    _shards: Arc<CredentialsShards>,
}

impl SharedServerCredentials {
    /// This shard's credentials over the shared gnutls objects: a shard-local `SharedPtr`,
    /// whose copies per session are not atomic, registered so that reloads reach it.
    pub fn into_local(self) -> SharedPtr<ServerCredentials> {
        let sp = make_shared(ServerCredentials {
            base: CertificateCredentials {
                _impl: make_shared(CertificateCredentialsImpl::for_shard(&self._settings)),
                _reloadable: None,
            },
            _shards: Some(self._shards.clone()),
            _shard_copy: true,
        });
        if let Some(p) = sp.get() {
            // SAFETY: the slot is used for this call only.
            unsafe { self._shards.local() }.push(p.as_ptr());
        }
        sp
    }
}

impl std::ops::Deref for ServerCredentials {
    type Target = CertificateCredentials;
    fn deref(&self) -> &CertificateCredentials {
        &self.base
    }
}

impl std::ops::DerefMut for ServerCredentials {
    fn deref_mut(&mut self) -> &mut CertificateCredentials {
        &mut self.base
    }
}

/// Called after reloadable credentials were rebuilt (or failed to be): the files that
/// changed and the error, if any. `None` means no callback.
pub type ReloadCallback = Option<std::rc::Rc<dyn Fn(&HashSet<Sstring>, Option<ExceptionPtr>)>>;
/// As [`ReloadCallback`], also given the builder that was used, and returning a future the
/// reload waits for.
pub type ReloadCallbackEx = Option<
    std::rc::Rc<dyn Fn(&CredentialsBuilder, &HashSet<Sstring>, Option<ExceptionPtr>) -> Future<()>>,
>;

/// A plain, cloneable container of certificate credential options.
///
/// It holds the raw blobs and settings rather than live credentials, so it can be cloned
/// to every shard and used there to build [`CertificateCredentials`] or
/// [`ServerCredentials`], for example when the TLS objects are initialised.
///
/// Invalid input (a malformed certificate, for instance) is not detected when it is
/// added: the error surfaces at the earliest when one of the build functions runs.
#[derive(Clone, Default)]
pub struct CredentialsBuilder {
    pub(crate) _blobs: BTreeMap<Sstring, Vec<BlobValue>>,
    pub(crate) _client_auth: ClientAuth,
    pub(crate) _session_resume_mode: SessionResumeMode,
    pub(crate) _priority: Sstring,
    pub(crate) _session_resume_key: Vec<u8>,
}

/// A value the builder holds under a key (several values per key are allowed).
#[derive(Clone, Debug)]
pub enum BlobValue {
    /// A certificate or CRL blob.
    X509Simple(X509Simple),
    /// A certificate and key pair.
    X509Key(X509Key),
    /// A PKCS#12 bundle.
    Pkcs12Simple(Pkcs12Simple),
    /// A DH parameter strength.
    DhLevel(DhLevel),
    /// `bool` (system trust)
    Bool(bool),
}

/// The opaque resumption data of a session.
pub type SessionData = Vec<u8>;

/// TLS configuration options
#[derive(Clone, Debug)]
pub struct TlsOptions {
    /// Whether to wait for EOF from the server when the session shuts down.
    pub wait_for_eof_on_shutdown: bool,
    /// The server name sent in the SNI TLS extension.
    pub server_name: Sstring,

    /// Whether the server certificate is verified. May be set to `false` in test
    /// environments.
    pub verify_certificate: bool,

    /// Optional session resume data, as returned by [`get_session_resume_data`] on an
    /// earlier connection; empty for a new session.
    pub session_resume_data: SessionData,
}

impl Default for TlsOptions {
    fn default() -> Self {
        TlsOptions {
            wait_for_eof_on_shutdown: true,
            server_name: Sstring::default(),
            verify_certificate: true,
            session_resume_data: SessionData::new(),
        }
    }
}

/// The types of subject alternative name entries in an X.509 certificate.
#[repr(i32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
// The variants keep the lowercase names of the X.509 general name types.
#[allow(non_camel_case_types)]
pub enum SubjectAltNameType {
    /// string value representing a 'DNS' entry
    dnsname = 1,
    /// string value representing an 'email' entry
    rfc822name,
    /// string value representing an 'uri' entry
    uri,
    /// inet_address value representing an 'IP' entry
    ipaddress,
    /// string value
    othername,
    /// string value
    dn,
}

/// The value of a subject alternative name: a string or an IP address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubjectAltNameValue {
    /// A string entry.
    String(Sstring),
    /// An address entry.
    Address(InetAddress),
}

/// Subject alt name entry
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubjectAltName {
    /// The kind of entry.
    pub type_: SubjectAltNameType,
    /// Its value.
    pub value: SubjectAltNameValue,
}

/// The DER bytes of a certificate.
pub type CertificateData = Vec<u8>;

impl fmt::Display for SubjectAltNameValue {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubjectAltNameValue::String(s) => write!(os, "{}", s),
            SubjectAltNameValue::Address(a) => write!(os, "{}", a),
        }
    }
}

impl fmt::Display for SubjectAltName {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}={}", self.type_, self.value)
    }
}

/// Returns the display name of a subject alternative name type.
///
/// Tools name these types inconsistently; this function follows the names most
/// certificate tools use rather than GnuTLS's:
///
/// - `dnsname`: `"DNS"`
/// - `rfc822name`: `"EMAIL"`
/// - `uri`: `"URI"`
/// - `ipaddress`: `"IP"`
/// - `othername`: `"OTHERNAME"`
/// - `dn`: `"DIRNAME"`
pub fn format_as(type_: SubjectAltNameType) -> &'static str {
    match type_ {
        SubjectAltNameType::dnsname => "DNS",
        SubjectAltNameType::rfc822name => "EMAIL",
        SubjectAltNameType::uri => "URI",
        SubjectAltNameType::ipaddress => "IP",
        SubjectAltNameType::othername => "OTHERNAME",
        SubjectAltNameType::dn => "DIRNAME",
    }
}

impl fmt::Display for SubjectAltNameType {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        os.write_str(format_as(*self))
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// Access to the implementation of a [`ConnectedSocket`].
pub struct GetImpl;

impl GetImpl {
    /// Takes the implementation out of `s`.
    pub fn get(s: ConnectedSocket) -> Option<Box<dyn ConnectedSocketImpl>> {
        let mut s = s;
        s._csi.take()
    }

    /// The implementation of `s`, if any.
    pub fn maybe_get_ptr(s: &mut ConnectedSocket) -> Option<*mut dyn ConnectedSocketImpl> {
        s._csi.as_deref_mut().map(|p| p as *mut dyn ConnectedSocketImpl)
    }
}

/// A `gnutls_datum_t` that borrows `input` (GnuTLS only reads it).
#[inline]
fn blob_wrapper(input: Blob<'_>) -> GnutlsDatumT {
    GnutlsDatumT { data: input.as_ptr() as *mut u8, size: input.len() as c_uint }
}

/// `gnutlsinit`: global init, never torn down before process exit.
struct GnutlsInit;

impl GnutlsInit {
    fn new() -> Self {
        // SAFETY: plain library initialization.
        unsafe { gnutls_global_init() };
        GnutlsInit
    }
}

impl Drop for GnutlsInit {
    fn drop(&mut self) {
        // SAFETY: paired with gnutls_global_init.
        unsafe { gnutls_global_deinit() };
    }
}

// Helper to ensure gnutls legacy init
// is handled properly with regards to
// object life spans. Could be better,
// this version will not destroy the
// gnutls stack until process exit.
/// Initializes GnuTLS once per process.
#[inline]
fn gnutlsobj() {
    static INIT: OnceLock<GnutlsInit> = OnceLock::new();
    INIT.get_or_init(GnutlsInit::new);
}

// Helper
/// A file read for the credentials, with its modification time.
#[derive(Clone, Debug)]
pub struct FileInfo {
    /// The file name.
    pub filename: Sstring,
    /// When it was last modified.
    pub modified: SystemTime,
}

/// The contents of a file read for the credentials.
pub struct FileResult {
    /// The contents.
    pub buf: TemporaryBuffer<u8>,
    /// The file.
    pub file: FileInfo,
}

fn read_fully(name: &Sstring, what: &Sstring) -> Future<FileResult> {
    let name1 = name.clone();
    let name2 = name.clone();
    let what = what.clone();
    open_file_dma(name.as_str(), OpenFlags::RO)
        .then(move |f: File| {
            do_with(f, move |f: &mut File| {
                let fp: *mut File = f;
                f.stat()
                    .then(move |s| {
                        // SAFETY: do_with keeps the file alive until the chain completes.
                        let f = unsafe { &mut *fp };
                        let size = s.st_size as usize;
                        let modified = SystemTime::UNIX_EPOCH
                            + Duration::from_secs(s.st_mtim.tv_sec as u64)
                            + Duration::from_nanos(s.st_mtim.tv_nsec as u64);
                        // SAFETY: no io intent (a null pointer).
                        unsafe {
                            f.dma_read_bulk::<u8>(
                                0,
                                size,
                                std::ptr::null_mut(), /* no io intent */
                            )
                        }
                        .then(move |buf: TemporaryBuffer<u8>| {
                            make_ready_future(FileResult {
                                buf,
                                file: FileInfo { filename: name1, modified },
                            })
                        })
                    })
                    .finally(move || {
                        // SAFETY: as above.
                        unsafe { (*fp).close() }
                    })
            })
        })
        .handle_exception(move |ep: ExceptionPtr| {
            make_exception_future::<FileResult>(ExceptionPtr::with_nested(
                RuntimeError(format!("Could not read {} {}", what, name2)),
                ep,
            ))
        })
}

// GnuTLS is used through its C interface only, so that the public interface stays
// independent of it.
/// The error category of GnuTLS error codes: their name and messages.
pub struct GnutlsErrorCategory;

impl GnutlsErrorCategory {
    /// "GnuTLS"
    pub fn name(&self) -> &'static str {
        "GnuTLS"
    }
    /// The GnuTLS message of `error` (`gnutls_strerror`).
    pub fn message(&self, error: i32) -> String {
        // SAFETY: gnutls_strerror returns a static C string.
        unsafe { CStr::from_ptr(gnutls_strerror(error)) }.to_string_lossy().into_owned()
    }
}

/// The error category of the [`TlsError`]s this module returns.
pub fn error_category() -> &'static GnutlsErrorCategory {
    static EC: GnutlsErrorCategory = GnutlsErrorCategory;
    &EC
}

/// A failed GnuTLS call: its error code (see [`GnutlsErrorCategory`]) and context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TlsError {
    /// The GnuTLS error code.
    pub code: i32,
    /// The context of the error.
    pub what: String,
}

impl fmt::Display for TlsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.what.is_empty() {
            f.write_str(&error_category().message(self.code))
        } else {
            write!(f, "{}: {}", self.what, error_category().message(self.code))
        }
    }
}

impl std::error::Error for TlsError {}

/// The root pointer of a `shared_ptr`'s object (its node's, with that provenance), which
/// stays valid across later borrows of the object, unlike one taken from a reference.
/// Null for a null pointer.
#[inline]
fn shared_root<T>(p: &SharedPtr<T>) -> *mut T {
    p.get().map_or(std::ptr::null_mut(), |n| n.as_ptr())
}

// Checks a gnutls return value.
// < 0 -> error.
#[inline]
fn gtls_chk(res: c_int) -> Result<(), ExceptionPtr> {
    if res < 0 {
        return Err(ExceptionPtr::from(TlsError { code: res, what: String::new() }));
    }
    Ok(())
}

// helper for gnutls-functions for receiving a string
// arguments
//  func - the gnutls function that is returning a string (e.g. gnutls_x509_crt_get_issuer_dn)
//  args - the arguments to func that come before the char array's ptr and size args
// returns
//  pair<int, string> - [gnutls error code, extracted string],
//                      in case of no errors, the error code is zero
fn get_gtls_string<F: FnMut(*mut c_char, *mut usize) -> c_int>(mut func: F) -> (c_int, Sstring) {
    let mut size: usize = 0;
    let ret = func(std::ptr::null_mut(), &mut size);

    // by construction, we expect the SHORT_MEMORY_BUFFER error code here
    if ret != GNUTLS_E_SHORT_MEMORY_BUFFER {
        return (ret, Sstring::default());
    }
    assert!(size != 0);
    let mut res: Vec<u8> = vec![0; size];
    let ret = func(res.as_mut_ptr() as *mut c_char, &mut size);
    res.truncate(size.min(res.len().saturating_sub(1)));
    (ret, Sstring::from(String::from_utf8_lossy(&res).as_ref()))
}

/// The GnuTLS side of [`DhParams`]: generated or imported parameters, or (GnuTLS 3.6+)
/// just the security level, which GnuTLS turns into well-known parameters.
pub struct DhParamsImpl {
    _params: gnutls_dh_params_t,
    #[cfg(not(feature = "gnutls_legacy"))]
    _sec_param: Option<gnutls_sec_param_t>,
}

impl DhParamsImpl {
    fn to_gnutls_level(l: DhLevel) -> gnutls_sec_param_t {
        match l {
            DhLevel::LEGACY => GNUTLS_SEC_PARAM_LEGACY,
            DhLevel::MEDIUM => GNUTLS_SEC_PARAM_MEDIUM,
            DhLevel::HIGH => GNUTLS_SEC_PARAM_HIGH,
            DhLevel::ULTRA => GNUTLS_SEC_PARAM_ULTRA,
        }
    }

    fn new_dh_params() -> Result<gnutls_dh_params_t, ExceptionPtr> {
        let mut params: gnutls_dh_params_t = std::ptr::null_mut();
        // SAFETY: plain init call.
        gtls_chk(unsafe { gnutls_dh_params_init(&mut params) })?;
        Ok(params)
    }

    fn from_ptr(p: gnutls_dh_params_t) -> Self {
        gnutlsobj();
        DhParamsImpl {
            _params: p,
            #[cfg(not(feature = "gnutls_legacy"))]
            _sec_param: None,
        }
    }

    /// Parameters of strength `lvl` (generated with the legacy feature).
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when GnuTLS fails to allocate or generate them.
    pub fn from_level(lvl: DhLevel) -> Result<Self, ExceptionPtr> {
        gnutlsobj();
        #[cfg(not(feature = "gnutls_legacy"))]
        {
            Ok(DhParamsImpl {
                _params: std::ptr::null_mut(),
                _sec_param: Some(Self::to_gnutls_level(lvl)),
            })
        }
        #[cfg(feature = "gnutls_legacy")]
        {
            // SAFETY: plain calls on a fresh parameter set.
            let bits =
                unsafe { gnutls_sec_param_to_pk_bits(GNUTLS_PK_DH, Self::to_gnutls_level(lvl)) };
            let ptr = Self::new_dh_params()?;
            let p = Self::from_ptr(ptr);
            gtls_chk(unsafe { gnutls_dh_params_generate2(p._params, bits) })?;
            Ok(p)
        }
    }

    /// Parameters imported from PKCS#3 bytes in `fmt`.
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when the bytes are not valid parameters.
    pub fn from_blob(pkcs3: Blob<'_>, fmt: X509CrtFormat) -> Result<Self, ExceptionPtr> {
        let p = Self::from_ptr(Self::new_dh_params()?);
        let w = blob_wrapper(pkcs3);
        // SAFETY: `w` points to `pkcs3`, alive for the call.
        gtls_chk(unsafe { gnutls_dh_params_import_pkcs3(p._params, &w, fmt as c_int) })?;
        Ok(p)
    }

    /// A copy of `v`.
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when GnuTLS fails to copy them.
    pub fn copy_of(v: &DhParamsImpl) -> Result<Self, ExceptionPtr> {
        let p = Self::from_ptr(Self::new_dh_params()?);
        // SAFETY: both parameter sets are valid.
        gtls_chk(unsafe { gnutls_dh_params_cpy(p._params, v._params) })?;
        Ok(p)
    }

    /// The GnuTLS handle (null when only a level is set).
    #[inline]
    fn get(&self) -> gnutls_dh_params_t {
        self._params
    }

    #[cfg(not(feature = "gnutls_legacy"))]
    fn sec_param(&self) -> Option<gnutls_sec_param_t> {
        self._sec_param
    }
}

impl Drop for DhParamsImpl {
    fn drop(&mut self) {
        if !self._params.is_null() {
            // SAFETY: the parameters were made by gnutls_dh_params_init.
            unsafe { gnutls_dh_params_deinit(self._params) };
        }
    }
}

impl DhParams {
    /// Parameters of the given strength.
    pub fn new(lvl: DhLevel) -> Result<Self, ExceptionPtr> {
        Ok(DhParams { _impl: Box::new(DhParamsImpl::from_level(lvl)?) })
    }

    /// loads a key from data
    pub fn from_blob(b: Blob<'_>, fmt: X509CrtFormat) -> Result<Self, ExceptionPtr> {
        Ok(DhParams { _impl: Box::new(DhParamsImpl::from_blob(b, fmt)?) })
    }

    /// Loads Diffie-Hellman parameters from a file in format `fmt`.
    ///
    /// The file's bytes up to the first NUL byte are parsed.
    ///
    /// # Errors
    ///
    /// The returned future fails if the file cannot be read or does not hold valid
    /// parameters.
    pub fn from_file(filename: &Sstring, fmt: X509CrtFormat) -> Future<DhParams> {
        read_fully(filename, &Sstring::from("dh parameters")).then(move |r: FileResult| {
            // The file's bytes up to the first null, as for a C string.
            let bytes = r.buf.as_slice();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            match DhParams::from_blob(&bytes[..end], fmt) {
                Ok(d) => make_ready_future::<DhParams>(d),
                Err(e) => make_exception_future(e),
            }
        })
    }
}

/// The GnuTLS certificate behind an [`X509Cert`].
pub struct X509CertImpl {
    _cert: gnutls_x509_crt_t,
}

impl X509CertImpl {
    fn new() -> Result<Self, ExceptionPtr> {
        gnutlsobj();
        let mut cert: gnutls_x509_crt_t = std::ptr::null_mut();
        // SAFETY: plain init call.
        gtls_chk(unsafe { gnutls_x509_crt_init(&mut cert) })?;
        Ok(X509CertImpl { _cert: cert })
    }

    fn from_blob(b: Blob<'_>, fmt: X509CrtFormat) -> Result<Self, ExceptionPtr> {
        let i = Self::new()?;
        let w = blob_wrapper(b);
        // SAFETY: `w` points to `b`, alive for the call.
        gtls_chk(unsafe { gnutls_x509_crt_import(i._cert, &w, fmt as c_int) })?;
        Ok(i)
    }
}

impl Drop for X509CertImpl {
    fn drop(&mut self) {
        if !self._cert.is_null() {
            // SAFETY: the certificate was made by gnutls_x509_crt_init.
            unsafe { gnutls_x509_crt_deinit(self._cert) };
        }
    }
}

impl X509Cert {
    fn from_impl(impl_: SharedPtr<X509CertImpl>) -> Self {
        X509Cert { _impl: impl_ }
    }

    /// A certificate parsed from `b`.
    pub fn new(b: Blob<'_>, fmt: X509CrtFormat) -> Result<Self, ExceptionPtr> {
        Ok(X509Cert::from_impl(make_shared(X509CertImpl::from_blob(b, fmt)?)))
    }

    /// A certificate read from a file.
    pub fn from_file(filename: &Sstring, fmt: X509CrtFormat) -> Future<X509Cert> {
        read_fully(filename, &Sstring::from("x509 certificate")).then(move |r: FileResult| {
            let bytes = r.buf.as_slice();
            let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
            match X509Cert::new(&bytes[..end], fmt) {
                Ok(c) => make_ready_future::<X509Cert>(c),
                Err(e) => make_exception_future(e),
            }
        })
    }
}

// wrapper for gnutls_datum, with raii free
/// A `gnutls_datum_t` owning its gnutls-allocated bytes, wiped on release.
pub struct GnutlsDatum(pub GnutlsDatumT);

impl GnutlsDatum {
    /// `s` bytes from `gnutls_malloc`.
    pub fn with_size(s: usize) -> Self {
        // SAFETY: `gnutls_malloc` is a global that gnutls initializes statically with its
        // allocator and never leaves null; reading it and calling through it is a plain allocation.
        let data = unsafe { gnutls_malloc(s) } as *mut u8;
        if data.is_null() {
            std::alloc::handle_alloc_error(
                std::alloc::Layout::array::<u8>(s.max(1))
                    .unwrap_or(std::alloc::Layout::new::<u8>()),
            );
        }
        GnutlsDatum(GnutlsDatumT { data, size: s as c_uint })
    }

    /// An empty datum.
    pub fn new() -> Self {
        GnutlsDatum(GnutlsDatumT { data: std::ptr::null_mut(), size: 0 })
    }

    /// `operator=(gnutls_datum&& other)`
    pub fn assign(&mut self, mut other: GnutlsDatum) {
        self.release();
        self.0.data = std::mem::replace(&mut other.0.data, std::ptr::null_mut());
        self.0.size = std::mem::replace(&mut other.0.size, 0);
    }

    fn release(&mut self) {
        if !self.0.data.is_null() {
            // SAFETY: the bytes came from gnutls_malloc (or a gnutls allocator), so the
            // global `gnutls_free` (always set, like `gnutls_malloc`) releases them.
            unsafe {
                gnutls_memset(self.0.data as *mut c_void, 0, self.0.size as usize);
                gnutls_free(self.0.data as *mut c_void);
            }
            self.0.data = std::ptr::null_mut();
            self.0.size = 0;
        }
    }
}

impl Default for GnutlsDatum {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for GnutlsDatum {
    fn drop(&mut self) {
        self.release();
    }
}

/// The GnuTLS objects of [`CertificateCredentialsImpl`]: the credentials, their DH
/// parameters, the priority and the session ticket key. They only change while the
/// credentials are built (or reloaded, which builds new ones); once built GnuTLS reads them
/// without changing them and allows that from any thread. They sit behind an `Arc` so that
/// the shards of a server share them (see [`ServerCredentials::share_with_shards`]).
pub struct GnutlsCredentialsState {
    _creds: gnutls_certificate_credentials_t,
    _dh_params: Option<Box<DhParamsImpl>>,
    _priority: gnutls_priority_t,
    _session_resume_key: GnutlsDatum,
}

// SAFETY: the state owns its gnutls objects (freed once, in Drop); they are mutated only
// through `Arc::get_mut`, that is while no other thread can see them, and gnutls reads
// certificate credentials, priorities and ticket keys from several threads at once.
unsafe impl Send for GnutlsCredentialsState {}
// SAFETY: as above: shared access only reads.
unsafe impl Sync for GnutlsCredentialsState {}

impl Drop for GnutlsCredentialsState {
    fn drop(&mut self) {
        if !self._priority.is_null() {
            // SAFETY: made by gnutls_priority_init.
            unsafe { gnutls_priority_deinit(self._priority) };
        }
        if !self._creds.is_null() {
            // SAFETY: made by gnutls_certificate_allocate_credentials.
            unsafe { gnutls_certificate_free_credentials(self._creds) };
        }
    }
}

/// The state behind [`CertificateCredentials`]: the GnuTLS objects, the client
/// authentication and resumption modes, the pending load of the system trust store (done
/// once, under a semaphore, before the first session), the DN callback and whether peer
/// certificates are verified.
pub struct CertificateCredentialsImpl {
    _g: Arc<GnutlsCredentialsState>,
    _client_auth: ClientAuth,
    _session_resume_mode: SessionResumeMode,
    _load_system_trust: bool,
    _system_trust_sem: Semaphore,
    _dn_callback: Option<DnCallback>,
    _enable_certificate_verification: bool,
}

impl CertificateCredentialsImpl {
    /// Allocates the gnutls credentials.
    pub fn new() -> Self {
        gnutlsobj();
        let mut xcred: gnutls_certificate_credentials_t = std::ptr::null_mut();
        // SAFETY: plain allocation call.
        unsafe { gnutls_certificate_allocate_credentials(&mut xcred) };
        if xcred.is_null() {
            std::alloc::handle_alloc_error(std::alloc::Layout::new::<usize>());
        }
        CertificateCredentialsImpl {
            _g: Arc::new(GnutlsCredentialsState {
                _creds: xcred,
                _dh_params: None,
                _priority: std::ptr::null_mut(),
                _session_resume_key: GnutlsDatum::new(),
            }),
            _client_auth: ClientAuth::NONE,
            _session_resume_mode: SessionResumeMode::NONE,
            _load_system_trust: false,
            _system_trust_sem: Semaphore::new(1),
            _dn_callback: None,
            _enable_certificate_verification: true,
        }
    }

    /// The impl of another shard over the same gnutls objects, with the same settings. The
    /// DN callback is not carried over (it is a function of the owner shard); system trust
    /// must be loaded already (see [`ServerCredentials::share_with_shards`]).
    fn for_shard(s: &SharedCredentialsSettings) -> Self {
        CertificateCredentialsImpl {
            _g: s.g.clone(),
            _client_auth: s.client_auth,
            _session_resume_mode: s.session_resume_mode,
            _load_system_trust: false,
            _system_trust_sem: Semaphore::new(1),
            _dn_callback: None,
            _enable_certificate_verification: s.enable_certificate_verification,
        }
    }

    /// The gnutls objects, for a change: only while no other shard shares them.
    fn g_mut(&mut self) -> Result<&mut GnutlsCredentialsState, ExceptionPtr> {
        Arc::get_mut(&mut self._g).ok_or_else(|| {
            ExceptionPtr::from(LogicError(
                "tls credentials shared between shards cannot be changed".to_string(),
            ))
        })
    }

    /// `operator gnutls_certificate_credentials_t() const`
    #[inline]
    pub fn get(&self) -> gnutls_certificate_credentials_t {
        self._g._creds
    }

    fn set_x509_trust(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        let creds = self.g_mut()?._creds;
        let w = blob_wrapper(b);
        // SAFETY: `w` points to `b`, alive for the call.
        gtls_chk(unsafe { gnutls_certificate_set_x509_trust_mem(creds, &w, fmt as c_int) })
    }
    fn set_x509_crl(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        let creds = self.g_mut()?._creds;
        let w = blob_wrapper(b);
        // SAFETY: as above.
        gtls_chk(unsafe { gnutls_certificate_set_x509_crl_mem(creds, &w, fmt as c_int) })
    }
    fn set_x509_key(
        &mut self,
        cert: Blob<'_>,
        key: Blob<'_>,
        fmt: X509CrtFormat,
    ) -> Result<(), ExceptionPtr> {
        let creds = self.g_mut()?._creds;
        let w1 = blob_wrapper(cert);
        let w2 = blob_wrapper(key);
        // SAFETY: as above.
        gtls_chk(unsafe { gnutls_certificate_set_x509_key_mem(creds, &w1, &w2, fmt as c_int) })
    }
    fn set_simple_pkcs12(
        &mut self,
        b: Blob<'_>,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        let creds = self.g_mut()?._creds;
        let w = blob_wrapper(b);
        // A NUL inside is rejected rather than passed truncated.
        let pw = CString::new(password.as_bytes()).map_err(|_| {
            ExceptionPtr::from(InvalidArgument("pkcs12 password contains a NUL byte".to_string()))
        })?;
        // SAFETY: as above.
        gtls_chk(unsafe {
            gnutls_certificate_set_x509_simple_pkcs12_mem(creds, &w, fmt as c_int, pw.as_ptr())
        })
    }
    fn dh_params(&mut self, dh: &DhParams) -> Result<(), ExceptionPtr> {
        let g = self.g_mut()?;
        #[cfg(not(feature = "gnutls_legacy"))]
        {
            if let Some(sec_param) = dh._impl.sec_param() {
                // SAFETY: the credentials are valid.
                unsafe { gnutls_certificate_set_known_dh_params(g._creds, sec_param) };
                return Ok(());
            }
        }
        let cpy = Box::new(DhParamsImpl::copy_of(&dh._impl)?);
        // SAFETY: the credentials and the copy are valid; the copy is kept alive below.
        unsafe { gnutls_certificate_set_dh_params(g._creds, cpy.get()) };
        g._dh_params = Some(cpy);
        Ok(())
    }
    /// Through the credentials' root pointer, touching only `_g` and `_load_system_trust`:
    /// `maybe_load_system_trust` calls it inside `with_semaphore`, which holds a `&mut` to
    /// `_system_trust_sem`.
    ///
    /// # Safety
    /// `this` is the credentials' root pointer, on their shard, with no reference to `_g` or
    /// `_load_system_trust` live; the credentials outlive the returned future.
    unsafe fn set_system_trust(this: *mut Self) -> Future<()> {
        // `g_mut()` by place: a `&mut Self` would cover the semaphore.
        // SAFETY: guaranteed by the caller; a short borrow of `_g`.
        let creds = match Arc::get_mut(unsafe { &mut (*this)._g }) {
            Some(g) => g._creds,
            None => {
                return make_exception_future(ExceptionPtr::from(LogicError(
                    "tls credentials shared between shards cannot be changed".to_string(),
                )));
            }
        };
        async_(move || {
            // SAFETY: the credentials are valid, and their gnutls objects are not shared with
            // other shards (checked above).
            gtls_chk(unsafe { gnutls_certificate_set_x509_system_trust(creds) })?;
            // SAFETY: the credentials outlive the engine thread doing the load (guaranteed by
            // the caller); a place write, with no reference to the field live.
            unsafe { (*this)._load_system_trust = false }; // should only do once, for whatever reason
            Ok(())
        })
    }
    fn set_client_auth(&mut self, ca: ClientAuth) {
        self._client_auth = ca;
    }
    fn get_client_auth(&self) -> ClientAuth {
        self._client_auth
    }
    fn set_session_resume_mode(
        &mut self,
        m: SessionResumeMode,
        key: &[u8],
    ) -> Result<(), ExceptionPtr> {
        self._session_resume_mode = m;
        // (re-)generate session key
        if m != SessionResumeMode::NONE {
            let resume_key = &mut self.g_mut()?._session_resume_key;
            resume_key.assign(GnutlsDatum::new());
            if key.is_empty() {
                // SAFETY: the datum receives a gnutls-allocated key.
                gtls_chk(unsafe { gnutls_session_ticket_key_generate(&mut resume_key.0) })?;
            } else {
                resume_key.assign(GnutlsDatum::with_size(key.len()));
                // SAFETY: the datum has key.len() bytes.
                unsafe {
                    std::ptr::copy_nonoverlapping(key.as_ptr(), resume_key.0.data, key.len())
                };
            }
        }
        Ok(())
    }
    fn get_session_resume_mode(&self) -> SessionResumeMode {
        self._session_resume_mode
    }
    fn get_session_resume_key(&self) -> *const GnutlsDatumT {
        &self._g._session_resume_key.0
    }
    fn set_priority_string(&mut self, prio: &Sstring) -> Result<(), ExceptionPtr> {
        let g = self.g_mut()?;
        // A NUL inside is rejected rather than passed truncated.
        let cprio = CString::new(prio.as_bytes()).map_err(|_| {
            ExceptionPtr::from(InvalidArgument("priority string contains a NUL byte".to_string()))
        })?;
        let mut err: *const c_char = cprio.as_ptr();
        let mut p: gnutls_priority_t = std::ptr::null_mut();
        // SAFETY: `cprio` is a valid C string; `err` points into it.
        let r = gtls_chk(unsafe { gnutls_priority_init(&mut p, cprio.as_ptr(), &mut err) });
        match r {
            Ok(()) => {
                if !g._priority.is_null() {
                    // SAFETY: the previous priority was made by gnutls_priority_init.
                    unsafe { gnutls_priority_deinit(g._priority) };
                }
                g._priority = p;
                Ok(())
            }
            Err(e) => {
                // SAFETY: `err` points into `cprio`.
                let errs = unsafe { CStr::from_ptr(err) }.to_string_lossy().into_owned();
                Err(ExceptionPtr::with_nested(
                    InvalidArgument(format!("Could not set priority: {}", errs)),
                    e,
                ))
            }
        }
    }
    fn get_priority(&self) -> gnutls_priority_t {
        self._g._priority
    }

    fn set_dn_verification_callback(&mut self, cb: Option<DnCallback>) {
        self._dn_callback = cb;
    }

    fn set_enable_certificate_verification(&mut self, enable: bool) {
        self._enable_certificate_verification = enable;
    }

    fn need_load_system_trust(&self) -> bool {
        self._load_system_trust
    }

    /// Through the credentials' root pointer: the section runs in line when the semaphore is
    /// free, while `with_semaphore` holds its `&mut` to `_system_trust_sem`, so it touches the
    /// other fields by place only, never through a `&mut Self`.
    ///
    /// # Safety
    /// `this` is the credentials' root pointer, on their shard, with no reference to them
    /// live; they do not move and outlive the returned future.
    unsafe fn maybe_load_system_trust(this: *mut Self) -> Future<()> {
        let section = move || {
            // SAFETY: the credentials outlive the semaphore-guarded section (guaranteed by the
            // caller); a place read of a field the semaphore's borrow does not cover.
            if !unsafe { (*this)._load_system_trust } {
                return make_ready_future(());
            }
            // SAFETY: as above; set_system_trust touches `_g` and `_load_system_trust` only.
            unsafe { Self::set_system_trust(this) }
        };
        // SAFETY: the semaphore is a field of the credentials, which do not move and outlive
        // the section (guaranteed by the caller).
        unsafe { with_semaphore(&mut (*this)._system_trust_sem, 1, section) }
    }
}

impl Default for CertificateCredentialsImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl CertificateCredentials {
    /// Empty credentials.
    pub fn new() -> Self {
        CertificateCredentials {
            _impl: make_shared(CertificateCredentialsImpl::new()),
            _reloadable: None,
        }
    }
}

impl Default for CertificateCredentials {
    fn default() -> Self {
        Self::new()
    }
}

impl AbstractCredentials for CertificateCredentials {
    fn set_x509_trust(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_x509_trust(b, fmt)
    }
    fn set_x509_crl(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_x509_crl(b, fmt)
    }
    fn set_x509_key(
        &mut self,
        cert: Blob<'_>,
        key: Blob<'_>,
        fmt: X509CrtFormat,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_x509_key(cert, key, fmt)
    }
    fn set_simple_pkcs12(
        &mut self,
        b: Blob<'_>,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_simple_pkcs12(b, fmt, password)
    }
}

fn abstract_set_x509_trust_file<C: AbstractCredentials + 'static>(
    c: &mut C,
    cafile: &Sstring,
    fmt: X509CrtFormat,
) -> Future<()> {
    let this: *mut C = c;
    read_fully(cafile, &Sstring::from("trust file")).then(move |r: FileResult| {
        // SAFETY: the caller keeps the credentials alive until the load completes.
        match unsafe { (*this).set_x509_trust(r.buf.as_slice(), fmt) } {
            Ok(()) => make_ready_future(()),
            Err(e) => make_exception_future(e),
        }
    })
}

fn abstract_set_x509_crl_file<C: AbstractCredentials + 'static>(
    c: &mut C,
    crlfile: &Sstring,
    fmt: X509CrtFormat,
) -> Future<()> {
    let this: *mut C = c;
    read_fully(crlfile, &Sstring::from("crl file")).then(move |r: FileResult| {
        // SAFETY: as above.
        match unsafe { (*this).set_x509_crl(r.buf.as_slice(), fmt) } {
            Ok(()) => make_ready_future(()),
            Err(e) => make_exception_future(e),
        }
    })
}

fn abstract_set_x509_key_file<C: AbstractCredentials + 'static>(
    c: &mut C,
    cf: &Sstring,
    kf: &Sstring,
    fmt: X509CrtFormat,
) -> Future<()> {
    let this: *mut C = c;
    let kf = kf.clone();
    read_fully(cf, &Sstring::from("certificate file")).then(move |r: FileResult| {
        let buf = r.buf;
        read_fully(&kf, &Sstring::from("key file")).then(move |r2: FileResult| {
            // SAFETY: as above.
            match unsafe { (*this).set_x509_key(buf.as_slice(), r2.buf.as_slice(), fmt) } {
                Ok(()) => make_ready_future(()),
                Err(e) => make_exception_future(e),
            }
        })
    })
}

fn abstract_set_simple_pkcs12_file<C: AbstractCredentials + 'static>(
    c: &mut C,
    pkcs12file: &Sstring,
    fmt: X509CrtFormat,
    password: &Sstring,
) -> Future<()> {
    let this: *mut C = c;
    let password = password.clone();
    read_fully(pkcs12file, &Sstring::from("pkcs12 file")).then(move |r: FileResult| {
        // SAFETY: as above.
        match unsafe { (*this).set_simple_pkcs12(r.buf.as_slice(), fmt, &password) } {
            Ok(()) => make_ready_future(()),
            Err(e) => make_exception_future(e),
        }
    })
}

impl CertificateCredentials {
    /// Loads the system's default trusted certificate authorities into these credentials.
    ///
    /// # Errors
    ///
    /// The returned future fails if the system trust store cannot be loaded.
    pub fn set_system_trust(&mut self) -> Future<()> {
        // SAFETY: the shared object is on this shard, at its node's root pointer, and nothing
        // else holds a reference to it during the call; the load keeps no reference either
        // and the caller keeps the object alive until the load completes.
        unsafe { CertificateCredentialsImpl::set_system_trust(shared_root(&self._impl)) }
    }

    // TODO add methods for certificate verification

    /// Sets the TLS handshake priority string, which selects the allowed handshake
    /// algorithms and their order.
    ///
    /// The syntax is GnuTLS's, described at
    /// <https://gnutls.org/manual/html_node/Priority-Strings.html>.
    ///
    /// # Errors
    ///
    /// Returns an error if GnuTLS rejects `prio`.
    pub fn set_priority_string(&mut self, prio: &Sstring) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_priority_string(prio)
    }

    /// Registers a callback that receives the Distinguished Name (DN) information of the
    /// peer's certificate during the TLS handshake.
    ///
    /// The DN comes from the leaf certificate of the chain the peer sends. The callback
    /// is not invoked when the peer sends no certificate (for example, a client
    /// connecting to a server whose client authentication is not
    /// [`ClientAuth::REQUIRE`]).
    ///
    /// The callback does not replace the standard certificate verification; it only
    /// allows extra checks. To abort the handshake, return an error from it, such as a
    /// [`VerificationError`].
    ///
    /// Passing `None` unregisters the callback. See [`DnCallback`] for its arguments.
    pub fn set_dn_verification_callback(&mut self, cb: Option<DnCallback>) {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_dn_verification_callback(cb);
    }

    /// Enables or disables verification of the peer certificate (enabled by default).
    pub fn set_enable_certificate_verification(&mut self, enable: bool) {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_enable_certificate_verification(enable);
    }
}

impl ServerCredentials {
    /// Server credentials; with GnuTLS >= 3.6 no DH parameters are needed.
    pub fn new() -> Result<Self, ExceptionPtr> {
        #[cfg(feature = "gnutls_legacy")]
        {
            Self::with_dh_params(&DhParams::new(DhLevel::LEGACY)?)
        }
        #[cfg(not(feature = "gnutls_legacy"))]
        {
            Ok(Self::from_base(CertificateCredentials::new()))
        }
    }

    /// Server credentials over `base`, not shared with other shards.
    fn from_base(base: CertificateCredentials) -> Self {
        ServerCredentials { base, _shards: None, _shard_copy: false }
    }

    /// Prepares `creds` for a server listening on every shard, since the credentials
    /// themselves are not reference-counted atomically and cannot be copied to other
    /// shards: the result goes to each shard, where [`SharedServerCredentials::into_local`] makes that shard's
    /// credentials over the same gnutls objects. A reload of `creds` reaches those copies.
    /// System trust, if asked for, is loaded first, since the load changes the gnutls
    /// credentials.
    pub fn share_with_shards(
        creds: &SharedPtr<ServerCredentials>,
    ) -> Future<SharedServerCredentials> {
        let c = creds.clone();
        // SAFETY: the credentials are on this shard and nothing else uses them during the
        // call; the load keeps the credentials alive through `c`.
        let load = unsafe {
            CertificateCredentialsImpl::maybe_load_system_trust(shared_root(
                &creds.get_mut_unchecked().base._impl,
            ))
        };
        load.then(move |()| {
            // SAFETY: as above, for this continuation.
            let me = unsafe { c.get_mut_unchecked() };
            let settings = match SharedCredentialsSettings::of(&me.base._impl) {
                Ok(s) => s,
                Err(e) => return make_exception_future(e),
            };
            let shards = me
                ._shards
                .get_or_insert_with(|| {
                    Arc::new(CredentialsShards {
                        _slots: (0..Smp::count()).map(|_| UnsafeCell::new(Vec::new())).collect(),
                    })
                })
                .clone();
            make_ready_future(SharedServerCredentials { _settings: settings, _shards: shards })
        })
    }

    /// After a reload of these credentials: gives their shard copies the new gnutls objects.
    fn update_shards(&self) {
        let Some(shards) = &self._shards else {
            return;
        };
        let settings = match SharedCredentialsSettings::of(&self.base._impl) {
            Ok(s) => s,
            Err(e) => {
                // The copies keep the credentials they had.
                crate::log_error!(
                    crate::engine::core::reactor::engine_logger(),
                    "cannot reload tls credentials on other shards: {:?}",
                    e
                );
                return;
            }
        };
        for i in 0..Smp::count() {
            let shards = shards.clone();
            let settings = settings.clone();
            // FIXME: future is discarded (callers expect the reload to be synchronous).
            let _ = Smp::submit_to(i, move || {
                shards.update_local(&settings);
                make_ready_future(())
            });
        }
    }

    /// Server credentials with the DH parameters `dh` (copied).
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when GnuTLS cannot allocate the credentials or copy the parameters.
    pub fn with_shared_dh_params(dh: &SharedPtr<DhParams>) -> Result<Self, ExceptionPtr> {
        Self::with_dh_params(dh)
    }

    /// Server credentials with the DH parameters `dh` (copied).
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when GnuTLS cannot allocate the credentials or copy the parameters.
    pub fn with_dh_params(dh: &DhParams) -> Result<Self, ExceptionPtr> {
        let s = Self::from_base(CertificateCredentials::new());
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { s._impl.get_mut_unchecked() }.dh_params(dh)?;
        Ok(s)
    }

    /// Sets whether client certificates are asked for.
    pub fn set_client_auth(&mut self, ca: ClientAuth) {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_client_auth(ca);
    }

    /// Sets the session resumption mode.
    ///
    /// With [`SessionResumeMode::TLS13_SESSION_TICKET`], each call also rotates the
    /// session ticket key, starting a new window of TLS session keys.
    ///
    /// # Errors
    ///
    /// Returns an error if the ticket key cannot be generated or installed.
    pub fn set_session_resume_mode(&mut self, m: SessionResumeMode) -> Result<(), ExceptionPtr> {
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { self._impl.get_mut_unchecked() }.set_session_resume_mode(m, &[])
    }
}

impl AbstractCredentials for ServerCredentials {
    fn set_x509_trust(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        self.base.set_x509_trust(b, fmt)
    }
    fn set_x509_crl(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        self.base.set_x509_crl(b, fmt)
    }
    fn set_x509_key(
        &mut self,
        cert: Blob<'_>,
        key: Blob<'_>,
        fmt: X509CrtFormat,
    ) -> Result<(), ExceptionPtr> {
        self.base.set_x509_key(cert, key, fmt)
    }
    fn set_simple_pkcs12(
        &mut self,
        b: Blob<'_>,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        self.base.set_simple_pkcs12(b, fmt, password)
    }
}

const DH_LEVEL_KEY: &str = "dh_level";
const X509_TRUST_KEY: &str = "x509_trust";
const X509_CRL_KEY: &str = "x509_crl";
const X509_KEY_KEY: &str = "x509_key";
const PKCS12_KEY: &str = "pkcs12";
const SYSTEM_TRUST: &str = "system_trust";

/// The contents of a file the builder holds.
pub type BufferType = Vec<u8>;

/// A trust or revocation blob of the builder.
#[derive(Clone, Debug)]
pub struct X509Simple {
    /// The contents.
    pub data: BufferType,
    /// Their encoding.
    pub format: X509CrtFormat,
    /// The file they were read from, if any.
    pub file: Option<FileInfo>,
}

/// A certificate and key pair of the builder.
#[derive(Clone, Debug)]
pub struct X509Key {
    /// The certificate.
    pub cert: BufferType,
    /// The key.
    pub key: BufferType,
    /// Their encoding.
    pub format: X509CrtFormat,
    /// The certificate file, if any.
    pub cert_file: Option<FileInfo>,
    /// The key file, if any.
    pub key_file: Option<FileInfo>,
}

/// A PKCS#12 bundle of the builder.
#[derive(Clone, Debug)]
pub struct Pkcs12Simple {
    /// The contents.
    pub data: BufferType,
    /// Their encoding.
    pub format: X509CrtFormat,
    /// The password.
    pub password: Sstring,
    /// The file it was read from, if any.
    pub file: Option<FileInfo>,
}

fn file_name(f: &Option<FileInfo>) -> Sstring {
    f.as_ref().map(|f| f.filename.clone()).unwrap_or_default()
}

impl CredentialsBuilder {
    fn emplace(&mut self, key: &str, v: BlobValue) {
        self._blobs.entry(Sstring::from(key)).or_default().push(v);
    }

    /// Sets the DH parameter strength of the server credentials built.
    pub fn set_dh_level(&mut self, level: DhLevel) {
        self.emplace(DH_LEVEL_KEY, BlobValue::DhLevel(level));
    }

    /// Loads the trusted CAs.
    pub fn set_system_trust(&mut self) -> Future<()> {
        // TODO / Caveat:
        // We cannot actually issue a loading of system trust here,
        // because we have no actual tls context.
        // And we probably _don't want to get into the guessing game
        // of where the system trust cert chains are, since this is
        // super distro dependent, and usually compiled into the library.
        // Pretent it is raining, and just set a flag.
        // Leave the function returning future, so if we change our
        // minds and want to do explicit loading, we can...
        self.emplace(SYSTEM_TRUST, BlobValue::Bool(true));
        make_ready_future(())
    }

    /// Sets whether client certificates are asked for.
    pub fn set_client_auth(&mut self, auth: ClientAuth) {
        self._client_auth = auth;
    }

    /// Sets the handshake priority string.
    pub fn set_priority_string(&mut self, prio: &Sstring) {
        self._priority = prio.clone();
    }

    /// Sets the session resumption mode applied to every server credential set built
    /// from this builder.
    ///
    /// Any mode other than [`SessionResumeMode::NONE`] generates a session key that all
    /// server credentials built afterwards share. To build server credentials with
    /// different keys from the same builder, call this method again between builds.
    ///
    /// # Errors
    ///
    /// Returns an error if the session key cannot be generated.
    pub fn set_session_resume_mode(&mut self, m: SessionResumeMode) -> Result<(), ExceptionPtr> {
        self._session_resume_mode = m;
        if m != SessionResumeMode::NONE {
            let mut key = GnutlsDatum::new();
            // SAFETY: the datum receives a gnutls-allocated key.
            gtls_chk(unsafe { gnutls_session_ticket_key_generate(&mut key.0) })?;
            // SAFETY: the datum holds `size` bytes.
            self._session_resume_key =
                unsafe { std::slice::from_raw_parts(key.0.data, key.0.size as usize) }.to_vec();
        }
        Ok(())
    }

    /// Applies the builder's settings to `creds`.
    pub fn apply_to(&self, creds: &mut CertificateCredentials) -> Result<(), ExceptionPtr> {
        let mut r: Result<(), ExceptionPtr> = Ok(());
        visit_blobs(&self._blobs, |key, v| {
            if r.is_err() {
                return;
            }
            r = match v {
                BlobValue::X509Simple(info) => {
                    if key == X509_TRUST_KEY {
                        creds.set_x509_trust(&info.data, info.format)
                    } else if key == X509_CRL_KEY {
                        creds.set_x509_crl(&info.data, info.format)
                    } else {
                        Ok(())
                    }
                }
                BlobValue::X509Key(info) => creds.set_x509_key(&info.cert, &info.key, info.format),
                BlobValue::Pkcs12Simple(info) => {
                    creds.set_simple_pkcs12(&info.data, info.format, &info.password)
                }
                _ => Ok(()),
            };
        });
        r?;

        // TODO / Caveat:
        // We cannot do this immediately, because we are not a continuation, and
        // potentially blocking calls are a no-no.
        // Doing this detached would be indeterministic, so set a flag in
        // credentials, and do actual loading in first handshake (see session)
        if self._blobs.contains_key(&Sstring::from(SYSTEM_TRUST)) {
            // SAFETY: the shared object is on this shard and the call below is the only access
            // to it while it runs (none of these calls re-enters the object).
            unsafe { creds._impl.get_mut_unchecked() }._load_system_trust = true;
        }

        if !self._priority.empty() {
            creds.set_priority_string(&self._priority)?;
        }

        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { creds._impl.get_mut_unchecked() }.set_client_auth(self._client_auth);
        // Note: this causes server session key rotation on cert reload
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { creds._impl.get_mut_unchecked() }
            .set_session_resume_mode(self._session_resume_mode, &self._session_resume_key)
    }

    /// Builds client credentials.
    pub fn build_certificate_credentials(
        &self,
    ) -> Result<SharedPtr<CertificateCredentials>, ExceptionPtr> {
        let creds = make_shared(CertificateCredentials::new());
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        self.apply_to(unsafe { creds.get_mut_unchecked() })?;
        Ok(creds)
    }

    /// Builds server credentials.
    pub fn build_server_credentials(&self) -> Result<SharedPtr<ServerCredentials>, ExceptionPtr> {
        let level = self._blobs.get(&Sstring::from(DH_LEVEL_KEY)).and_then(|v| match v.first() {
            Some(BlobValue::DhLevel(l)) => Some(*l),
            _ => None,
        });
        let Some(level) = level else {
            #[cfg(feature = "gnutls_legacy")]
            {
                return Err(ExceptionPtr::from(InvalidArgument("No DH level set".to_string())));
            }
            #[cfg(not(feature = "gnutls_legacy"))]
            {
                let creds = make_shared(ServerCredentials::new()?);
                // SAFETY: the shared object is on this shard and the call below is the only access
                // to it while it runs (none of these calls re-enters the object).
                self.apply_to(&mut unsafe { creds.get_mut_unchecked() }.base)?;
                return Ok(creds);
            }
        };
        let creds = make_shared(ServerCredentials::with_dh_params(&DhParams::new(level)?)?);
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        self.apply_to(&mut unsafe { creds.get_mut_unchecked() }.base)?;
        Ok(creds)
    }

    /// Replaces the state of `creds` with freshly built client credentials.
    pub fn rebuild_certificate(
        &self,
        creds: &mut CertificateCredentials,
    ) -> Result<(), ExceptionPtr> {
        let tmp = self.build_certificate_credentials()?;
        creds._impl = tmp._impl.clone();
        Ok(())
    }

    /// Replaces the state of `creds` with freshly built server credentials.
    pub fn rebuild_server(&self, creds: &mut ServerCredentials) -> Result<(), ExceptionPtr> {
        let tmp = self.build_server_credentials()?;
        creds.base._impl = tmp.base._impl.clone();
        Ok(())
    }

    // same as above, but any files used for certs/keys etc will be watched
    // for modification and reloaded if changed
    /// Builds client credentials that reload their files when they change.
    pub fn build_reloadable_certificate_credentials(
        &self,
        cb: ReloadCallbackEx,
        tolerance: Option<Duration>,
    ) -> Future<SharedPtr<CertificateCredentials>> {
        let creds = match self.build_certificate_credentials() {
            Ok(c) => c,
            Err(e) => return make_exception_future(e),
        };
        // The node's root pointer: the reload keeps it past later borrows of the object.
        let target = ReloadTarget::Certificate(shared_root(&creds));
        let base = match ReloadableCredentialsBase::new(
            self.clone(),
            cb,
            target,
            tolerance.unwrap_or(ReloadableCredentialsBase::DEFAULT_TOLERANCE),
        ) {
            Ok(b) => b,
            Err(e) => return make_exception_future(e),
        };
        let f = base.init();
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { creds.get_mut_unchecked() }._reloadable = Some(base);
        f.then(move |()| make_ready_future::<SharedPtr<CertificateCredentials>>(creds))
    }

    /// Builds server credentials that reload their files when they change.
    pub fn build_reloadable_server_credentials(
        &self,
        cb: ReloadCallbackEx,
        tolerance: Option<Duration>,
    ) -> Future<SharedPtr<ServerCredentials>> {
        let creds = match self.build_server_credentials() {
            Ok(c) => c,
            Err(e) => return make_exception_future(e),
        };
        // The node's root pointer: the reload keeps it past later borrows of the object.
        let target = ReloadTarget::Server(shared_root(&creds));
        let base = match ReloadableCredentialsBase::new(
            self.clone(),
            cb,
            target,
            tolerance.unwrap_or(ReloadableCredentialsBase::DEFAULT_TOLERANCE),
        ) {
            Ok(b) => b,
            Err(e) => return make_exception_future(e),
        };
        let f = base.init();
        // SAFETY: the shared object is on this shard and the call below is the only access
        // to it while it runs (none of these calls re-enters the object).
        unsafe { creds.get_mut_unchecked() }.base._reloadable = Some(base);
        f.then(move |()| make_ready_future::<SharedPtr<ServerCredentials>>(creds))
    }

    /// As [`CredentialsBuilder::build_reloadable_certificate_credentials`], with a callback
    /// that is not given the builder and returns nothing.
    pub fn build_reloadable_certificate_credentials_simple(
        &self,
        cb: ReloadCallback,
        tolerance: Option<Duration>,
    ) -> Future<SharedPtr<CertificateCredentials>> {
        let ex: ReloadCallbackEx = Some(std::rc::Rc::new(
            move |_: &CredentialsBuilder, files: &HashSet<Sstring>, p: Option<ExceptionPtr>| {
                if let Some(cb) = &cb {
                    cb(files, p);
                }
                make_ready_future(())
            },
        ));
        self.build_reloadable_certificate_credentials(ex, tolerance)
    }

    /// As [`CredentialsBuilder::build_reloadable_server_credentials`], with a callback that
    /// is not given the builder and returns nothing.
    pub fn build_reloadable_server_credentials_simple(
        &self,
        cb: ReloadCallback,
        tolerance: Option<Duration>,
    ) -> Future<SharedPtr<ServerCredentials>> {
        let ex: ReloadCallbackEx = Some(std::rc::Rc::new(
            move |_: &CredentialsBuilder, files: &HashSet<Sstring>, p: Option<ExceptionPtr>| {
                if let Some(cb) = &cb {
                    cb(files, p);
                }
                make_ready_future(())
            },
        ));
        self.build_reloadable_server_credentials(ex, tolerance)
    }
}

impl AbstractCredentials for CredentialsBuilder {
    fn set_x509_trust(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        self.emplace(
            X509_TRUST_KEY,
            BlobValue::X509Simple(X509Simple { data: b.to_vec(), format: fmt, file: None }),
        );
        Ok(())
    }
    fn set_x509_crl(&mut self, b: Blob<'_>, fmt: X509CrtFormat) -> Result<(), ExceptionPtr> {
        self.emplace(
            X509_CRL_KEY,
            BlobValue::X509Simple(X509Simple { data: b.to_vec(), format: fmt, file: None }),
        );
        Ok(())
    }
    fn set_x509_key(
        &mut self,
        cert: Blob<'_>,
        key: Blob<'_>,
        fmt: X509CrtFormat,
    ) -> Result<(), ExceptionPtr> {
        self.emplace(
            X509_KEY_KEY,
            BlobValue::X509Key(X509Key {
                cert: cert.to_vec(),
                key: key.to_vec(),
                format: fmt,
                cert_file: None,
                key_file: None,
            }),
        );
        Ok(())
    }
    fn set_simple_pkcs12(
        &mut self,
        b: Blob<'_>,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Result<(), ExceptionPtr> {
        self.emplace(
            PKCS12_KEY,
            BlobValue::Pkcs12Simple(Pkcs12Simple {
                data: b.to_vec(),
                format: fmt,
                password: password.clone(),
                file: None,
            }),
        );
        Ok(())
    }

    fn set_x509_trust_file(&mut self, cafile: &Sstring, fmt: X509CrtFormat) -> Future<()> {
        let this: *mut CredentialsBuilder = self;
        read_fully(cafile, &Sstring::from("trust file")).then(move |f: FileResult| {
            // SAFETY: the caller keeps the builder alive until the load completes.
            unsafe {
                (*this).emplace(
                    X509_TRUST_KEY,
                    BlobValue::X509Simple(X509Simple {
                        data: to_buffer(&f.buf),
                        format: fmt,
                        file: Some(f.file),
                    }),
                )
            };
            make_ready_future(())
        })
    }

    fn set_x509_crl_file(&mut self, crlfile: &Sstring, fmt: X509CrtFormat) -> Future<()> {
        let this: *mut CredentialsBuilder = self;
        read_fully(crlfile, &Sstring::from("crl file")).then(move |f: FileResult| {
            // SAFETY: as above.
            unsafe {
                (*this).emplace(
                    X509_CRL_KEY,
                    BlobValue::X509Simple(X509Simple {
                        data: to_buffer(&f.buf),
                        format: fmt,
                        file: Some(f.file),
                    }),
                )
            };
            make_ready_future(())
        })
    }

    fn set_x509_key_file(&mut self, cf: &Sstring, kf: &Sstring, fmt: X509CrtFormat) -> Future<()> {
        let this: *mut CredentialsBuilder = self;
        let kf = kf.clone();
        read_fully(cf, &Sstring::from("certificate file")).then(move |cf: FileResult| {
            read_fully(&kf, &Sstring::from("key file")).then(move |kf: FileResult| {
                // SAFETY: as above.
                unsafe {
                    (*this).emplace(
                        X509_KEY_KEY,
                        BlobValue::X509Key(X509Key {
                            cert: to_buffer(&cf.buf),
                            key: to_buffer(&kf.buf),
                            format: fmt,
                            cert_file: Some(cf.file),
                            key_file: Some(kf.file),
                        }),
                    )
                };
                make_ready_future(())
            })
        })
    }

    fn set_simple_pkcs12_file(
        &mut self,
        pkcs12file: &Sstring,
        fmt: X509CrtFormat,
        password: &Sstring,
    ) -> Future<()> {
        let this: *mut CredentialsBuilder = self;
        let password = password.clone();
        read_fully(pkcs12file, &Sstring::from("pkcs12 file")).then(move |f: FileResult| {
            // SAFETY: as above.
            unsafe {
                (*this).emplace(
                    PKCS12_KEY,
                    BlobValue::Pkcs12Simple(Pkcs12Simple {
                        data: to_buffer(&f.buf),
                        format: fmt,
                        password,
                        file: Some(f.file),
                    }),
                )
            };
            make_ready_future(())
        })
    }
}

fn to_buffer(buf: &TemporaryBuffer<u8>) -> BufferType {
    buf.as_slice().to_vec()
}

/// Visits the certificate blobs in the order GnuTLS needs them: trust, crl, key, pkcs12.
fn visit_blobs<F: FnMut(&str, &BlobValue)>(
    blobs: &BTreeMap<Sstring, Vec<BlobValue>>,
    mut visitor: F,
) {
    for key in [X509_TRUST_KEY, X509_CRL_KEY, X509_KEY_KEY, PKCS12_KEY] {
        if let Some(vs) = blobs.get(&Sstring::from(key)) {
            for v in vs {
                visitor(key, v);
            }
        }
    }
}

fn visit_blobs_mut<F: FnMut(&str, &mut BlobValue)>(
    blobs: &mut BTreeMap<Sstring, Vec<BlobValue>>,
    mut visitor: F,
) {
    for key in [X509_TRUST_KEY, X509_CRL_KEY, X509_KEY_KEY, PKCS12_KEY] {
        if let Some(vs) = blobs.get_mut(&Sstring::from(key)) {
            for v in vs.iter_mut() {
                visitor(key, v);
            }
        }
    }
}

use crate::engine::core::fsnotify::{
    Fsnotifier, FsnotifierEvent, FsnotifierFlags, FsnotifierWatch, WatchToken,
};
use crate::engine::core::loop_::parallel_for_each;
use crate::engine::core::timer::Timer;
use std::pin::Pin;

/// What a `reloadable_credentials<Base>` rebuilds: the `Base` part.
///
/// Crate-private: only `build_reloadable_certificate_credentials` and
/// `build_reloadable_server_credentials` make one, from the root pointer of the shared
/// credentials that then own the reloading builder holding it.
#[derive(Clone, Copy)]
pub(crate) enum ReloadTarget {
    /// Reloadable client credentials.
    Certificate(*mut CertificateCredentials),
    /// Reloadable server credentials.
    Server(*mut ServerCredentials),
}

impl ReloadTarget {
    /// Rebuilds the target credentials from `builder`.
    ///
    /// # Safety
    /// The pointer in `self` points to live credentials on this shard, with no reference to
    /// them live during the call. A target held by a `ReloadingBuilder` meets this: the
    /// credentials own their reloadable base, which detaches the builder (clearing the
    /// target) when they are destroyed, and the builder runs on their shard without keeping
    /// a reference to them.
    unsafe fn rebuild(&self, builder: &CredentialsBuilder) -> Result<(), ExceptionPtr> {
        match *self {
            // SAFETY: guaranteed by the caller: `c` is live and not otherwise borrowed during
            // the call.
            ReloadTarget::Certificate(c) => builder.rebuild_certificate(unsafe { &mut *c }),
            ReloadTarget::Server(s) => {
                // SAFETY: guaranteed by the caller: `s` is live and not otherwise borrowed
                // during the call.
                builder.rebuild_server(unsafe { &mut *s })?;
                // the shard copies of these credentials take the rebuilt state too
                // SAFETY: as above; the `&mut` of the previous call has ended.
                unsafe { (*s).update_shards() };
                Ok(())
            }
        }
    }
}

/// The builder of reloadable credentials: it watches the files the builder read, and when
/// they change (after a tolerance delay that coalesces bursts of events) rebuilds the
/// credentials and calls the reload callback.
pub struct ReloadingBuilder {
    /// The builder's own address with the provenance of its `SharedPtr` node, written once
    /// by `ReloadableCredentialsBase::new`.
    _this: *mut ReloadingBuilder,
    _esft: EnableSharedFromThis,
    builder: CredentialsBuilder,
    _cb: ReloadCallbackEx,
    _creds: Option<ReloadTarget>,
    _fsn: Fsnotifier,
    _watches: std::collections::HashMap<WatchToken, (FsnotifierWatch, Sstring)>,
    _files: std::collections::HashMap<Sstring, FsnotifierFlags>,
    _all_files: HashSet<Sstring>,
    _timer: Timer,
    _delay: Duration,
}

impl ReloadingBuilder {
    fn new(
        b: CredentialsBuilder,
        cb: ReloadCallbackEx,
        creds: ReloadTarget,
        delay: Duration,
    ) -> Result<Self, ExceptionPtr> {
        Ok(ReloadingBuilder {
            _this: std::ptr::null_mut(),
            _esft: EnableSharedFromThis::new(),
            builder: b,
            _cb: cb,
            _creds: Some(creds),
            _fsn: Fsnotifier::new()?,
            _watches: std::collections::HashMap::new(),
            _files: std::collections::HashMap::new(),
            _all_files: HashSet::new(),
            _timer: Timer::new(),
            _delay: delay,
        })
    }

    fn me(&self) -> SharedPtr<ReloadingBuilder> {
        // SAFETY: every reloading builder is made by make_shared_esft in
        // ReloadableCredentialsBase::new, which writes `_this` from the shared pointer.
        unsafe { Self::shared_from_this(self._this) }
    }

    /// # Safety
    /// `this` is the builder's root pointer, with no reference to it live: the watches may
    /// be added in line.
    unsafe fn init(this: *mut Self) -> Future<()> {
        // SAFETY: guaranteed by the caller; the reference is not used once the loop starts.
        let me_ = unsafe { &mut *this };
        let mut all = HashSet::new();
        visit_blobs(&me_.builder._blobs, |_, v| match v {
            BlobValue::X509Simple(info) => {
                all.insert(file_name(&info.file));
            }
            BlobValue::X509Key(info) => {
                all.insert(file_name(&info.cert_file));
                all.insert(file_name(&info.key_file));
            }
            BlobValue::Pkcs12Simple(info) => {
                all.insert(file_name(&info.file));
            }
            _ => {}
        });
        me_._all_files = all;
        let files: Vec<Sstring> = me_._all_files.iter().cloned().collect();
        let me = me_.me();
        parallel_for_each(files, move |f: Sstring| {
            if !f.empty() {
                // SAFETY: `me` keeps the builder alive until the watches are added.
                return unsafe {
                    Self::add_watch(
                        this,
                        &f,
                        FsnotifierFlags::CLOSE_WRITE | FsnotifierFlags::DELETE_SELF,
                    )
                }
                .then(|_| make_ready_future(()));
            }
            make_ready_future(())
        })
        .finally(move || drop(me))
    }

    /// # Safety
    /// `this` is the builder's root pointer, with no reference to it live.
    unsafe fn start(this: *mut Self) {
        // SAFETY: guaranteed by the caller; a short access.
        let me = unsafe { (*this).me() };
        // run the loop in a thread. makes code almost readable.
        let _ = async_(move || {
            // SAFETY: `me` keeps the builder alive while the thread runs; `run` holds no
            // reference to it while the thread waits.
            unsafe { ReloadingBuilder::run(this) };
            Ok(())
        })
        .finally(move || drop(me));
    }

    /// The loop of the reloading thread. While the thread waits for file events, `detach`
    /// (from the credentials' destructor) and the timer run on the same object, so no
    /// reference to it is held across the wait: only short accesses through `this`.
    ///
    /// # Safety
    /// `this` is a live builder on this shard, kept alive by the caller.
    unsafe fn run(this: *mut ReloadingBuilder) {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live builder
        // on this shard, kept alive by the caller). Every access below is a short borrow that ends
        // before the thread can yield.
        unsafe {
            while (*this)._creds.is_some() {
                let r: Result<(), ExceptionPtr> = (|| {
                    let wait = (*this)._fsn.wait();
                    let events = wait.get()?;
                    if events.is_empty() && (*this)._creds.is_none() {
                        return Ok(());
                    }
                    Self::rebuild(this, &events)?;
                    Pin::new_unchecked(&mut (*this)._timer).cancel();
                    Ok(())
                })();
                if let Err(ep) = r
                    && !(*this)._timer.armed()
                {
                    let mut ep = Some(ep);
                    // the timer sits inside the shared_ptr node (stable address) and is
                    // owned by the builder, which therefore outlives the callback.
                    Pin::new_unchecked(&mut (*this)._timer)
                        .set_callback(move || Self::do_callback(this, ep.take()));
                    Pin::new_unchecked(&mut (*this)._timer).arm_in((*this)._delay);
                }
            }
        }
    }

    /// Through the builder's root pointer: the reloading thread may be waiting inside
    /// `rebuild`, so this makes no reference to the whole builder, only place accesses to
    /// the fields it changes.
    ///
    /// # Safety
    /// `this` is the builder's root pointer, on its shard, with no reference to `_creds`,
    /// `_cb`, `_fsn` or `_timer` live.
    unsafe fn detach(this: *mut Self) {
        // SAFETY: guaranteed by the caller; short accesses, and the timer sits inside the
        // shared_ptr node and does not move.
        unsafe {
            (*this)._creds = None;
            (*this)._cb = None;
            (*this)._fsn.shutdown();
            Pin::new_unchecked(&mut (*this)._timer).cancel();
        }
    }

    /// The files of the blobs that `rebuild` may reload, in the order `visit_blobs` gives
    /// their slots (a key-and-certificate pair is two slots, certificate first).
    fn blob_files(blobs: &BTreeMap<Sstring, Vec<BlobValue>>) -> Vec<Sstring> {
        let mut files = Vec::new();
        visit_blobs(blobs, |_, v| match v {
            BlobValue::X509Simple(info) => files.push(file_name(&info.file)),
            BlobValue::X509Key(info) => {
                files.push(file_name(&info.cert_file));
                files.push(file_name(&info.key_file));
            }
            BlobValue::Pkcs12Simple(info) => files.push(file_name(&info.file)),
            _ => {}
        });
        files
    }

    // called from teseladb::thread
    ///
    /// Through the builder's root pointer: the thread waits here (`add_watch(..).get()`,
    /// `read_fully(..).get()`, the callback's `.get()`), and meanwhile `detach` and the timer
    /// run on the same builder, so no reference to it is held across a wait and `_creds` is
    /// read again after them. The files are read first and written
    /// into the blobs afterwards, so no borrow of `_blobs` is live while the rest of the
    /// builder is touched.
    ///
    /// # Safety
    /// `this` is a live builder on this shard, kept alive by the caller, with no reference
    /// to it live; called from its engine thread.
    unsafe fn rebuild(this: *mut Self, events: &[FsnotifierEvent]) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live builder
        // on this shard, kept alive by the caller, with no reference to it live; called from its
        // engine thread). Every access below is a short borrow of one field that ends before the
        // thread can yield.
        unsafe {
            for e in events {
                // don't use at. We could be getting two events for
                // same watch (mod + delete), but we only need to care
                // about one...
                if let Some((_, filename)) = (*this)._watches.remove(&e.id) {
                    // only add actual file watches to
                    // query set. If this was a directory
                    // watch, the file should already be
                    // in there.
                    if (*this)._all_files.contains(&filename) {
                        (*this)._files.insert(filename, e.mask);
                    }
                }
            }
            let mut num_changed = 0;

            // Each file is reloaded into its blob, stopping at the first error: the reloads first, then the loaded buffers go
            // into their slots, those before an error included.
            let files = Self::blob_files(&(*this).builder._blobs);
            let mut loaded: Vec<Option<BufferType>> = Vec::with_capacity(files.len());
            let mut r: Result<(), ExceptionPtr> = Ok(());
            for filename in &files {
                match Self::maybe_reload(this, filename) {
                    Ok(b) => {
                        if b.is_some() {
                            num_changed += 1;
                        }
                        loaded.push(b);
                    }
                    Err(e) => {
                        r = Err(e);
                        break;
                    }
                }
            }
            let mut loaded = loaded.into_iter();
            let mut put = |dst: &mut BufferType| {
                if let Some(Some(b)) = loaded.next() {
                    *dst = b;
                }
            };
            visit_blobs_mut(&mut (*this).builder._blobs, |_, v| match v {
                BlobValue::X509Simple(info) => put(&mut info.data),
                BlobValue::X509Key(info) => {
                    put(&mut info.cert);
                    put(&mut info.key);
                }
                BlobValue::Pkcs12Simple(info) => put(&mut info.data),
                _ => {}
            });
            r?;
            // only try this if anything was in fact successfully loaded.
            // if files were missing, or pairs incomplete, we can just skip.
            if num_changed == 0 {
                return Ok(());
            }
            let rr: Result<(), ExceptionPtr> = (|| {
                // force rebuilding session resume mode key if
                // enabled. should not reuse sessions across certificate
                // change (should not work anyway)
                let m = (*this).builder._session_resume_mode;
                (*this).builder.set_session_resume_mode(m)?;
                if let Some(c) = (*this)._creds {
                    // `_creds` was read again after the waits above, so it is set only while
                    // the credentials it points to are live (`detach` clears it when they are
                    // destroyed); this thread runs on their shard and holds no reference to
                    // them, which is `ReloadTarget::rebuild`'s contract.
                    c.rebuild(&(*this).builder)?;
                }
                Ok(())
            })();
            if let Err(e) = rr {
                if (*this)._files.values().any(|f| *f == FsnotifierFlags::IGNORED) {
                    // if any file in the reload set was deleted - i.e. we have not seen a "closed" yet - assume
                    // this is a spurious reload and we'd better wait for next event - hopefully a "closed" -
                    // and try again
                    return Ok(());
                }
                return Err(e);
            }
            // if we got here, all files loaded, all watches were created,
            // and gnutls was ok with the content. success.
            Self::do_callback(this, None);
            Self::on_success(this);
            Ok(())
        }
    }

    /// The reload of one file for `rebuild`, without the write into its blob: the reloaded
    /// contents, or `None` when the file is not in the reload set or its watch
    /// cannot be added.
    ///
    /// # Safety
    /// As for [`ReloadingBuilder::rebuild`].
    unsafe fn maybe_reload(
        this: *mut Self,
        filename: &Sstring,
    ) -> Result<Option<BufferType>, ExceptionPtr> {
        // SAFETY: the caller upholds this function's `# Safety` contract (as for
        // [`ReloadingBuilder::rebuild`]). The only borrow (of `_files`) ends before the first wait.
        unsafe {
            if filename.empty() || !(*this)._files.contains_key(filename) {
                return Ok(None);
            }
            // #756
            // first, add a watch to nearest parent dir we
            // can find. If user deleted folders, we could end
            // up looking at modifications to root.
            // The idea is that should adding a watch to actual file
            // fail (deleted file/folder), we wait for changes to closest
            // parent. When this happens, we will retry all files
            // that have not been successfully replaced (and maybe more),
            // repeating the process. At some point, we hopefully
            // get new, current data.
            Self::add_dir_watch(this, filename)?;
            // #756 add watch _first_. File could change while we are
            // reading this.
            if Self::add_watch(
                this,
                filename,
                FsnotifierFlags::CLOSE_WRITE | FsnotifierFlags::DELETE_SELF,
            )
            .get()
            .is_err()
            {
                // let's just assume if this happens, it's because the file or folder was deleted.
                // just ignore for now, and hope the dir watch will tell us when it is back...
                return Ok(None);
            }
            let buf = read_fully(filename, &Sstring::from("reloading")).get()?;
            Ok(Some(to_buffer(&buf.buf)))
        }
    }

    /// # Safety
    /// `this` is a live builder on this shard with no reference to `_files` or `_watches`
    /// live.
    unsafe fn on_success(this: *mut Self) {
        // SAFETY: guaranteed by the caller; borrows of three distinct fields.
        unsafe {
            (*this)._files.clear();
            // remove all directory watches, since we've successfully
            // reloaded -> the file watches themselves should suffice now
            let all = &(*this)._all_files;
            (*this)._watches.retain(|_, (_, f)| all.contains(f));
        }
    }

    /// Through the builder's root pointer: the callback's future is waited for here (from
    /// the reloading thread), and `detach` may run meanwhile.
    ///
    /// # Safety
    /// `this` is a live builder on this shard, kept alive by the caller, with no reference
    /// to it live.
    unsafe fn do_callback(this: *mut Self, ep: Option<ExceptionPtr>) {
        // SAFETY: guaranteed by the caller; short borrows of `_cb` and `_files`. The callback
        // reads the builder while it runs; `detach` does not
        // touch `builder`.
        unsafe {
            if let Some(cb) = (*this)._cb.clone()
                && !(*this)._files.is_empty()
            {
                let files: HashSet<Sstring> = (*this)._files.keys().cloned().collect();
                let _ = cb(&(*this).builder, &files, ep).get();
            }
        }
    }

    // called from teseladb::thread
    ///
    /// # Safety
    /// As for [`ReloadingBuilder::rebuild`].
    unsafe fn add_dir_watch(
        this: *mut Self,
        filename: &Sstring,
    ) -> Result<WatchToken, ExceptionPtr> {
        let mut dir = std::path::PathBuf::from(filename.as_str())
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        loop {
            let d = Sstring::from(dir.to_string_lossy().as_ref());
            // SAFETY: guaranteed by the caller.
            match unsafe {
                Self::add_watch(this, &d, FsnotifierFlags::CREATE_CHILD | FsnotifierFlags::MOVE)
            }
            .get()
            {
                Ok(t) => return Ok(t),
                Err(e) => {
                    let parent = dir.parent().map(|p| p.to_path_buf()).unwrap_or_default();
                    if parent.as_os_str().is_empty() || dir == parent {
                        return Err(e);
                    }
                    dir = parent;
                    continue;
                }
            }
        }
    }

    /// # Safety
    /// `this` is a live builder on this shard, with no reference to `_fsn` or `_watches`
    /// live; it outlives its watches.
    unsafe fn add_watch(
        this: *mut Self,
        filename: &Sstring,
        flags: FsnotifierFlags,
    ) -> Future<WatchToken> {
        let filename = filename.clone();
        // SAFETY: guaranteed by the caller; a short borrow of `_fsn`.
        let f = unsafe { (*this)._fsn.create_watch(&filename, flags) };
        f.then(move |mut w: FsnotifierWatch| {
            // SAFETY: the builder outlives its watches; a short borrow of `_watches`, from
            // the reactor or in line above, with no other reference to it live.
            let watches = unsafe { &mut (*this)._watches };
            let t = w.token();
            // we might create multiple watches for same token in case of dirs, avoid deleting previously
            // created one
            if let std::collections::hash_map::Entry::Vacant(e) = watches.entry(t) {
                e.insert((w, filename));
            } else {
                w.release();
            }
            make_ready_future(t)
        })
    }
}

/// The reloading part of reloadable credentials: owns the [`ReloadingBuilder`] and detaches
/// it when the credentials are destroyed.
pub struct ReloadableCredentialsBase {
    _builder: SharedPtr<ReloadingBuilder>,
}

impl ReloadableCredentialsBase {
    /// The default delay between a file change and the rebuild.
    pub const DEFAULT_TOLERANCE: Duration = Duration::from_millis(500);

    fn new(
        builder: CredentialsBuilder,
        cb: ReloadCallbackEx,
        creds: ReloadTarget,
        delay: Duration,
    ) -> Result<Box<Self>, ExceptionPtr> {
        let b = make_shared_esft(ReloadingBuilder::new(builder, cb, creds, delay)?);
        // SAFETY: just made, so engaged; the root pointer of the shared object, which is on
        // this shard with no reference to it live.
        unsafe {
            let root = b.get().unwrap_unchecked().as_ptr();
            (*root)._this = root;
            ReloadingBuilder::start(root);
        }
        Ok(Box::new(ReloadableCredentialsBase { _builder: b }))
    }

    fn init(&self) -> Future<()> {
        // SAFETY: the shared object is on this shard, engaged, and no reference to it is
        // live; `_this` is its root pointer.
        unsafe { ReloadingBuilder::init((*self._builder.get().unwrap_unchecked().as_ptr())._this) }
    }
}

crate::shared_from_this_impl!(ReloadingBuilder { _esft });

impl Drop for ReloadableCredentialsBase {
    fn drop(&mut self) {
        // SAFETY: the shared object is on this shard, at its node's root pointer; the
        // reloading thread may be waiting inside it but holds no reference to the fields
        // detach changes (see `ReloadingBuilder::rebuild`).
        unsafe { ReloadingBuilder::detach(shared_root(&self._builder)) };
    }
}

// Reloadable credentials are ordinary credentials whose `_reloadable` is set, rebuilt
// through `ReloadTarget::rebuild`.

// ---------------------------------------------------------------------------
// The TLS session and the sockets built on it
// ---------------------------------------------------------------------------

use crate::engine::core::api::{listen_with as engine_listen, make_socket};
use crate::engine::core::future::futurize_invoke;
use crate::engine::core::iostream::{DataSink, DataSinkImpl, DataSource, DataSourceImpl};
use crate::engine::core::loop_::{StopIteration, repeat};
use crate::engine::core::reactor::engine;
use crate::engine::core::shared_ptr::{EnableLwSharedFromThis, LwSharedPtr, make_lw_shared};
use crate::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use crate::engine::core::with_timeout::with_timeout;
use crate::engine::net::api::{AcceptResult, KeepaliveParams};
use crate::engine::net::packet::{Fragment, Packet};
use crate::engine::net::socket_defs::{Transport, in_addr, in6_addr};
use crate::engine::net::stack::{ServerSocketImpl, SocketImpl};

/// GnuTLS's I/O vector (`giovec_t`), laid out as `struct iovec`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Giovec {
    iov_base: *mut c_void,
    iov_len: usize,
}

const GNUTLS_SERVER: c_uint = 1;
const GNUTLS_CLIENT: c_uint = 1 << 1;
const GNUTLS_NONBLOCK: c_uint = 1 << 3;
const GNUTLS_CRD_CERTIFICATE: c_int = 1;
const GNUTLS_CERT_IGNORE: c_int = 0;
const GNUTLS_CERT_REQUEST: c_int = 1;
const GNUTLS_CERT_REQUIRE: c_int = 2;
const GNUTLS_NAME_DNS: c_int = 1;
const GNUTLS_AL_FATAL: c_int = 2;
const GNUTLS_CERT_INVALID: c_uint = 1 << 1;
const GNUTLS_SHUT_WR: c_int = 1;
const GNUTLS_SFLAGS_SESSION_TICKET: c_uint = 1 << 7;
const GNUTLS_X509_FMT_DER: c_int = 0;
const GNUTLS_SAN_DNSNAME: c_int = 1;
const GNUTLS_SAN_RFC822NAME: c_int = 2;
const GNUTLS_SAN_URI: c_int = 3;
const GNUTLS_SAN_IPADDRESS: c_int = 4;
const GNUTLS_SAN_OTHERNAME: c_int = 5;
const GNUTLS_SAN_DN: c_int = 6;
// Linux values, like the rest of the crate (see `net/dns.rs`): the crate targets Linux.
const EAGAIN: c_int = 11;
const EIO: c_int = 5;
const EPIPE: c_int = 32;
const ENOTCONN: c_int = 107;

#[link(name = "gnutls")]
unsafe extern "C" {
    fn gnutls_init(session: *mut gnutls_session_t, flags: c_uint) -> c_int;
    fn gnutls_deinit(session: gnutls_session_t);
    fn gnutls_set_default_priority(session: gnutls_session_t) -> c_int;
    fn gnutls_credentials_set(session: gnutls_session_t, type_: c_int, cred: *mut c_void) -> c_int;
    fn gnutls_certificate_server_set_request(session: gnutls_session_t, req: c_int);
    fn gnutls_session_ticket_enable_server(
        session: gnutls_session_t,
        key: *const GnutlsDatumT,
    ) -> c_int;
    fn gnutls_priority_set(session: gnutls_session_t, priority: gnutls_priority_t) -> c_int;
    fn gnutls_transport_set_ptr(session: gnutls_session_t, ptr: gnutls_transport_ptr_t);
    fn gnutls_transport_get_ptr(session: gnutls_session_t) -> gnutls_transport_ptr_t;
    fn gnutls_transport_set_vec_push_function(
        session: gnutls_session_t,
        f: unsafe extern "C" fn(gnutls_transport_ptr_t, *const Giovec, c_int) -> isize,
    );
    fn gnutls_transport_set_pull_function(
        session: gnutls_session_t,
        f: unsafe extern "C" fn(gnutls_transport_ptr_t, *mut c_void, usize) -> isize,
    );
    fn gnutls_session_set_verify_function(
        session: gnutls_session_t,
        f: unsafe extern "C" fn(gnutls_session_t) -> c_int,
    );
    fn gnutls_session_set_data(
        session: gnutls_session_t,
        data: *const c_void,
        size: usize,
    ) -> c_int;
    fn gnutls_certificate_verification_status_print(
        status: c_uint,
        type_: c_int,
        out: *mut GnutlsDatumT,
        flags: c_uint,
    ) -> c_int;
    fn gnutls_alert_send(session: gnutls_session_t, level: c_int, desc: c_int) -> c_int;
    fn gnutls_server_name_set(
        session: gnutls_session_t,
        type_: c_int,
        name: *const c_void,
        name_length: usize,
    ) -> c_int;
    fn gnutls_handshake(session: gnutls_session_t) -> c_int;
    fn gnutls_record_get_direction(session: gnutls_session_t) -> c_int;
    fn gnutls_error_to_alert(err: c_int, level: *mut c_int) -> c_int;
    fn gnutls_certificate_verify_peers3(
        session: gnutls_session_t,
        hostname: *const c_char,
        status: *mut c_uint,
    ) -> c_int;
    fn gnutls_certificate_type_get(session: gnutls_session_t) -> c_int;
    fn gnutls_record_check_pending(session: gnutls_session_t) -> usize;
    fn gnutls_record_recv(session: gnutls_session_t, data: *mut c_void, data_size: usize) -> isize;
    fn gnutls_record_send(
        session: gnutls_session_t,
        data: *const c_void,
        data_size: usize,
    ) -> isize;
    fn gnutls_record_get_max_size(session: gnutls_session_t) -> usize;
    fn gnutls_transport_set_errno(session: gnutls_session_t, err: c_int);
    fn gnutls_bye(session: gnutls_session_t, how: c_int) -> c_int;
    fn gnutls_session_is_resumed(session: gnutls_session_t) -> c_int;
    fn gnutls_session_get_flags(session: gnutls_session_t) -> c_uint;
    fn gnutls_session_get_data2(session: gnutls_session_t, data: *mut GnutlsDatumT) -> c_int;
    fn gnutls_x509_crt_get_subject_alt_name(
        cert: gnutls_x509_crt_t,
        seq: c_uint,
        san: *mut c_void,
        san_size: *mut usize,
        critical: *mut c_uint,
    ) -> c_int;
    fn gnutls_certificate_get_peers(
        session: gnutls_session_t,
        list_size: *mut c_uint,
    ) -> *const GnutlsDatumT;
    fn gnutls_x509_crt_get_dn(
        cert: gnutls_x509_crt_t,
        buf: *mut c_char,
        buf_size: *mut usize,
    ) -> c_int;
    fn gnutls_x509_crt_get_issuer_dn(
        cert: gnutls_x509_crt_t,
        buf: *mut c_char,
        buf_size: *mut usize,
    ) -> c_int;
}

/// The role of a session, with the GnuTLS values.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionKind {
    /// The client side (`GNUTLS_CLIENT`).
    CLIENT = GNUTLS_CLIENT,
    /// The server side (`GNUTLS_SERVER`).
    SERVER = GNUTLS_SERVER,
}

/// The buffers a session reads and writes.
type BufType = TemporaryBuffer<u8>;

/// A peer certificate, freed on drop.
struct X509CrtPtr(gnutls_x509_crt_t);

impl Drop for X509CrtPtr {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: made by gnutls_x509_crt_init.
            unsafe { gnutls_x509_crt_deinit(self.0) };
        }
    }
}

/// A TLS session: wraps a GnuTLS session and carries the encrypted data flow of one
/// connection.
///
/// The session does its I/O through a [`ConnectedSocket`] and that socket's sink and
/// source, which it owns because it drives the handshake and shutdown itself.
pub struct Session {
    /// The session's own address with the provenance of its `LwSharedPtr` node, written
    /// once by `create`. GnuTLS's transport pointer,
    /// `shared_from_this` and every continuation derive from it, never from a reference.
    _this: *mut Session,
    _type: SessionKind,

    _sock: Box<dyn ConnectedSocketImpl>,
    _creds: SharedPtr<CertificateCredentialsImpl>,
    _in: DataSource,
    _out: DataSink,

    _in_sem: Semaphore,
    _out_sem: Semaphore,

    _options: TlsOptions,

    _eof: bool,
    _shutdown: bool,
    _connected: bool,
    _error: Option<ExceptionPtr>,

    _output_pending: Future<()>,
    _input: BufType,

    /// `_options.server_name` as the C string `gnutls_certificate_verify_peers3` takes, made
    /// once.
    _server_name_c: CString,

    // modify this to a unique_ptr to handle exceptions in our constructor.
    _session: gnutls_session_t,
}

type SessionPtr = LwSharedPtr<Session>;

impl EnableLwSharedFromThis for Session {}

impl Session {
    /// A session of role `t` over `sock` with `creds` and `options`, made inside its
    /// `LwSharedPtr` because GnuTLS keeps a pointer to it.
    ///
    /// # Errors
    ///
    /// A [`TlsError`] when GnuTLS cannot set the session up, or an error for a server name
    /// with a NUL byte.
    pub fn create(
        t: SessionKind,
        creds: &CertificateCredentials,
        mut sock: Box<dyn ConnectedSocketImpl>,
        options: TlsOptions,
    ) -> Result<SessionPtr, ExceptionPtr> {
        // A server name with a NUL inside is rejected rather than verified against its
        // truncation.
        let server_name_c = CString::new(options.server_name.as_bytes()).map_err(|_| {
            ExceptionPtr::from(InvalidArgument("server name contains a NUL byte".to_string()))
        })?;
        let mut gs: gnutls_session_t = std::ptr::null_mut();
        // SAFETY: plain init call.
        gtls_chk(unsafe { gnutls_init(&mut gs, GNUTLS_NONBLOCK | t as u32) })?;
        let in_ = sock.source();
        let out = sock.sink();
        let s = make_lw_shared(Session {
            _this: std::ptr::null_mut(),
            _type: t,
            _sock: sock,
            _creds: creds._impl.clone(),
            _in: in_,
            _out: out,
            _in_sem: Semaphore::new(1),
            _out_sem: Semaphore::new(1),
            _options: options,
            _eof: false,
            _shutdown: false,
            _connected: false,
            _error: None,
            _output_pending: make_ready_future(()),
            _input: BufType::empty_buffer(),
            _server_name_c: server_name_c,
            _session: gs,
        });
        let root: *mut Session = s.get();
        // SAFETY: the session was just made; this is the only reference to it, and gnutls is
        // only configured below (no call here reaches the transport callbacks). `root` is
        // the node's root pointer.
        let me = unsafe {
            (*root)._this = root;
            &mut *root
        };
        // SAFETY: `gs` is a valid session; the credentials outlive it (the session
        // holds a reference); `me` is the session's final address.
        unsafe {
            gtls_chk(gnutls_set_default_priority(gs))?;
            // `(*me._creds).get()`: the gnutls handle of the credentials, not `SharedPtr::get`.
            gtls_chk(gnutls_credentials_set(gs, GNUTLS_CRD_CERTIFICATE, (*me._creds).get()))?;
            if me._type == SessionKind::SERVER {
                match me._creds.get_client_auth() {
                    ClientAuth::NONE => {
                        gnutls_certificate_server_set_request(gs, GNUTLS_CERT_IGNORE)
                    }
                    ClientAuth::REQUEST => {
                        gnutls_certificate_server_set_request(gs, GNUTLS_CERT_REQUEST)
                    }
                    ClientAuth::REQUIRE => {
                        gnutls_certificate_server_set_request(gs, GNUTLS_CERT_REQUIRE)
                    }
                }
                // Maybe set up server session ticket support
                match me._creds.get_session_resume_mode() {
                    SessionResumeMode::NONE => {}
                    SessionResumeMode::TLS13_SESSION_TICKET => {
                        gnutls_session_ticket_enable_server(gs, me._creds.get_session_resume_key());
                    }
                }
            }

            let prio = me._creds.get_priority();
            if !prio.is_null() {
                gtls_chk(gnutls_priority_set(gs, prio))?;
            }

            // The root pointer, not one from `me`: gnutls uses it after this call.
            gnutls_transport_set_ptr(gs, root as gnutls_transport_ptr_t);
            gnutls_transport_set_vec_push_function(gs, Session::vec_push_wrapper);
            gnutls_transport_set_pull_function(gs, Session::pull_wrapper);

            // This would be nice, because we preferably want verification to
            // abort hand shake so peer immediately knows we bailed...
            if me._type == SessionKind::CLIENT {
                gnutls_session_set_verify_function(gs, Session::verify_wrapper);
            }
            // if we are a client, check if we have a session ticket to unpack.
            if me._type == SessionKind::CLIENT && !me._options.session_resume_data.is_empty() {
                gtls_chk(gnutls_session_set_data(
                    gs,
                    me._options.session_resume_data.as_ptr() as *const c_void,
                    me._options.session_resume_data.len(),
                ))?;
            }
        }
        me._options.session_resume_data.clear(); // no need to keep around
        Ok(s)
    }

    /// As [`Session::create`], over the implementation of `sock`.
    ///
    /// # Errors
    ///
    /// As [`Session::create`]; also when `sock` has no implementation.
    pub fn create_from_socket(
        t: SessionKind,
        creds: &CertificateCredentials,
        sock: ConnectedSocket,
        options: TlsOptions,
    ) -> Result<SessionPtr, ExceptionPtr> {
        let Some(impl_) = GetImpl::get(sock) else {
            return Err(ExceptionPtr::from(std::io::Error::from_raw_os_error(ENOTCONN)));
        };
        Self::create(t, creds, impl_, options)
    }

    #[inline]
    fn gs(&self) -> gnutls_session_t {
        self._session
    }

    fn me(&self) -> SessionPtr {
        // SAFETY: every session is made by make_lw_shared in `create`, and `_this` is the
        // node's root pointer; only the counter before the session is touched.
        unsafe { Self::shared_from_this(self._this) }
    }

    /// `cert_status_to_string`: does not touch the session.
    fn cert_status_to_string(type_: c_int, status: c_uint) -> Result<Sstring, ExceptionPtr> {
        let mut out = GnutlsDatumT { data: std::ptr::null_mut(), size: 0 };
        // SAFETY: `out` receives a gnutls-allocated string, freed below.
        gtls_chk(unsafe {
            gnutls_certificate_verification_status_print(status, type_, &mut out, 0)
        })?;
        // SAFETY: `out` holds `size` bytes.
        let s = unsafe { std::slice::from_raw_parts(out.data, out.size as usize) };
        let r = Sstring::from(String::from_utf8_lossy(s).as_ref());
        // SAFETY: allocated by gnutls; the global `gnutls_free` always holds its deallocator.
        unsafe { gnutls_free(out.data as *mut c_void) };
        Ok(r)
    }

    // Aliasing. gnutls calls back into the session (`pull`, `vec_push`, `verify`) from inside
    // gnutls_handshake, gnutls_record_recv/send, gnutls_alert_send and gnutls_bye, through the
    // transport pointer. Every method that reaches one of
    // those calls takes `this: *mut Session` and holds no reference to the session across it
    // (only short field accesses `(*this).field` before and after); the callbacks touch only
    // the fields they need, through the same pointer. Futures and continuations capture
    // `this`; the session outlives them because callers keep a `session_ref`.

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn send_alert(this: *mut Session, level: c_int, desc: c_int) -> Future<()> {
        repeat(move || {
            // SAFETY: guaranteed by the caller of send_alert, for each iteration; no reference
            // to the session is live across gnutls_alert_send (it may call vec_push).
            unsafe {
                let res = gnutls_alert_send((*this)._session, level, desc);
                match res {
                    GNUTLS_E_SUCCESS => Session::wait_for_output(this)
                        .then(|()| make_ready_future(StopIteration::YES)),
                    GNUTLS_E_AGAIN | GNUTLS_E_INTERRUPTED => Session::wait_for_output(this)
                        .then(|()| make_ready_future(StopIteration::NO)),
                    _ => Session::handle_output_error(this, res)
                        .then(|()| make_ready_future(StopIteration::YES)),
                }
            }
        })
    }

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn do_handshake(this: *mut Session) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call). See the aliasing note above.
        unsafe {
            if (*this)._connected {
                return make_ready_future(());
            }
            if (*this)._type == SessionKind::CLIENT && !(*this)._options.server_name.empty() {
                let name = &(*this)._options.server_name;
                // the name outlives the call; gnutls copies it.
                gnutls_server_name_set(
                    (*this)._session,
                    GNUTLS_NAME_DNS,
                    name.as_bytes().as_ptr() as *const c_void,
                    name.size(),
                );
            }
            let res = gnutls_handshake((*this)._session);
            if res < 0 {
                match res {
                    GNUTLS_E_AGAIN => {
                        // #453 always wait for output first.
                        // If none is pending, it should be a no-op
                        let dir = gnutls_record_get_direction((*this)._session);
                        return Session::wait_for_output(this).then(move |()| {
                            // we actually E_AGAIN:ed in a write. Don't
                            // wait for input.
                            if dir == 1 {
                                return Session::do_handshake(this);
                            }
                            Session::wait_for_input(this)
                                .then(move |()| Session::do_handshake(this))
                        });
                    }
                    GNUTLS_E_NO_CERTIFICATE_FOUND => {
                        return make_exception_future(ExceptionPtr::from(VerificationError(
                            "No certificate was found".to_string(),
                        )));
                    }
                    _ => {
                        if res == GNUTLS_E_CERTIFICATE_ERROR
                            && let Err(e) = Session::verify(this)
                        {
                            // a verification failure is the error; otherwise fall through
                            return make_exception_future(e);
                        }
                        // Send the handshake error returned by gnutls_handshake()
                        // to the client, as an alert. For example, if the protocol
                        // version requested by the client is not supported, the
                        // "protocol_version" alert is sent.
                        let alert = gnutls_error_to_alert(res, std::ptr::null_mut());
                        return Session::handle_output_error(this, res).then_wrapped(
                            move |output_future: Future<()>| {
                                Session::send_alert(this, GNUTLS_AL_FATAL, alert).then_wrapped(
                                    move |mut f: Future<()>| {
                                        // Return to the caller the handshake error itself.
                                        // If send_alert() *also* failed, ignore that.
                                        f.ignore_ready_future();
                                        output_future
                                    },
                                )
                            },
                        );
                    }
                }
            }
            if ((*this)._type == SessionKind::CLIENT
                || (*this)._creds.get_client_auth() != ClientAuth::NONE)
                && let Err(e) = Session::verify(this)
            {
                return make_exception_future(e);
            }
            (*this)._connected = true;
            // make sure we reset output_pending
            Session::wait_for_output(this)
        }
    }

    /// Runs the handshake (loading system trust first if needed).
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn handshake(this: *mut Session) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call). The semaphores are never
        // touched by the gnutls callbacks, so the `&mut` to them that with_semaphore holds does not
        // overlap.
        unsafe {
            // maybe load system certificates before handshake, in case we
            // have not done so yet...
            if (*this)._creds.need_load_system_trust() {
                // the credentials are a different object, shared by sessions of this shard.
                return CertificateCredentialsImpl::maybe_load_system_trust(shared_root(
                    &(*this)._creds,
                ))
                .then(move |()| Session::handshake(this));
            }
            // acquire both semaphores to sync both read & write
            with_semaphore(&mut (*this)._in_sem, 1, move || {
                with_semaphore(&mut (*this)._out_sem, 1, move || {
                    Session::do_handshake(this).handle_exception(move |ep: ExceptionPtr| {
                        let e = (*this)._error.get_or_insert(ep).clone();
                        make_exception_future(e)
                    })
                })
            })
        }
    }

    #[inline]
    fn in_avail(&self) -> usize {
        self._input.size()
    }

    #[inline]
    fn eof(&self) -> bool {
        self._eof
    }

    /// # Safety
    /// `this` is the session's root pointer, live on this shard, with no reference to it
    /// live: the continuations may run in line.
    unsafe fn wait_for_input(this: *mut Session) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is the session's
        // root pointer, live on this shard, with no reference to it live: the continuations may run
        // in line). Each access is short.
        if unsafe { !(*this)._input.empty() } {
            return make_ready_future(());
        }
        // SAFETY: as above; the borrow of `_in` ends when `get` returns the future.
        let f = unsafe { (*this)._in.get() };
        f.then(move |buf: BufType| {
            // SAFETY: the session outlives its operations (callers keep a session_ref); the
            // continuation runs from the reactor with no other reference live.
            unsafe {
                (*this)._eof |= buf.empty();
                (*this)._input = buf;
            }
            make_ready_future(())
        })
        .handle_exception(move |ep: ExceptionPtr| {
            // SAFETY: as above.
            unsafe { (*this)._error = Some(ep.clone()) };
            make_exception_future(ep)
        })
    }

    /// # Safety
    /// As for `wait_for_input`.
    unsafe fn wait_for_output(this: *mut Session) -> Future<()> {
        // SAFETY: guaranteed by the caller; a short access.
        let f = unsafe { std::mem::replace(&mut (*this)._output_pending, make_ready_future(())) };
        f.handle_exception(move |ep: ExceptionPtr| {
            // SAFETY: see wait_for_input.
            unsafe { (*this)._error = Some(ep.clone()) };
            make_exception_future(ep)
        })
    }

    #[inline]
    fn from_transport_ptr(ptr: gnutls_transport_ptr_t) -> *mut Session {
        ptr as *mut Session
    }

    unsafe extern "C" fn verify_wrapper(gs: gnutls_session_t) -> c_int {
        // SAFETY: gnutls hands back the pointer set in `create`; the method that called gnutls
        // holds no reference to the session (see the aliasing note).
        match unsafe { Session::verify(Self::from_transport_ptr(gnutls_transport_get_ptr(gs))) } {
            Ok(()) => 0,
            Err(_) => GNUTLS_E_CERTIFICATE_ERROR,
        }
    }

    unsafe extern "C" fn vec_push_wrapper(
        ptr: gnutls_transport_ptr_t,
        iov: *const Giovec,
        iovcnt: c_int,
    ) -> isize {
        // SAFETY: as in verify_wrapper.
        unsafe { Session::vec_push(Self::from_transport_ptr(ptr), iov, iovcnt) }
    }

    unsafe extern "C" fn pull_wrapper(
        ptr: gnutls_transport_ptr_t,
        dst: *mut c_void,
        len: usize,
    ) -> isize {
        // SAFETY: as in verify_wrapper.
        unsafe { Session::pull(Self::from_transport_ptr(ptr), dst, len) }
    }

    /// # Safety
    /// `this` is a live session; no `&mut` to it is live during the call (it runs from the
    /// verify callback, inside gnutls_handshake).
    unsafe fn verify(this: *mut Session) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session;
        // no `&mut` to it is live during the call (it runs from the verify callback, inside
        // gnutls_handshake)). Only fields that the caller of gnutls does not borrow (`_creds`,
        // `_options`, `_type`, `_session`) are read.
        unsafe {
            let creds = &(*this)._creds;
            if !creds._enable_certificate_verification {
                return Ok(());
            }
            let gs = (*this)._session;
            let type_ = (*this)._type;

            let mut status: c_uint = 0;
            let hostname = if type_ != SessionKind::CLIENT || (*this)._options.server_name.empty() {
                std::ptr::null()
            } else {
                // NUL-terminated and checked for interior NULs when the options were set.
                (*this)._server_name_c.as_ptr()
            };
            let res = gnutls_certificate_verify_peers3(gs, hostname, &mut status);
            if res == GNUTLS_E_NO_CERTIFICATE_FOUND
                && type_ != SessionKind::CLIENT
                && creds.get_client_auth() != ClientAuth::REQUIRE
            {
                return Ok(());
            }
            if res < 0 {
                return Err(ExceptionPtr::from(TlsError { code: res, what: String::new() }));
            }
            if status & GNUTLS_CERT_INVALID != 0 {
                let mut stat_str =
                    Session::cert_status_to_string(gnutls_certificate_type_get(gs), status)?;
                let dn = Session::extract_dn_information_of(gs)?;

                // If possible, include issuer/subject info on the cert that failed verification.
                if let Some(dn) = dn {
                    let mut ss = String::from(stat_str.as_str());
                    if !ss.ends_with(' ') {
                        ss.push(' ');
                    }
                    ss.push_str(&format!("(Issuer=[{}], Subject=[{}])", dn.issuer, dn.subject));
                    stat_str = Sstring::from(ss.as_str());
                }
                return Err(ExceptionPtr::from(VerificationError(stat_str.to_string())));
            }
            if creds._dn_callback.is_some() {
                // if the user registered a DN (Distinguished Name) callback
                // then extract subject and issuer from the (leaf) peer certificate and invoke the callback

                // otherwise we couldn't have gotten here
                let Some(dn) = Session::extract_dn_information_of(gs)? else {
                    return Err(ExceptionPtr::from(LogicError(
                        "verified peer without a certificate".to_string(),
                    )));
                };

                // a switch here might look overelaborate, however,
                // the compiler will warn us if someone alters the definition of type
                let t = match type_ {
                    SessionKind::CLIENT => SessionType::CLIENT,
                    SessionKind::SERVER => SessionType::SERVER,
                };

                // The credentials are another object, shared by the sessions of this shard;
                // only this call touches them now.
                if let Some(cb) = creds.get_mut_unchecked()._dn_callback.as_mut() {
                    // A failure of the callback fails the verification and the handshake.
                    cb.call(t, dn.subject, dn.issuer)?;
                }
            }
            Ok(())
        }
    }

    /// Reads decrypted data.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn get(this: *mut Session) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call).
        unsafe {
            if let Some(e) = &(*this)._error {
                return make_exception_future(e.clone());
            }
            if (*this)._shutdown || (*this).eof() {
                return make_ready_future(TemporaryBuffer::<u8>::empty_buffer());
            }
            if !(*this)._connected {
                return Session::handshake(this).then(move |()| Session::get(this));
            }
            with_semaphore(&mut (*this)._in_sem, 1, move || Session::do_get(this)).then(
                move |buf: TemporaryBuffer<u8>| {
                    if buf.empty() && !(*this).eof() {
                        // this must mean we got a re-handshake request.
                        // see do_get.
                        // We there clear connected flag and return empty in case
                        // other side requests re-handshake. Now, someone else could have already dealt with it by the
                        // time we are here (continuation reordering). In fact, someone could have dealt with
                        // it and set the eof flag also, but in that case we're still eof...
                        return Session::handshake(this).then(move |()| Session::get(this));
                    }
                    make_ready_future(buf)
                },
            )
        }
    }

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn do_get(this: *mut Session) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call). No reference to the session
        // is live across gnutls_record_recv (it calls pull, and may call vec_push).
        unsafe {
            // gnutls might have stuff in its buffers.
            let mut avail = gnutls_record_check_pending((*this)._session);
            if avail == 0 {
                // or we might...
                avail = (*this).in_avail();
            }
            if avail != 0 {
                // typically, unencrypted data can get smaller (padding),
                // but not larger.
                let mut buf = TemporaryBuffer::<u8>::new(avail);
                // `buf` has `avail` writable bytes.
                let n = gnutls_record_recv(
                    (*this)._session,
                    buf.get_write() as *mut c_void,
                    buf.size(),
                );
                if n < 0 {
                    match n as c_int {
                        GNUTLS_E_AGAIN => {
                            // Assume we got this because we read to little underlying
                            // data to finish a tls packet
                            // Our input buffer should be empty now, so just go again
                            return Session::do_get(this);
                        }
                        GNUTLS_E_REHANDSHAKE => {
                            // server requests new HS. must release semaphore, so set new state
                            // and return nada.
                            (*this)._connected = false;
                            return make_ready_future(TemporaryBuffer::<u8>::empty_buffer());
                        }
                        _ => {
                            let e = ExceptionPtr::from(TlsError {
                                code: n as c_int,
                                what: String::new(),
                            });
                            (*this)._error = Some(e.clone());
                            return make_exception_future(e);
                        }
                    }
                }
                buf.trim(n as usize);
                if n == 0 {
                    (*this)._eof = true;
                }
                return make_ready_future(buf);
            }
            if (*this).eof() {
                return make_ready_future(TemporaryBuffer::<u8>::empty_buffer());
            }
            // No input? wait for out buffers to fill...
            Session::wait_for_input(this).then(move |()| Session::do_get(this))
        }
    }

    /// `do_put(frag_iter i, frag_iter e)`: the fragments `[frags, frags + nr)` of a packet
    /// that the caller keeps alive until the returned future resolves.
    ///
    /// # Safety
    /// As for [`Session::send_alert`], and `frags` points to `nr` fragments that stay valid
    /// and unchanged until the future resolves.
    unsafe fn do_put(this: *mut Session, frags: *const Fragment, nr: usize) -> Future<()> {
        // SAFETY: guaranteed by the caller.
        debug_assert!(unsafe { (*this)._output_pending.available() });
        let mut idx: usize = 0;
        let mut off: usize = 0; // here to appease eclipse cdt
        repeat(move || {
            if idx == nr {
                return make_ready_future(StopIteration::YES);
            }
            // SAFETY: guaranteed by the caller of do_put; `idx < nr`.
            let f = unsafe { *frags.add(idx) };
            if off == f.size {
                idx += 1;
                off = 0;
                return make_ready_future(StopIteration::NO);
            }
            // SAFETY: the fragment has `size` bytes; no reference to the session is live across
            // gnutls_record_send (it calls vec_push).
            unsafe {
                let res = gnutls_record_send(
                    (*this)._session,
                    f.base.add(off) as *const c_void,
                    f.size - off,
                );
                if res > 0 {
                    // don't really need to check, but...
                    off += res as usize;
                }
                // what will we wait for? error or results...
                let w = if res < 0 {
                    Session::handle_output_error(this, res as c_int)
                } else {
                    Session::wait_for_output(this)
                };
                w.then(|()| make_ready_future(StopIteration::NO))
            }
        })
    }

    /// Encrypts and sends `p`.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn put(this: *mut Session, mut p: Packet) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call).
        unsafe {
            if let Some(e) = &(*this)._error {
                return make_exception_future(e.clone());
            }
            if (*this)._shutdown {
                return make_exception_future(ExceptionPtr::from(
                    std::io::Error::from_raw_os_error(EPIPE),
                ));
            }
            if !(*this)._connected {
                return Session::handshake(this).then(move |()| Session::put(this, p));
            }

            // We want to make sure that we call gnutls_record_send with as large
            // packets as possible. This is because each call to gnutls_record_send
            // becomes a sendmsg syscall. Further it results in larger TLS
            // records which makes encryption/decryption faster. Hence to avoid
            // cases where we would do an extra syscall for something like a 100
            // bytes header we linearize the packet if it's below the max TLS record
            // size.
            if p.nr_frags() > 1 && p.len() as usize <= gnutls_record_get_max_size((*this)._session)
            {
                p.linearize_all();
            }

            // `[p.fragments().begin(), p.fragments().end())`: the fragment array lives in the
            // packet's heap block, which does not move when `p` is moved into `finally`.
            let frags = p.fragment_array() as *const Fragment;
            let nr = p.nr_frags() as usize;
            with_semaphore(&mut (*this)._out_sem, 1, move || Session::do_put(this, frags, nr))
                .finally(move || drop(p))
        }
    }

    /// # Safety
    /// `this` is a live session and only gnutls (from inside one of the raw-pointer methods)
    /// calls this; the fields it touches are not borrowed by the caller of gnutls.
    unsafe fn pull(this: *mut Session, dst: *mut c_void, len: usize) -> isize {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // and only gnutls (from inside one of the raw-pointer methods) calls this; the fields it
        // touches are not borrowed by the caller of gnutls). Short accesses to `_eof` and `_input`.
        unsafe {
            if (*this)._eof {
                return 0;
            }
            // If we have data in buffers, we can complete.
            // Otherwise, we must be conservative.
            let input = &mut (*this)._input;
            if input.empty() {
                gnutls_transport_set_errno((*this)._session, EAGAIN);
                return -1;
            }
            let n = std::cmp::min(len, input.size());
            // gnutls passes room for `len` bytes; `_input` holds at least `n`.
            std::ptr::copy_nonoverlapping(input.get(), dst as *mut u8, n);
            input.trim_front(n);
            n as isize
        }
    }

    /// # Safety
    /// As for [`Session::pull`]; touches `_output_pending` and `_out`.
    unsafe fn vec_push(this: *mut Session, iov: *const Giovec, iovcnt: c_int) -> isize {
        // SAFETY: the caller upholds this function's `# Safety` contract (as for [`Session::pull`];
        // touches `_output_pending` and `_out`).
        unsafe {
            let gs = (*this)._session;
            let pending = &mut (*this)._output_pending;
            if !pending.available() {
                gnutls_transport_set_errno(gs, EAGAIN);
                return -1;
            }
            let mut n: isize = 0; // Set on the good path and unused on the bad path

            if !pending.failed() {
                let mut msg = Packet::new();
                for i in 0..iovcnt as usize {
                    // gnutls passes `iovcnt` iovecs.
                    let v = *iov.add(i);
                    // One copy into a heap `TemporaryBuffer` (no other allocation, no
                    // zeroing), skipped when empty.
                    if v.iov_len != 0 {
                        let bytes = std::slice::from_raw_parts(v.iov_base as *const u8, v.iov_len);
                        msg = Packet::append_buffer(msg, TemporaryBuffer::<u8>::copy_from(bytes));
                    }
                }
                n = msg.len() as isize;
                *pending = (*this)._out.put(msg);
            }
            let pending = &mut (*this)._output_pending;
            if pending.failed() {
                // exception is copied back into _output_pending
                // by the catch handlers below
                // `get_exception()` moves the exception out; the handlers below set the future again.
                let e = std::mem::replace(pending, make_ready_future(())).get_exception();
                // A system error of any category counts; every system error of this crate is
                // an `io::Error` or a `SystemError`.
                let code = e
                    .downcast_ref::<std::io::Error>()
                    .and_then(|x| x.raw_os_error())
                    .or_else(|| e.downcast_ref::<SystemError>().map(SystemError::code))
                    .unwrap_or(EIO);
                gnutls_transport_set_errno(gs, code);
                *pending = make_exception_future(e);
                return -1;
            }
            n
        }
    }

    /// # Safety
    /// As for `wait_for_input`.
    unsafe fn handle_output_error(this: *mut Session, res: c_int) -> Future<()> {
        let e = ExceptionPtr::from(TlsError { code: res, what: String::new() });
        // SAFETY: guaranteed by the caller; a short access.
        unsafe { (*this)._error = Some(e.clone()) };
        // #453
        // defensively wait for output before generating the error.
        // if we have both error code and an exception in output
        // future, return both.
        // SAFETY: as above.
        unsafe { Session::wait_for_output(this) }.then_wrapped(move |f: Future<()>| match f.get() {
            // output was ok/done, just generate error code exception
            Ok(()) => make_exception_future(e),
            Err(inner) => make_exception_future(ExceptionPtr::with_nested(
                TlsError { code: res, what: String::new() },
                inner,
            )),
        })
    }

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn do_shutdown(this: *mut Session) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call). Nothing borrowed across
        // gnutls_bye.
        unsafe {
            if (*this)._error.is_some() || !(*this)._connected {
                return make_ready_future(());
            }
            let res = gnutls_bye((*this)._session, GNUTLS_SHUT_WR);
            if res < 0 {
                match res {
                    GNUTLS_E_AGAIN => {
                        // We only send "bye" alert, letting a "normal" (either pending, or subsequent)
                        // read deal with reading the expected EOF alert.
                        debug_assert!(gnutls_record_get_direction((*this)._session) == 1);
                        return Session::wait_for_output(this)
                            .then(move |()| Session::do_shutdown(this));
                    }
                    _ => return Session::handle_output_error(this, res),
                }
            }
            Session::wait_for_output(this)
        }
    }

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn wait_for_eof(this: *mut Session) -> Future<()> {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call).
        unsafe {
            if !(*this)._options.wait_for_eof_on_shutdown {
                return make_ready_future(());
            }

            // read records until we get an eof alert
            // since this call could time out, we must not ac
            with_semaphore(&mut (*this)._in_sem, 1, move || {
                if (*this)._error.is_some() || !(*this)._connected {
                    return make_ready_future(());
                }
                repeat(move || {
                    if (*this).eof() {
                        return make_ready_future(StopIteration::YES);
                    }
                    Session::do_get(this).then(|_buf| make_ready_future(StopIteration::NO))
                })
            })
        }
    }

    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    unsafe fn shutdown(this: *mut Session) -> Future<()> {
        // first, make sure any pending write is done.
        // bye handshake is a flush operation, but this
        // allows us to not pay extra attention to output state
        //
        // we only send a simple "bye" alert packet. Then we
        // read from input until we see EOF. Any other reader
        // before us will get it instead of us, and mark _eof = true
        // in which case we will be no-op.
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call).
        unsafe {
            let me = (*this).me();
            with_semaphore(&mut (*this)._out_sem, 1, move || Session::do_shutdown(this))
                .then(move |()| Session::wait_for_eof(this))
                .finally(move || drop(me))
        }
        // note moved finally clause above. It is theorethically possible
        // that we could complete do_shutdown just before the close calls
        // below, get pre-empted, have "close()" finish, get freed, and
        // then call wait_for_eof on stale pointer.
    }

    /// Closes the session in the background: bye handshake, then (after at most 10s) the
    /// underlying streams.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn close(this: *mut Session) {
        // SAFETY: the caller upholds this function's `# Safety` contract (`this` is a live session
        // on this shard; no reference to it is live during the call).
        unsafe {
            // only do once.
            if std::mem::replace(&mut (*this)._shutdown, true) {
                return;
            }
            let me = (*this).me();
            // running in background. try to bye-handshake us nicely, but after 10s we forcefully close.
            (*engine()).run_in_background(
                // `timer<>::clock::now() + 10s`
                with_timeout::<SteadyClockType, ()>(
                    SteadyTimePoint(SteadyClockType::now().0 + 10_000_000_000),
                    Session::shutdown(this),
                )
                .finally(move || {
                    // `me` (captured below) keeps the session alive.
                    (*this)._eof = true;
                    let _ = (*this)._in.close().handle_exception(|_| make_ready_future(())); // should wake any waiters
                    let _ = (*this)._out.close().handle_exception(|_| make_ready_future(()));
                    // make sure to wait for handshake attempt to leave semaphores. Must be in same order as
                    // handshake aqcuire, because in worst case, we get here while a reader is attempting
                    // re-handshake.
                    with_semaphore(&mut (*this)._in_sem, 1, move || {
                        with_semaphore(&mut (*this)._out_sem, 1, || make_ready_future(()))
                    })
                })
                .then_wrapped(move |mut f: Future<()>| {
                    // must keep object alive until here.
                    drop(me);
                    f.ignore_ready_future();
                    make_ready_future(())
                }),
            );
        }
    }

    // helper for sink
    /// # Safety
    /// As for `wait_for_input`.
    unsafe fn flush(this: *mut Session) -> Future<()> {
        // SAFETY: guaranteed by the caller. The semaphore is a field of the session, which
        // does not move and outlives its pending I/O; the closure (in line or later) touches
        // only `_out`, a field disjoint from the semaphore, through the root pointer.
        unsafe {
            with_semaphore(&mut (*this)._out_sem, 1, move || {
                // SAFETY: the flush of the sink does not enter gnutls; the session outlives it.
                (*this)._out.flush()
            })
        }
    }

    /// The underlying socket.
    pub fn socket(&self) -> &dyn ConnectedSocketImpl {
        &*self._sock
    }

    fn socket_mut(&mut self) -> &mut dyn ConnectedSocketImpl {
        &mut *self._sock
    }

    // helper routine.
    /// # Safety
    /// As for [`Session::send_alert`]; `f` does not call gnutls functions that call back.
    unsafe fn state_checked_access<R: 'static, F>(this: *mut Session, f: F) -> Future<R>
    where
        F: FnMut(&mut Session) -> Result<R, ExceptionPtr> + 'static,
    {
        // SAFETY: the caller upholds this function's `# Safety` contract (as for
        // [`Session::send_alert`]; `f` does not call gnutls functions that call back).
        unsafe {
            if let Some(e) = &(*this)._error {
                return make_exception_future(e.clone());
            }
            if (*this)._shutdown {
                return make_exception_future(ExceptionPtr::from(
                    std::io::Error::from_raw_os_error(ENOTCONN),
                ));
            }
            if !(*this)._connected {
                return Session::handshake(this).then(move |()| {
                    // always recurse, in case malicious api caller does a shutdown while the above handshake is
                    // happening. I.e. misuses the api.
                    Session::state_checked_access(this, f)
                });
            }
            let mut f = f;
            // the only reference to the session while `f` runs.
            futurize_invoke(move || f(&mut *this))
        }
    }

    /// Whether the session was resumed.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn is_resumed(this: *mut Session) -> Future<bool> {
        // SAFETY: guaranteed by the caller; valid session.
        unsafe {
            Session::state_checked_access(this, |s| Ok(gnutls_session_is_resumed(s.gs()) != 0))
        }
    }

    /// The session resume data (empty when none is available).
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn get_session_resume_data(this: *mut Session) -> Future<SessionData> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Session::state_checked_access(this, |s| {
                /*
                 * Session ticket data is not available just because handshake
                 * was done. First off, of course other part must support it,
                 * but we also (mostly?) need to actually transfer data before
                 * the ticket is received.
                 *
                 * Check session flags so we can return no data in the case
                 * none is avail. Gnutls returns a 4-byte "empty marker"
                 * on none avail.
                 */
                // SAFETY: valid session.
                let flags = gnutls_session_get_flags(s.gs());
                if (flags & GNUTLS_SFLAGS_SESSION_TICKET) == 0 {
                    return Ok(SessionData::new());
                }
                let mut tmp = GnutlsDatum::new();
                // SAFETY: `tmp` receives gnutls-allocated data.
                gtls_chk(gnutls_session_get_data2(s.gs(), &mut tmp.0))?;
                // SAFETY: `tmp` holds `size` bytes.
                Ok(std::slice::from_raw_parts(tmp.0.data, tmp.0.size as usize).to_vec())
            })
        }
    }

    /// The DN of the peer's leaf certificate.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn get_distinguished_name(this: *mut Session) -> Future<Option<SessionDn>> {
        // SAFETY: guaranteed by the caller.
        unsafe { Session::state_checked_access(this, |s| s.extract_dn_information()) }
    }

    /// The subject alt names of the peer certificate, of the given types (all if empty).
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn get_alt_name_information(
        this: *mut Session,
        types: HashSet<SubjectAltNameType>,
    ) -> Future<Vec<SubjectAltName>> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Session::state_checked_access(this, move |s| {
                let mut res: Vec<SubjectAltName> = Vec::new();

                let Some(peer) = s.get_peer_certificate()? else {
                    return Ok(res);
                };

                let mut i: c_uint = 0;
                loop {
                    let mut size: usize = 0;

                    // SAFETY: valid certificate; a null buffer queries the size.
                    let mut err = {
                        gnutls_x509_crt_get_subject_alt_name(
                            peer.0,
                            i,
                            std::ptr::null_mut(),
                            &mut size,
                            std::ptr::null_mut(),
                        )
                    };

                    if err == GNUTLS_E_REQUESTED_DATA_NOT_AVAILABLE {
                        break;
                    }
                    if err != GNUTLS_E_SHORT_MEMORY_BUFFER {
                        gtls_chk(err)?; // returns the error
                    }
                    let mut buf: Vec<u8> = vec![0; size];

                    // SAFETY: `buf` has `size` bytes.
                    err = {
                        gnutls_x509_crt_get_subject_alt_name(
                            peer.0,
                            i,
                            buf.as_mut_ptr() as *mut c_void,
                            &mut size,
                            std::ptr::null_mut(),
                        )
                    };
                    if err < 0 {
                        gtls_chk(err)?; // returns the error
                    }
                    i += 1;

                    const _: () =
                        assert!(SubjectAltNameType::dnsname as c_int == GNUTLS_SAN_DNSNAME);
                    const _: () =
                        assert!(SubjectAltNameType::rfc822name as c_int == GNUTLS_SAN_RFC822NAME);
                    const _: () = assert!(SubjectAltNameType::uri as c_int == GNUTLS_SAN_URI);
                    const _: () =
                        assert!(SubjectAltNameType::ipaddress as c_int == GNUTLS_SAN_IPADDRESS);
                    const _: () =
                        assert!(SubjectAltNameType::othername as c_int == GNUTLS_SAN_OTHERNAME);
                    const _: () = assert!(SubjectAltNameType::dn as c_int == GNUTLS_SAN_DN);

                    let type_ = match err {
                        GNUTLS_SAN_DNSNAME => SubjectAltNameType::dnsname,
                        GNUTLS_SAN_RFC822NAME => SubjectAltNameType::rfc822name,
                        GNUTLS_SAN_URI => SubjectAltNameType::uri,
                        GNUTLS_SAN_IPADDRESS => SubjectAltNameType::ipaddress,
                        GNUTLS_SAN_OTHERNAME => SubjectAltNameType::othername,
                        GNUTLS_SAN_DN => SubjectAltNameType::dn,
                        _ => continue,
                    };

                    if !types.is_empty() && !types.contains(&type_) {
                        continue;
                    }

                    let value = match type_ {
                        SubjectAltNameType::ipaddress => {
                            if size == std::mem::size_of::<in_addr>() {
                                // SAFETY: `buf` holds an in_addr.
                                SubjectAltNameValue::Address(InetAddress::from_in_addr({
                                    (buf.as_ptr() as *const in_addr).read_unaligned()
                                }))
                            } else if size == std::mem::size_of::<in6_addr>() {
                                // SAFETY: `buf` holds an in6_addr.
                                SubjectAltNameValue::Address(InetAddress::from_in6_addr(
                                    (buf.as_ptr() as *const in6_addr).read_unaligned(),
                                    InetAddress::INVALID_SCOPE,
                                ))
                            } else {
                                return Err(ExceptionPtr::from(RuntimeError(format!(
                                    "Unexpected size {} for ipaddress alt name value",
                                    size
                                ))));
                            }
                        }
                        _ => {
                            // data we get back is null-terminated.
                            while buf.last() == Some(&0) {
                                buf.pop();
                            }
                            SubjectAltNameValue::String(Sstring::from(
                                String::from_utf8_lossy(&buf).as_ref(),
                            ))
                        }
                    };

                    res.push(SubjectAltName { type_, value });
                }
                Ok(res)
            })
        }
    }

    /// The raw certificate chain of the peer.
    ///
    /// # Safety
    /// `this` is a live session on this shard; no reference to it is live during the call.
    pub unsafe fn get_peer_certificate_chain(this: *mut Session) -> Future<Vec<CertificateData>> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            Session::state_checked_access(this, |s| {
                let mut list_size: c_uint = 0;
                // SAFETY: valid session.
                let client_cert_list = gnutls_certificate_get_peers(s.gs(), &mut list_size);
                let mut res: Vec<CertificateData> = Vec::with_capacity(list_size as usize);
                if !client_cert_list.is_null() {
                    // SAFETY: gnutls returns `list_size` datums.
                    for client_cert in
                        std::slice::from_raw_parts(client_cert_list, list_size as usize)
                    {
                        // SAFETY: each datum holds `size` bytes.
                        res.push(
                            std::slice::from_raw_parts(client_cert.data, client_cert.size as usize)
                                .to_vec(),
                        );
                    }
                }
                Ok(res)
            })
        }
    }

    fn get_peer_certificate(&self) -> Result<Option<X509CrtPtr>, ExceptionPtr> {
        Session::get_peer_certificate_of(self.gs())
    }

    /// `get_peer_certificate` on the gnutls session alone (also used from the verify callback).
    fn get_peer_certificate_of(gs: gnutls_session_t) -> Result<Option<X509CrtPtr>, ExceptionPtr> {
        let mut list_size: c_uint = 0;
        // SAFETY: valid session.
        let client_cert_list = unsafe { gnutls_certificate_get_peers(gs, &mut list_size) };
        if !client_cert_list.is_null() && list_size > 0 {
            let mut peer_leaf_cert: gnutls_x509_crt_t = std::ptr::null_mut();
            // SAFETY: plain init call.
            gtls_chk(unsafe { gnutls_x509_crt_init(&mut peer_leaf_cert) })?;

            let res = X509CrtPtr(peer_leaf_cert);
            // SAFETY: the first datum is the leaf certificate.
            gtls_chk(unsafe {
                gnutls_x509_crt_import(peer_leaf_cert, client_cert_list, GNUTLS_X509_FMT_DER)
            })?;
            return Ok(Some(res));
        }
        Ok(None)
    }

    fn extract_dn_information(&self) -> Result<Option<SessionDn>, ExceptionPtr> {
        Session::extract_dn_information_of(self.gs())
    }

    /// `extract_dn_information` on the gnutls session alone.
    fn extract_dn_information_of(gs: gnutls_session_t) -> Result<Option<SessionDn>, ExceptionPtr> {
        let Some(peer_leaf_cert) = Session::get_peer_certificate_of(gs)? else {
            return Ok(None);
        };
        let c = peer_leaf_cert.0;
        // SAFETY: valid certificate; the helper passes valid buffers.
        let (ec, subject) = get_gtls_string(|b, s| unsafe { gnutls_x509_crt_get_dn(c, b, s) });
        // SAFETY: as above.
        let (ec2, issuer) =
            get_gtls_string(|b, s| unsafe { gnutls_x509_crt_get_issuer_dn(c, b, s) });
        if ec != 0 || ec2 != 0 {
            return Err(ExceptionPtr::from(RuntimeError(
                "error while extracting certificate DN strings".to_string(),
            )));
        }
        Ok(Some(SessionDn { subject, issuer }))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // ENGINE_ASSERT aborts, whatever the panic strategy.
        if !self._output_pending.available() {
            std::process::abort();
        }
        if !self._session.is_null() {
            // SAFETY: made by gnutls_init.
            unsafe { gnutls_deinit(self._session) };
        }
    }
}

/// `session::session_ref`: an owner of a session; the last one closes it.
#[derive(Clone, Default)]
pub struct SessionRef {
    _session: Option<SessionPtr>,
}

impl SessionRef {
    /// An owner of `session`.
    pub fn new(session: SessionPtr) -> Self {
        SessionRef { _session: Some(session) }
    }

    /// The session, for the raw-pointer methods of [`Session`].
    #[inline]
    fn p(&self) -> *mut Session {
        // SAFETY: the refs handed to sockets, sources and sinks always hold a session.
        unsafe { self._session.as_ref().unwrap_unchecked() }.get()
    }

    /// A short-lived reference to the session, for the accessors that do not enter gnutls.
    #[inline]
    fn s(&self) -> &Session {
        // SAFETY: the session is live (see `p`) and on this shard; the callers use the
        // reference for one call that does not reach gnutls, so no mutable one is live.
        unsafe { &*self.p() }
    }

    /// As [`SessionRef::s`], for the accessors that change the socket.
    #[inline]
    fn s_mut(&mut self) -> &mut Session {
        // SAFETY: as in `s`; no other reference to the session is live during the call.
        unsafe { &mut *self.p() }
    }
}

impl Drop for SessionRef {
    fn drop(&mut self) {
        // This is not super pretty. But we take some care to only own sessions
        // through session_ref, and we need to initiate shutdown on "last owner",
        // since we cannot revive the session in destructor.
        if let Some(s) = &self._session
            && s.use_count() == 1
        {
            // SAFETY: the last owner, on this shard; no reference to the session is live.
            unsafe { Session::close(s.get()) };
        }
    }
}

/// A TLS connection.
pub struct TlsConnectedSocketImpl {
    r: SessionRef,
}

impl TlsConnectedSocketImpl {
    /// A socket over `sess`.
    pub fn new(sess: SessionRef) -> Self {
        TlsConnectedSocketImpl { r: sess }
    }

    /// The DN of the peer's leaf certificate.
    pub fn get_distinguished_name(&mut self) -> Future<Option<SessionDn>> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::get_distinguished_name(self.r.p()) }
    }
    /// The subject alt names of the peer certificate.
    pub fn get_alt_name_information(
        &mut self,
        types: HashSet<SubjectAltNameType>,
    ) -> Future<Vec<SubjectAltName>> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::get_alt_name_information(self.r.p(), types) }
    }
    /// The certificate chain of the peer.
    pub fn get_peer_certificate_chain(&mut self) -> Future<Vec<CertificateData>> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::get_peer_certificate_chain(self.r.p()) }
    }
    /// Whether the session was resumed.
    pub fn check_session_is_resumed(&mut self) -> Future<bool> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::is_resumed(self.r.p()) }
    }
    /// The session resume data.
    pub fn get_session_resume_data(&mut self) -> Future<SessionData> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::get_session_resume_data(self.r.p()) }
    }
}

impl ConnectedSocketImpl for TlsConnectedSocketImpl {
    fn source(&mut self) -> DataSource {
        DataSource::new(Box::new(TlsSourceImpl { r: self.r.clone() }))
    }
    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(TlsSinkImpl { r: self.r.clone() }))
    }
    fn shutdown_input(&mut self) {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::close(self.r.p()) };
    }
    fn shutdown_output(&mut self) {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::close(self.r.p()) };
    }
    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self.r.s_mut().socket_mut().set_nodelay(nodelay)
    }
    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self.r.s().socket().get_nodelay()
    }
    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self.r.s_mut().socket_mut().set_keepalive(keepalive)
    }
    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self.r.s().socket().get_keepalive()
    }
    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self.r.s_mut().socket_mut().set_keepalive_parameters(p)
    }
    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self.r.s().socket().get_keepalive_parameters()
    }
    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { self.r.s_mut().socket_mut().set_sockopt(level, optname, data, len) }
    }
    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: the caller's contract.
        unsafe { self.r.s().socket().get_sockopt(level, optname, data, len) }
    }
    fn local_address(&self) -> SocketAddress {
        self.r.s().socket().local_address()
    }
    fn remote_address(&self) -> SocketAddress {
        self.r.s().socket().remote_address()
    }
    fn wait_input_shutdown(&mut self) -> Future<()> {
        self.r.s_mut().socket_mut().wait_input_shutdown()
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

/// The data source of a TLS connected socket: decrypted records from the session.
struct TlsSourceImpl {
    r: SessionRef,
}

impl DataSourceImpl for TlsSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::get(self.r.p()) }
    }
    fn close(&mut self) -> Future<()> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::close(self.r.p()) };
        make_ready_future(())
    }
}

// Note: source/sink, and by extension, the in/out streams
// produced, cannot exist outside the direct life span of
// the connected_socket itself. This is consistent with
// other sockets in engine, though I am than less fond of it...
/// The data sink of a TLS connected socket: data encrypted through the session.
struct TlsSinkImpl {
    r: SessionRef,
}

impl DataSinkImpl for TlsSinkImpl {
    fn flush(&mut self) -> Future<()> {
        // SAFETY: the session's root pointer; no reference to it is live here.
        unsafe { Session::flush(self.r.p()) }
    }
    fn put(&mut self, p: Packet) -> Future<()> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::put(self.r.p(), p) }
    }
    fn close(&mut self) -> Future<()> {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::close(self.r.p()) };
        make_ready_future(())
    }
    fn can_batch_flushes(&self) -> bool {
        true
    }
    fn on_batch_flush_error(&mut self) {
        // SAFETY: the ref holds the session, on this shard; no reference to it is live.
        unsafe { Session::close(self.r.p()) };
    }
}

/// A listening socket that wraps accepted connections in TLS.
pub struct ServerSession {
    _creds: SharedPtr<ServerCredentials>,
    _sock: ServerSocket,
}

impl ServerSession {
    /// Wraps `sock` with `creds`.
    pub fn new(creds: SharedPtr<ServerCredentials>, sock: ServerSocket) -> Self {
        ServerSession { _creds: creds, _sock: sock }
    }
}

impl ServerSocketImpl for ServerSession {
    fn accept(&mut self) -> Future<AcceptResult> {
        // We're not actually doing anything very SSL until we get
        // an actual connection. Then we create a "server" session
        // and wrap it up after handshaking.
        let creds = self._creds.clone();
        self._sock.accept().then(move |ar: AcceptResult| {
            let addr = ar.remote_address;
            wrap_server(creds, ar.connection).then(move |s: ConnectedSocket| {
                make_ready_future::<AcceptResult>(AcceptResult {
                    connection: s,
                    remote_address: addr,
                })
            })
        })
    }
    fn abort_accept(&mut self) {
        self._sock.abort_accept();
    }
    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        self._sock.local_address()
    }
}

/// A client socket that wraps its connection in TLS.
pub struct TlsSocketImpl {
    _cred: Option<SharedPtr<CertificateCredentials>>,
    _options: TlsOptions,
    _socket: Socket,
}

impl TlsSocketImpl {
    /// A socket of the default stack, wrapped with `cred`.
    pub fn new(cred: SharedPtr<CertificateCredentials>, options: TlsOptions) -> Self {
        TlsSocketImpl { _cred: Some(cred), _options: options, _socket: make_socket() }
    }
}

impl SocketImpl for TlsSocketImpl {
    fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        let cred = self._cred.take();
        let options = std::mem::take(&mut self._options);
        self._socket.connect(sa, local, proto).then(move |s: ConnectedSocket| match cred {
            Some(cred) => wrap_client(cred, s, options),
            None => make_exception_future(ExceptionPtr::from(std::io::Error::from_raw_os_error(
                ENOTCONN,
            ))),
        })
    }
    fn set_reuseaddr(&mut self, reuseaddr: bool) -> Result<(), ExceptionPtr> {
        self._socket.set_reuseaddr(reuseaddr)
    }
    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        self._socket.get_reuseaddr()
    }
    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        self._socket.shutdown()
    }
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Connects to `sa` over TLS with the default network stack, expecting the server name
/// `name`.
///
/// Equivalent to [`connect`] with only [`TlsOptions::server_name`] set. The
/// credentials should hold enough trust information to validate the remote certificate.
///
/// # Errors
///
/// The returned future fails if the connection or the session setup fails.
#[deprecated(note = "Use overload with tls_options parameter")]
pub fn connect_with_name(
    cred: SharedPtr<CertificateCredentials>,
    sa: SocketAddress,
    name: Sstring,
) -> Future<ConnectedSocket> {
    let options = TlsOptions { server_name: name, ..Default::default() };
    connect(cred, sa, options)
}

/// `connect(cred, sa, local, name)` (deprecated)
#[deprecated(note = "Use overload with tls_options parameter")]
pub fn connect_local_with_name(
    cred: SharedPtr<CertificateCredentials>,
    sa: SocketAddress,
    local: SocketAddress,
    name: Sstring,
) -> Future<ConnectedSocket> {
    let options = TlsOptions { server_name: name, ..Default::default() };
    connect_local(cred, sa, local, options)
}

/// Connects to `sa` over TLS with the default network stack and the credentials `cred`.
///
/// The credentials should hold enough trust information to validate the remote
/// certificate; `options` holds the rest of the session configuration.
///
/// # Errors
///
/// The returned future fails if the connection or the session setup fails.
pub fn connect(
    cred: SharedPtr<CertificateCredentials>,
    sa: SocketAddress,
    options: TlsOptions,
) -> Future<ConnectedSocket> {
    // SAFETY: engine() is this shard's running reactor; the reference lives for this call only.
    unsafe { (*engine()).connect(sa) }.then(move |s: ConnectedSocket| wrap_client(cred, s, options))
}

/// Connects to `sa` from `local` over TCP and wraps the connection as a TLS client with
/// `options`.
pub fn connect_local(
    cred: SharedPtr<CertificateCredentials>,
    sa: SocketAddress,
    local: SocketAddress,
    options: TlsOptions,
) -> Future<ConnectedSocket> {
    // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
    unsafe { (*engine()).connect_from(sa, local, Transport::TCP) }
        .then(move |s: ConnectedSocket| wrap_client(cred, s, options))
}

/// Creates a [`Socket`] whose connections are TLS client connections expecting the
/// server name `name`.
///
/// Equivalent to [`socket`] with only [`TlsOptions::server_name`] set.
#[deprecated(note = "Use overload with tls_options parameter")]
pub fn socket_with_name(cred: SharedPtr<CertificateCredentials>, name: Sstring) -> Socket {
    let options = TlsOptions { server_name: name, ..Default::default() };
    socket(cred, options)
}

/// Creates a [`Socket`] whose connections are TLS client connections over the default
/// network stack, using the credentials `cred` and the session configuration `options`.
///
/// The credentials should hold enough trust information to validate the remote
/// certificate.
pub fn socket(cred: SharedPtr<CertificateCredentials>, options: TlsOptions) -> Socket {
    Socket::from_impl(Box::new(TlsSocketImpl::new(cred, options)))
}

/// Wraps the existing connection `s` in a TLS client session expecting the server name
/// `name`.
///
/// Equivalent to [`wrap_client`] with only [`TlsOptions::server_name`] set.
///
/// # Errors
///
/// The returned future fails if the session cannot be created.
#[deprecated(note = "Use overload with tls_options parameter")]
pub fn wrap_client_with_name(
    cred: SharedPtr<CertificateCredentials>,
    s: ConnectedSocket,
    name: Sstring,
) -> Future<ConnectedSocket> {
    let options = TlsOptions { server_name: name, ..Default::default() };
    wrap_client(cred, s, options)
}

/// Wraps the existing connection `s` in a TLS client session using the credentials
/// `cred` and the session configuration `options`.
///
/// # Errors
///
/// The returned future fails if the session cannot be created.
pub fn wrap_client(
    cred: SharedPtr<CertificateCredentials>,
    s: ConnectedSocket,
    options: TlsOptions,
) -> Future<ConnectedSocket> {
    match Session::create_from_socket(SessionKind::CLIENT, &cred, s, options) {
        Ok(sp) => {
            let sess = SessionRef::new(sp);
            let sock = ConnectedSocket::from_impl(Box::new(TlsConnectedSocketImpl::new(sess)));
            make_ready_future::<ConnectedSocket>(sock)
        }
        Err(e) => make_exception_future(e),
    }
}

/// Wraps an accepted connection in SSL/TLS as the server.
pub fn wrap_server(
    cred: SharedPtr<ServerCredentials>,
    s: ConnectedSocket,
) -> Future<ConnectedSocket> {
    match Session::create_from_socket(SessionKind::SERVER, &cred.base, s, TlsOptions::default()) {
        Ok(sp) => {
            let sess = SessionRef::new(sp);
            let sock = ConnectedSocket::from_impl(Box::new(TlsConnectedSocketImpl::new(sess)));
            make_ready_future::<ConnectedSocket>(sock)
        }
        Err(e) => make_exception_future(e),
    }
}

/// Listens on `sa` with the default network stack and returns a server socket whose
/// accepted connections are TLS server sessions using `creds`.
///
/// The credentials should hold the server's certificate and, optionally, trust and
/// revocation data.
///
/// # Errors
///
/// Returns an error if listening on `sa` fails.
pub fn listen(
    creds: SharedPtr<ServerCredentials>,
    sa: SocketAddress,
    opts: ListenOptions,
) -> Result<ServerSocket, ExceptionPtr> {
    Ok(listen_wrap(creds, engine_listen(sa, opts)?))
}

// Wraps an existing server socket in SSL
/// Wraps an existing server socket in SSL.
pub fn listen_wrap(creds: SharedPtr<ServerCredentials>, ss: ServerSocket) -> ServerSocket {
    ServerSocket::from_impl(Box::new(ServerSession::new(creds, ss)))
}

fn get_tls_socket(
    socket: &mut ConnectedSocket,
) -> Result<&mut TlsConnectedSocketImpl, ExceptionPtr> {
    let Some(impl_) = GetImpl::maybe_get_ptr(socket) else {
        // the socket is not yet created or moved from
        return Err(ExceptionPtr::from(std::io::Error::from_raw_os_error(ENOTCONN)));
    };
    // SAFETY: the pointer refers to the implementation owned by `socket`.
    let tls_impl = unsafe { &mut *impl_ }
        .as_any_mut()
        .and_then(|a| a.downcast_mut::<TlsConnectedSocketImpl>());
    match tls_impl {
        Some(t) => Ok(t),
        // bad cast here means that we're dealing with wrong socket type
        None => Err(ExceptionPtr::from(InvalidArgument("Not a TLS socket".to_string()))),
    }
}

/// Returns the distinguished name of the leaf certificate the connected peer uses.
///
/// Forces the TLS handshake: if it has not happened yet, it completes before the
/// returned future resolves. The future resolves to `None` when the peer sent no
/// certificate during the handshake.
///
/// # Errors
///
/// The returned future fails if `socket` is not a TLS socket or is not connected.
pub fn get_dn_information(socket: &mut ConnectedSocket) -> Future<Option<SessionDn>> {
    match get_tls_socket(socket) {
        Ok(t) => t.get_distinguished_name(),
        Err(e) => make_exception_future(e),
    }
}

/// Returns the subject alternative name entries of the peer certificate whose type is in
/// `types`, or all of them when `types` is empty.
///
/// The entries come from the peer's authentication certificate; the list is empty when
/// the connection uses no certificate authentication.
///
/// # Errors
///
/// The returned future fails if `socket` is not a TLS socket or is not connected.
pub fn get_alt_name_information(
    socket: &mut ConnectedSocket,
    types: HashSet<SubjectAltNameType>,
) -> Future<Vec<SubjectAltName>> {
    match get_tls_socket(socket) {
        Ok(t) => t.get_alt_name_information(types),
        Err(e) => make_exception_future(e),
    }
}

/// Returns the raw certificate chain the connected peer uses.
///
/// Forces the TLS handshake: if it has not happened yet, it completes before the
/// returned future resolves. The chain is empty when the peer sent no certificate during
/// the handshake.
///
/// # Errors
///
/// The returned future fails if `socket` is not a TLS socket or is not connected.
pub fn get_peer_certificate_chain(socket: &mut ConnectedSocket) -> Future<Vec<CertificateData>> {
    match get_tls_socket(socket) {
        Ok(t) => t.get_peer_certificate_chain(),
        Err(e) => make_exception_future(e),
    }
}

/// Returns whether the connection resumed an earlier TLS session.
///
/// Forces the TLS handshake if it has not happened yet.
///
/// # Errors
///
/// The returned future fails if `socket` is not a TLS socket or is not connected.
pub fn check_session_is_resumed(socket: &mut ConnectedSocket) -> Future<bool> {
    match get_tls_socket(socket) {
        Ok(t) => t.check_session_is_resumed(),
        Err(e) => make_exception_future(e),
    }
}

/// Returns the session resumption data of a connected client socket, to pass in
/// [`TlsOptions::session_resume_data`] on a later connection.
///
/// Forces the TLS handshake if it has not happened yet. The data is empty when no
/// resumption data is available. TLS 1.3 session tickets usually arrive only after some
/// data has been exchanged, so call this shortly before closing the socket rather than
/// right after connecting.
///
/// # Errors
///
/// The returned future fails if `socket` is not a TLS socket or is not connected.
pub fn get_session_resume_data(socket: &mut ConnectedSocket) -> Future<SessionData> {
    match get_tls_socket(socket) {
        Ok(t) => t.get_session_resume_data(),
        Err(e) => make_exception_future(e),
    }
}

// The more common GnuTLS error codes; not an exhaustive list.

/// `GNUTLS_E_UNKNOWN_COMPRESSION_ALGORITHM`
pub const ERROR_UNKNOWN_COMPRESSION_ALGORITHM: i32 = -3;
/// `GNUTLS_E_UNKNOWN_CIPHER_TYPE`
pub const ERROR_UNKNOWN_CIPHER_TYPE: i32 = -6;
/// `GNUTLS_E_INVALID_SESSION`
pub const ERROR_INVALID_SESSION: i32 = GNUTLS_E_INVALID_SESSION;
/// `GNUTLS_E_UNEXPECTED_HANDSHAKE_PACKET`
pub const ERROR_UNEXPECTED_HANDSHAKE_PACKET: i32 = -19;
/// `GNUTLS_E_UNKNOWN_CIPHER_SUITE`
pub const ERROR_UNKNOWN_CIPHER_SUITE: i32 = -21;
/// `GNUTLS_E_UNKNOWN_ALGORITHM`
pub const ERROR_UNKNOWN_ALGORITHM: i32 = -105;
/// `GNUTLS_E_UNSUPPORTED_SIGNATURE_ALGORITHM`
pub const ERROR_UNSUPPORTED_SIGNATURE_ALGORITHM: i32 = -106;
/// `GNUTLS_E_SAFE_RENEGOTIATION_FAILED`
pub const ERROR_SAFE_RENEGOTIATION_FAILED: i32 = -107;
/// `GNUTLS_E_UNSAFE_RENEGOTIATION_DENIED`
pub const ERROR_UNSAFE_RENEGOTIATION_DENIED: i32 = -108;
/// `GNUTLS_E_UNKNOWN_SRP_USERNAME`
pub const ERROR_UNKNOWN_SRP_USERNAME: i32 = -109;
/// `GNUTLS_E_PREMATURE_TERMINATION`
pub const ERROR_PREMATURE_TERMINATION: i32 = GNUTLS_E_PREMATURE_TERMINATION;
/// `GNUTLS_E_PUSH_ERROR`
pub const ERROR_PUSH: i32 = -53;
/// `GNUTLS_E_PULL_ERROR`
pub const ERROR_PULL: i32 = -54;
/// `GNUTLS_E_UNEXPECTED_PACKET`
pub const ERROR_UNEXPECTED_PACKET: i32 = -15;
/// `GNUTLS_E_UNSUPPORTED_VERSION_PACKET`
pub const ERROR_UNSUPPORTED_VERSION: i32 = -8;
/// `GNUTLS_E_NO_CIPHER_SUITES`
pub const ERROR_NO_CIPHER_SUITES: i32 = -87;
/// `GNUTLS_E_DECRYPTION_FAILED`
pub const ERROR_DECRYPTION_FAILED: i32 = -24;
/// `GNUTLS_E_MAC_VERIFY_FAILED`
pub const ERROR_MAC_VERIFY_FAILED: i32 = -100;
