//! The access list of a tenant (007 FR-035): addresses and CIDR blocks, IPv4 and IPv6; an empty
//! list lets anyone in and a client without an address no one.

use teseladb_auth::restrictions::sources_allow;

#[test]
fn a_tenant_list_lets_in_its_addresses_and_blocks_only() {
    let list = vec!["10.0.1.0/24".to_owned(), "203.0.113.9".to_owned(), "2001:db8::/32".to_owned()];
    assert!(sources_allow(&list, Some("10.0.1.200:4000")));
    assert!(sources_allow(&list, Some("203.0.113.9:1")));
    assert!(sources_allow(&list, Some("[2001:db8::7]:27017")));
    assert!(!sources_allow(&list, Some("10.0.2.1:4000")));
    assert!(!sources_allow(&list, Some("203.0.113.10:1")));
    assert!(!sources_allow(&list, Some("[2001:db9::1]:1")));
    assert!(!sources_allow(&list, None));
    assert!(!sources_allow(&list, Some("not an address")));
    assert!(sources_allow(&[], None));
    // A block that does not read lets nobody in.
    assert!(!sources_allow(&["10.0.0.0/40".to_owned()], Some("10.0.0.1:1")));
}
