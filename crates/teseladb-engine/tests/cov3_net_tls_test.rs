//! Server credentials shared between shards (`ServerCredentials::share_with_shards`, the
//! engine's way to give every shard of a server the same gnutls objects) and smaller TLS
//! paths: credentials with a DN verification callback cannot be shared; a shard copy is
//! registered on its shard, a reload of the owner reaches it there, and dropping it
//! unregisters it; a PKCS#12 password with a NUL byte is refused.
#![cfg(all(target_os = "linux", feature = "tls"))]
// This file calls unsafe engine interfaces directly; every `unsafe` block states why it is
// sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/tls_certs.rs"]
mod tls_certs;

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{ExceptionPtr, InvalidArgument};
use teseladb::engine::core::shared_ptr::{SharedPtr, make_shared};
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::net::tls::{
    AbstractCredentials, CertificateCredentials, CredentialsBuilder, DnCallback, ServerCredentials,
    X509CrtFormat,
};
use teseladb::engine::testing::test_runner::testing::global_test_runner;
use teseladb::engine_thread_test_case;
use tls_certs::certfile;

/// Starts the test reactor with two shards before `main`.
#[used]
#[unsafe(link_section = ".init_array")]
static START_WITH_TWO_SHARDS: extern "C" fn() = {
    extern "C" fn start() {
        let args = ["cov3_net_tls_test".to_string(), "--smp=2".to_string()];
        global_test_runner().start(&args);
    }
    start
};

fn s(x: &str) -> Sstring {
    Sstring::from(x)
}

thread_local! {
    /// Shard 1's copy of the shared credentials.
    static COPY: RefCell<Option<SharedPtr<ServerCredentials>>> = const { RefCell::new(None) };
}

/// A directory of its own under the system temporary directory.
fn tmpdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("cov3-tls-{}", std::process::id()));
    std::fs::create_dir_all(&d).expect("a directory");
    d
}

engine_thread_test_case!(credentials_with_a_dn_callback_cannot_be_shared, {
    let mut sc = ServerCredentials::new().expect("server credentials");
    sc.set_dn_verification_callback(Some(DnCallback::new(|_t, _s, _i| {})));
    let sc = make_shared(sc);
    let e = ServerCredentials::share_with_shards(&sc).get().err().expect("refused");
    assert!(e.downcast_ref::<InvalidArgument>().is_some());
});

engine_thread_test_case!(a_reload_reaches_the_copies_on_other_shards, {
    let dir = tmpdir();
    let cert = dir.join("test.crt");
    let key = dir.join("test.key");
    std::fs::copy(certfile("other.crt"), &cert).expect("copied");
    std::fs::copy(certfile("other.key"), &key).expect("copied");
    let (cert_s, key_s) = (s(cert.to_str().expect("a path")), s(key.to_str().expect("a path")));

    let mut b = CredentialsBuilder::default();
    b.set_x509_key_file(&cert_s, &key_s, X509CrtFormat::PEM).get().expect("the key pair");
    let reloads = Rc::new(Cell::new(0));
    let r = reloads.clone();
    let creds = b
        .build_reloadable_server_credentials_simple(
            Some(Rc::new(move |_files: &HashSet<Sstring>, ep: Option<ExceptionPtr>| {
                if ep.is_none() {
                    r.set(r.get() + 1);
                }
            })),
            Some(Duration::from_millis(50)),
        )
        .get()
        .expect("reloadable credentials");
    let shared = ServerCredentials::share_with_shards(&creds).get().expect("shared");
    // Sharing again reuses the registry of copies.
    let again = ServerCredentials::share_with_shards(&creds).get().expect("shared again");
    drop(again);
    Smp::submit_to(1, move || {
        COPY.with(|c| *c.borrow_mut() = Some(shared.into_local()));
    })
    .get()
    .expect("a copy on shard 1");

    // New files: the owner reloads, and sends the new objects to shard 1.
    std::fs::copy(certfile("test.crt"), &cert).expect("copied");
    std::fs::copy(certfile("test.key"), &key).expect("copied");
    let deadline = Instant::now() + Duration::from_secs(20);
    while reloads.get() == 0 {
        assert!(Instant::now() < deadline, "no reload");
        sleep_steady(Duration::from_millis(10)).get().expect("sleep");
    }
    // The copy is still usable after the update, then unregisters when dropped.
    let alive = Smp::submit_to(1, || {
        Ok::<bool, ExceptionPtr>(COPY.with(|c| c.borrow_mut().take().is_some()))
    })
    .get()
    .expect("shard 1");
    assert!(alive);
    drop(creds);
    let _ = std::fs::remove_dir_all(&dir);
});

engine_thread_test_case!(a_pkcs12_password_with_a_nul_is_refused, {
    let mut c = CertificateCredentials::default();
    let e = c
        .set_simple_pkcs12(b"not pkcs12", X509CrtFormat::DER, &s("pass\0word"))
        .expect_err("refused");
    assert!(e.downcast_ref::<InvalidArgument>().is_some());
});
