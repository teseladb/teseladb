//! Constants of the virtio split virtqueue and of the virtio network device, as defined by
//! the virtio specification (OASIS VIRTIO 1.x), used by the vhost-based virtio driver in
//! [`crate::engine::net::virtio`].
//!
//! A split virtqueue of `num` entries (a power of two) is one contiguous memory area holding,
//! in order: the descriptor table (16 bytes per descriptor), the available ring (flags,
//! index, `num` descriptor heads and the `used_event` index) and, after padding to the ring
//! alignment, the used ring (flags, index, `num` used elements and the `avail_event` index).
//! Both indices run freely and wrap at 2^16.

/// Descriptor flag: the buffer continues in the descriptor named by its `next` field.
pub const VRING_DESC_F_NEXT: u16 = 1;
/// Descriptor flag: the buffer is device-writable (otherwise device-readable).
pub const VRING_DESC_F_WRITE: u16 = 2;
/// Descriptor flag: the buffer holds a table of indirect descriptors.
pub const VRING_DESC_F_INDIRECT: u16 = 4;

/// Used-ring flag set by the device: the driver need not notify it when adding buffers.
/// Only a hint; the driver still notifies when the device runs out of buffers.
pub const VRING_USED_F_NO_NOTIFY: u16 = 1;
/// Available-ring flag set by the driver: the device need not interrupt it when consuming
/// buffers. Only a hint.
pub const VRING_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// Feature bit: indirect descriptors are supported.
pub const VIRTIO_RING_F_INDIRECT_DESC: u64 = 1 << 28;

/// Feature bit: event-index notification suppression. The driver publishes, after the
/// available ring, the used index at which it wants an interrupt, and the device publishes,
/// after the used ring, the available index at which it wants a notification; the ring
/// flags are then ignored.
pub const VIRTIO_RING_F_EVENT_IDX: u64 = 1 << 29;

/// Net feature bit: the device handles packets with partial checksum (transmit offload).
pub const VIRTIO_NET_F_CSUM: u64 = 1 << 0;
/// Net feature bit: the driver handles packets with partial checksum (receive offload).
pub const VIRTIO_NET_F_GUEST_CSUM: u64 = 1 << 1;
/// Net feature bit: guest offloads can be reconfigured through the control queue.
pub const VIRTIO_NET_F_CTRL_GUEST_OFFLOADS: u64 = 1 << 2;
/// Net feature bit: the device reports its MAC address in its configuration space.
pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;
/// Net feature bit: the driver can receive TCP segmentation offload (TSO) for IPv4.
pub const VIRTIO_NET_F_GUEST_TSO4: u64 = 1 << 7;
/// Net feature bit: the driver can receive TSO for IPv6.
pub const VIRTIO_NET_F_GUEST_TSO6: u64 = 1 << 8;
/// Net feature bit: the driver can receive TSO with ECN.
pub const VIRTIO_NET_F_GUEST_ECN: u64 = 1 << 9;
/// Net feature bit: the driver can receive UDP fragmentation offload (UFO).
pub const VIRTIO_NET_F_GUEST_UFO: u64 = 1 << 10;
/// Net feature bit: the device can perform TSO for IPv4 on transmit.
pub const VIRTIO_NET_F_HOST_TSO4: u64 = 1 << 11;
/// Net feature bit: the device can perform TSO for IPv6 on transmit.
pub const VIRTIO_NET_F_HOST_TSO6: u64 = 1 << 12;
/// Net feature bit: the device can perform TSO with ECN on transmit.
pub const VIRTIO_NET_F_HOST_ECN: u64 = 1 << 13;
/// Net feature bit: the device can perform UFO on transmit.
pub const VIRTIO_NET_F_HOST_UFO: u64 = 1 << 14;
/// Net feature bit: the driver can merge receive buffers (one packet over several buffers).
pub const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
/// Net feature bit: the configuration space has a link status field.
pub const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
/// Net feature bit: a control virtqueue is available.
pub const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;
/// Net feature bit: receive mode (promiscuous, all-multicast) can be set through the
/// control queue.
pub const VIRTIO_NET_F_CTRL_RX: u64 = 1 << 18;
/// Net feature bit: VLAN filtering can be set through the control queue.
pub const VIRTIO_NET_F_CTRL_VLAN: u64 = 1 << 19;
/// Net feature bit: the driver can send gratuitous announcements on request.
pub const VIRTIO_NET_F_GUEST_ANNOUNCE: u64 = 1 << 21;
/// Net feature bit: the device supports several transmit/receive queue pairs.
pub const VIRTIO_NET_F_MQ: u64 = 1 << 22;
/// Net feature bit: the MAC address can be set through the control queue.
pub const VIRTIO_NET_F_CTRL_MAC_ADDR: u64 = 1 << 23;
