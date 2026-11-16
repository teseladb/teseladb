//! Kernel release parsing beyond uname_test (the original's internal/uname.cc): releases with a
//! fourth component, malformed strings (which read as 0.0), the printed form of every shape and
//! the string form of the descendant check. The network configuration parser beyond
//! net_config_test (the original's net/config.cc over yaml-cpp): the hardware flags, ring size
//! in decimal, hex and octal, the YAML boolean spellings, quoting, comments, flow style, bad
//! values, and the consistency checks evaluated over all devices as the original does.
#![cfg(target_os = "linux")]

use std::io::Cursor;

use teseladb::engine::core::uname::parse_uname;
use teseladb::engine::net::config::{ConfigException, parse_config};

#[test]
fn four_component_releases_compare_on_the_last_one() {
    let check = |u: &str| parse_uname(u).whitelisted(&["4.9.8.5"]);
    assert!(check("4.9.8.5"));
    assert!(check("4.9.8.6"));
    assert!(!check("4.9.8.4"));
    assert!(!check("4.9.9.9"), "the third component must match exactly");
    assert!(!check("4.10.8.5"), "the line must be the same");
    assert!(parse_uname("4.9.8.6").same_as_or_descendant_of_str("4.9.8.5"));
    assert!(parse_uname("5.0").same_as_or_descendant_of_str("4.9"));
    assert_eq!(parse_uname("4.9.8.5").component_count(), 4);
}

#[test]
fn malformed_releases_read_as_zero() {
    for bad in ["", "x", "4", "4.", "4.x", ".5", "4.5.6+", "4.5-"] {
        let u = parse_uname(bad);
        assert_eq!((u.version, u.patchlevel), (0, 0), "{bad:?}");
        assert_eq!(u.component_count(), 2, "{bad:?}");
        assert_eq!(u.to_string(), "0.0", "{bad:?}");
    }
    // A release no real kernel matches is not whitelisted by a real line.
    assert!(!parse_uname("garbage").whitelisted(&["3.15"]));
}

#[test]
fn releases_print_as_they_were_written() {
    for s in [
        "5.4",
        "5.4.0",
        "5.4.0.1",
        "3.10.0-957.el7",
        "3.10.0-957.5.1.el7.x86_64",
        "6.1.0-.custom",
    ] {
        assert_eq!(parse_uname(s).to_string(), s);
    }
    let u = parse_uname("3.10.0-957.5.1.el7.x86_64");
    assert_eq!(u.distro_patch, Some(957));
    assert_eq!(u.distro_extra, ".5.1.el7.x86_64");
    assert!(u.has_distro_extra("el7"));
}

fn parse(
    s: &str,
) -> Result<std::collections::HashMap<String, teseladb::engine::net::config::DeviceConfig>, String>
{
    parse_config(&mut Cursor::new(s.as_bytes().to_vec())).map_err(|e| {
        e.downcast_ref::<ConfigException>()
            .map(|c| c.to_string())
            .unwrap_or_else(|| e.to_string())
    })
}

#[test]
fn hardware_flags_ring_sizes_and_boolean_spellings() {
    let cfg = parse(
        "# devices\n\
         eth0:\n  pci-address: \"0000:06:00.0\"  # quoted, with colons\n  ip: 192.168.100.10\n  \
         gateway: 192.168.100.1\n  netmask: 255.255.255.0\n  lro: yes\n  tso: On\n  ufo: TRUE\n  \
         hw-fc: n\n  event-index: off\n  ring-size: 0x100\n",
    )
    .expect("valid");
    let d = &cfg["eth0"];
    assert_eq!(d.hw_cfg.pci_address, "0000:06:00.0");
    assert!(d.hw_cfg.lro && d.hw_cfg.tso && d.hw_cfg.ufo);
    assert!(!d.hw_cfg.hw_fc && !d.hw_cfg.event_index);
    assert_eq!(d.hw_cfg.ring_size, Some(256));
    let cfg = parse("eth0: {ip: 1.1.1.1, gateway: 1.1.1.2, netmask: 255.0.0.0, ring-size: 0o20}\n")
        .expect("flow style");
    assert_eq!(cfg["eth0"].hw_cfg.ring_size, Some(16));
    let cfg = parse("{eth0: {ip: 1.1.1.1, gateway: 1.1.1.2, netmask: 255.0.0.0, ring-size: 512}, eth1: {dhcp: true}}")
        .expect("a flow document");
    assert_eq!(cfg["eth0"].hw_cfg.ring_size, Some(512));
    assert!(cfg["eth1"].ip_cfg.dhcp);
}

#[test]
fn bad_values_and_bad_documents_are_refused() {
    let base = "eth0:\n  ip: 1.1.1.1\n  gateway: 1.1.1.2\n  netmask: 255.0.0.0\n";
    for bad in [
        "  lro: maybe\n",
        "  ring-size: -1\n",
        "  ring-size: 0xzz\n",
        "  tso: 1\n",
    ] {
        assert_eq!(
            parse(&format!("{base}{bad}")).err().as_deref(),
            Some("bad conversion"),
            "{bad:?}"
        );
    }
    assert!(
        parse("  indented: first\n")
            .err()
            .unwrap()
            .contains("unexpected indentation")
    );
    assert!(
        parse("eth0:\n  no-colon-here\n")
            .err()
            .unwrap()
            .contains("invalid mapping entry")
    );
    assert!(parse("eth0: {ip 1.1.1.1}\n").is_err());
    assert!(
        parse("{eth0: {dhcp: true}").is_err(),
        "an unterminated flow document"
    );
    let e = ConfigException::new("the message");
    assert_eq!(e.to_string(), "the message");
}

#[test]
fn consistency_checks_hold_over_all_devices() {
    // Like the original, the static-address check only fails when no device has dhcp off with
    // all three addresses; a document where every device uses dhcp fails it too.
    assert_eq!(
        parse("eth0:\n  dhcp: true\n").err().as_deref(),
        Some("when dhcp is off then all of ip, gateway, netmask has to be specified")
    );
    let ok =
        "eth0:\n  ip: 1.1.1.1\n  gateway: 1.1.1.2\n  netmask: 255.0.0.0\neth1:\n  dhcp: true\n";
    assert!(
        parse(ok).is_ok(),
        "one complete static device satisfies the check"
    );
    // "dhcp and ip" fails only when every device has dhcp on and all addresses; with one
    // complete static device the first check passes and this one decides.
    let both = "eth0:\n  dhcp: false\n  ip: 1.1.1.1\n  gateway: 1.1.1.2\n  netmask: 255.0.0.0\n\
                eth1:\n  dhcp: true\n  ip: 2.2.2.2\n  gateway: 2.2.2.1\n  netmask: 255.0.0.0\n";
    assert!(
        parse(both).is_ok(),
        "not every device mixes dhcp and addresses"
    );
}
