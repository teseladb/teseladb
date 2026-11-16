//! Tests of `DnsResolver`: name and address resolution over UDP and TCP, bad names, timeouts,
//! refused connections, SRV records and parallel lookups. The cases that need the network
//! carry `#[cfg_attr(not(engine_testing_with_networking), ignore)]`.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::sync::LazyLock;
use std::time::Duration;

use teseladb::engine::core::future::{Future, make_exception_future};
use teseladb::engine::core::reactor::engine;
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::when_all::when_all;
use teseladb::engine::net::dns::{DnsResolver, DnsResolverOptions, Hostent, SrvProto, SrvRecords};
use teseladb::engine::net::inet_address::{Family, InetAddress};

static ENGINE_NAME: LazyLock<Sstring> = LazyLock::new(|| Sstring::from("engine.io"));

/// An exception thrown by the body of a future-returning function: its future fails.
macro_rules! try_or_fail {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return make_exception_future(e),
        }
    };
}

// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
fn d_of(d: &LwSharedPtr<DnsResolver>) -> &mut DnsResolver {
    // SAFETY: the resolver is used from one shard, one call at a time.
    unsafe { &mut *d.get() }
}

fn test_resolve(opts: DnsResolverOptions) -> Future<()> {
    let d = make_lw_shared(try_or_fail!(DnsResolver::with_options(&opts)));
    let d1 = d.clone();
    let d2 = d.clone();
    let d3 = d.clone();
    d_of(&d)
        .get_host_by_name(&ENGINE_NAME, Some(Family::INET))
        .then(move |e: Hostent| {
            let a = e.addr_list[0];
            d_of(&d1).get_host_by_addr(&a).then(move |e: Hostent| {
                d_of(&d2).get_host_by_name(&e.names[0], Some(Family::INET)).then(
                    move |e: Hostent| {
                        assert!(e.addr_list.iter().filter(|x| **x == a).count() != 0);
                    },
                )
            })
        })
        .finally(move || d_of(&d3).close())
}

fn test_bad_name(opts: DnsResolverOptions) -> Future<()> {
    let d = make_lw_shared(try_or_fail!(DnsResolver::with_options(&opts)));
    let d2 = d.clone();
    d_of(&d)
        .get_host_by_name(&Sstring::from("apa.ninja.gnu"), Some(Family::INET))
        .then_wrapped(|f: Future<Hostent>| {
            if f.get().is_ok() {
                panic!("should not succeed");
            }
            // ok.
        })
        .finally(move || d_of(&d2).close())
}

/// The `then_wrapped(...).finally(close)` the failure cases share.
fn expect_failure(d: LwSharedPtr<DnsResolver>) -> Future<()> {
    let d2 = d.clone();
    d_of(&d)
        .get_host_by_name(&ENGINE_NAME, Some(Family::INET))
        .then_wrapped(|f: Future<Hostent>| {
            if f.get().is_ok() {
                panic!("should not succeed");
            }
            // ok.
        })
        .finally(move || d_of(&d2).close())
}

fn inet(s: &str) -> InetAddress {
    InetAddress::from_sstring(&Sstring::from(s)).expect("inet_address")
}

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_resolve_udp,
    { test_resolve(DnsResolverOptions::default()) }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_bad_name_udp,
    { test_bad_name(DnsResolverOptions::default()) }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_timeout_udp,
    {
        let opts = DnsResolverOptions {
            servers: Some(vec![inet("1.2.3.4")]), // not a server
            udp_port: Some(29953),                // not a dns port
            timeout: Some(Duration::from_millis(500)),
            ..DnsResolverOptions::default()
        };

        // SAFETY: the reactor of this shard runs the test; the stack outlives the resolver.
        let d = make_lw_shared(try_or_fail!(DnsResolver::with_stack(
            unsafe { (*engine()).net() },
            &opts
        )));
        expect_failure(d)
    }
);

// NOTE: cannot really test timeout in TCP mode, because engine sockets do not support
// connect with timeout -> cannot complete connect future in dns::do_connect in reasonable
// time.

// But we can test for connection refused working as expected.
teseladb::engine_test_case!(test_connection_refused_tcp, {
    let opts = DnsResolverOptions {
        servers: Some(vec![inet("127.0.0.1")]),
        use_tcp_query: Some(true),
        tcp_port: Some(29953), // not a dns port
        ..DnsResolverOptions::default()
    };

    // SAFETY: the reactor of this shard runs the test; the stack outlives the resolver.
    let d =
        make_lw_shared(try_or_fail!(DnsResolver::with_stack(unsafe { (*engine()).net() }, &opts)));
    expect_failure(d)
});

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_resolve_tcp,
    {
        let opts =
            DnsResolverOptions { use_tcp_query: Some(true), ..DnsResolverOptions::default() };
        test_resolve(opts)
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_bad_name_tcp,
    {
        let opts =
            DnsResolverOptions { use_tcp_query: Some(true), ..DnsResolverOptions::default() };
        test_bad_name(opts)
    }
);

static IMAPS_SERVICE: LazyLock<Sstring> = LazyLock::new(|| Sstring::from("imaps"));
static GMAIL_DOMAIN: LazyLock<Sstring> = LazyLock::new(|| Sstring::from("gmail.com"));

fn test_srv() -> Future<()> {
    let d = make_lw_shared(try_or_fail!(DnsResolver::new()));
    let d2 = d.clone();
    d_of(&d)
        .get_srv_records(SrvProto::Tcp, &IMAPS_SERVICE, &GMAIL_DOMAIN)
        .then(|records: SrvRecords| {
            assert!(!records.is_empty());
            for record in &records {
                // record.target should end with "gmail.com"
                assert!(record.target.size() > GMAIL_DOMAIN.size());
                assert!(record.target.as_str().ends_with(GMAIL_DOMAIN.as_str()));
            }
        })
        .finally(move || d_of(&d2).close())
}

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_srv_tcp,
    { test_srv() }
);

fn parallel_resolve(opts: DnsResolverOptions) -> Future<()> {
    let d = make_lw_shared(try_or_fail!(DnsResolver::with_options(&opts)));
    let name = Sstring::from("www.google.com");
    when_all((
        d_of(&d).resolve_name(&name, None),
        d_of(&d).resolve_name(&name, None),
        d_of(&d).resolve_name(&name, None),
        d_of(&d).resolve_name(&name, None),
        d_of(&d).resolve_name(&name, None),
        d_of(&d).resolve_name(&name, None),
    ))
    .finally(move || drop(d))
    .discard_result()
}

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_parallel_resolve_name,
    {
        let opts =
            DnsResolverOptions { use_tcp_query: Some(true), ..DnsResolverOptions::default() };
        parallel_resolve(opts)
    }
);

teseladb::engine_test_case!(
    #[cfg_attr(not(engine_testing_with_networking), ignore)]
    test_parallel_resolve_name_udp,
    { parallel_resolve(DnsResolverOptions::default()) }
);
