//! Networking: the socket API, the POSIX stack over kernel sockets, the native user-space
//! TCP/IP stack, DNS resolution and TLS.
//!
//! - [`api`] and [`stack`] define the public handles (connected sockets, listeners, datagram
//!   channels, interfaces) and the [`NetworkStack`](api::NetworkStack) each shard owns.
//! - [`posix_stack`] implements them over non-blocking kernel sockets.
//! - The native stack ([`native_stack`], [`net`], [`ip`], [`tcp`], [`udp`], [`arp`],
//!   [`dhcp`], with the [`virtio`] and [`dpdk`] devices) runs TCP/IP in user space, one
//!   queue per shard, steering each flow to the shard that owns it.
//! - [`inet_address`], [`socket_address`], [`socket_defs`] and the wire address types give
//!   the address model; [`dns`] resolves names with c-ares and [`tls`] layers GnuTLS sessions
//!   over connected sockets.
//!
//! Every object belongs to the shard that created it; moving data between shards goes
//! through explicit cross-shard submission.

pub mod api;
pub mod arp;
pub mod byteorder;
pub mod config;
pub mod const_;
pub mod dhcp;
pub mod dns;
pub mod dpdk;
#[cfg(feature = "dpdk")]
pub mod dpdk_rte;
pub mod ethernet;
pub mod hash;
pub mod inet_address;
pub mod ip;
pub mod ip_checksum;
pub mod ipv4_address;
pub mod ipv6_address;
pub mod native_stack;
pub mod native_stack_impl;
// The link layer of the native stack keeps the name `net` inside `net`.
#[allow(clippy::module_inception)]
pub mod net;
pub mod node_ptr;
pub mod packet;
pub mod packet_data_source;
pub mod packet_util;
pub mod posix_stack;
pub mod proxy;
pub mod socket_address;
pub mod socket_defs;
pub mod stack;
pub mod tcp;
pub mod tcp_stack;
pub mod tls;
pub mod toeplitz;
pub mod udp;
pub mod unix_address;
pub mod virtio;
pub mod virtio_interface;
