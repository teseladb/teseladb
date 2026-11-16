//! Tests of the network device configuration parser: valid configurations by PCI address,
//! by port index and for a single device, and the errors for unknown keys, bad YAML and
//! conflicting or missing settings.

#![cfg(target_os = "linux")]

use std::io::Cursor;

use teseladb::engine::core::future::RuntimeError;
use teseladb::engine::net::config::{ConfigException, parse_config};

fn parse(
    s: &str,
) -> Result<
    std::collections::HashMap<String, teseladb::engine::net::config::DeviceConfig>,
    teseladb::engine::core::future::ExceptionPtr,
> {
    let mut ss = Cursor::new(s.as_bytes().to_vec());
    parse_config(&mut ss)
}

#[test]
fn test_valid_config_with_pci_address() {
    let device_configs = parse(
        "{eth0: {pci-address: 0000:06:00.0, ip: 192.168.100.10, gateway: 192.168.100.1, netmask: \
         255.255.255.0 } , eth1: {pci-address: 0000:06:00.1, dhcp: true } }",
    )
    .expect("parse_config");

    // eth0 tests
    assert!(device_configs.contains_key("eth0"));
    assert_eq!(device_configs["eth0"].hw_cfg.pci_address, "0000:06:00.0");
    assert!(!device_configs["eth0"].ip_cfg.dhcp);
    assert_eq!(device_configs["eth0"].ip_cfg.ip, "192.168.100.10");
    assert_eq!(device_configs["eth0"].ip_cfg.gateway, "192.168.100.1");
    assert_eq!(device_configs["eth0"].ip_cfg.netmask, "255.255.255.0");

    // eth1 tests
    assert!(device_configs.contains_key("eth1"));
    assert_eq!(device_configs["eth1"].hw_cfg.pci_address, "0000:06:00.1");
    assert!(device_configs["eth1"].ip_cfg.dhcp);
    assert_eq!(device_configs["eth1"].ip_cfg.ip, "");
    assert_eq!(device_configs["eth1"].ip_cfg.gateway, "");
    assert_eq!(device_configs["eth1"].ip_cfg.netmask, "");
}

#[test]
fn test_valid_config_with_port_index() {
    let device_configs = parse(
        "{eth0: {port-index: 0, ip: 192.168.100.10, gateway: 192.168.100.1, netmask: \
         255.255.255.0 } , eth1: {port-index: 1, dhcp: true } }",
    )
    .expect("parse_config");

    // eth0 tests
    assert!(device_configs.contains_key("eth0"));
    assert_eq!(device_configs["eth0"].hw_cfg.port_index.expect("port_index"), 0u32);
    assert!(!device_configs["eth0"].ip_cfg.dhcp);
    assert_eq!(device_configs["eth0"].ip_cfg.ip, "192.168.100.10");
    assert_eq!(device_configs["eth0"].ip_cfg.gateway, "192.168.100.1");
    assert_eq!(device_configs["eth0"].ip_cfg.netmask, "255.255.255.0");

    // eth1 tests
    assert!(device_configs.contains_key("eth1"));
    assert_eq!(device_configs["eth1"].hw_cfg.port_index.expect("port_index"), 1u32);
    assert!(device_configs["eth1"].ip_cfg.dhcp);
    assert_eq!(device_configs["eth1"].ip_cfg.ip, "");
    assert_eq!(device_configs["eth1"].ip_cfg.gateway, "");
    assert_eq!(device_configs["eth1"].ip_cfg.netmask, "");
}

#[test]
fn test_valid_config_single_device() {
    let device_configs = parse(
        "eth0: {pci-address: 0000:06:00.0, ip: 192.168.100.10, gateway: 192.168.100.1, netmask: \
         255.255.255.0 }",
    )
    .expect("parse_config");

    // eth0 tests
    assert!(device_configs.contains_key("eth0"));
    assert_eq!(device_configs["eth0"].hw_cfg.pci_address, "0000:06:00.0");
    assert!(!device_configs["eth0"].ip_cfg.dhcp);
    assert_eq!(device_configs["eth0"].ip_cfg.ip, "192.168.100.10");
    assert_eq!(device_configs["eth0"].ip_cfg.gateway, "192.168.100.1");
    assert_eq!(device_configs["eth0"].ip_cfg.netmask, "255.255.255.0");
}

#[test]
fn test_unsupported_key() {
    let e = parse(
        "{eth0: { some_not_supported_tag: xxx, pci-address: 0000:06:00.0, ip: 192.168.100.10, \
         gateway: 192.168.100.1, netmask: 255.255.255.0 } , eth1: {pci-address: 0000:06:00.1, \
         dhcp: true } }",
    )
    .expect_err("config_exception");
    assert!(e.is::<ConfigException>());
}

#[test]
fn test_bad_yaml_syntax_if_thrown() {
    let e = parse("some bad: [ yaml syntax }").expect_err("a RuntimeError");
    // The configuration error is a runtime error.
    assert!(e.is::<RuntimeError>() || e.is::<ConfigException>());
}

#[test]
fn test_pci_address_and_port_index_if_thrown() {
    let e = parse(
        "{eth0: {pci-address: 0000:06:00.0, port-index: 0, ip: 192.168.100.10, gateway: \
         192.168.100.1, netmask: 255.255.255.0 } , eth1: {pci-address: 0000:06:00.1, dhcp: true} \
         }",
    )
    .expect_err("config_exception");
    assert!(e.is::<ConfigException>());
}

#[test]
fn test_dhcp_and_ip_if_thrown() {
    let e = parse(
        "{eth0: {pci-address: 0000:06:00.0, ip: 192.168.100.10, gateway: 192.168.100.1, netmask: \
         255.255.255.0, dhcp: true } , eth1: {pci-address: 0000:06:00.1, dhcp: true} }",
    )
    .expect_err("config_exception");
    assert!(e.is::<ConfigException>());
}

#[test]
fn test_ip_missing_if_thrown() {
    let e = parse(
        "{eth0: {pci-address: 0000:06:00.0, gateway: 192.168.100.1, netmask: 255.255.255.0 } , \
         eth1: {pci-address: 0000:06:00.1, dhcp: true} }",
    )
    .expect_err("config_exception");
    assert!(e.is::<ConfigException>());
}
