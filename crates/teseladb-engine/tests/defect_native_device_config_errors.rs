#![cfg(target_os = "linux")]
//! Creating the native network device from a configuration it cannot use fails with the
//! reason: more than one interface ("only one network interface is supported") or, without
//! DPDK, the new configuration format at all. Before, the errors were built and dropped: the
//! extra interfaces were ignored, and without DPDK the creation failed later with an
//! unrelated "no network device".

use teseladb::engine::core::future::{ExceptionPtr, RuntimeError};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::net::native_stack::{NativeStackOptions, create_native_net_device};
use teseladb::engine_test_case;

fn create_with_config(config: &str) -> Result<(), ExceptionPtr> {
    let mut o = NativeStackOptions::new();
    o.net_config.set_value(config.to_string());
    // The options live for the whole program, as the creation requires.
    create_native_net_device(Box::leak(Box::new(o)))
}

fn message(r: Result<(), ExceptionPtr>) -> String {
    let e = r.expect_err("the creation must fail");
    e.downcast_ref::<RuntimeError>().map(|e| e.0.clone()).unwrap_or_else(|| format!("{e:?}"))
}

engine_test_case!(test_native_device_rejects_two_interfaces, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let m = message(create_with_config(
            "{eth0: {pci-address: 0000:06:00.0, dhcp: true}, \
              eth1: {pci-address: 0000:06:00.1, dhcp: true}}",
        ));
        assert_eq!(m, "only one network interface is supported");
        Ok(())
    })
});

#[cfg(not(feature = "dpdk"))]
engine_test_case!(test_native_device_rejects_the_new_format_without_dpdk, {
    thread_async(|| -> Result<(), ExceptionPtr> {
        let m = message(create_with_config("{eth0: {pci-address: 0000:06:00.0, dhcp: true}}"));
        assert_eq!(m, "only DPDK supports new configuration format");
        Ok(())
    })
});
