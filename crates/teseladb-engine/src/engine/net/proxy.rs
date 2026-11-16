//! The proxy device queue used on shards that have no hardware queue of their own.
//!
//! When a NIC has fewer queues than there are shards, the shards without a queue send
//! through a [`ProxyNetDevice`]: it batches outgoing packets and submits them to the shard
//! that owns the real queue, which transmits them and frees each packet back on the
//! sending shard. Received packets reach such a shard through the owning shard's
//! forwarding (see `QpBase`).

use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::future::Future;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::smp::Smp;
use crate::engine::net::net::{Device, Qp, QpBase, QpPtr, qp_init};
use crate::engine::net::node_ptr::{CrossShard, NodePtr};
use crate::engine::net::packet::Packet;

/// A transmit queue that forwards packets to the queue of another shard.
///
/// At most `SEND_QUEUE_LENGTH` (128) packets are in flight to the remote shard at a time.
/// The value lives in a [`NodePtr`] and never moves after [`new`](Self::new).
pub struct ProxyNetDevice {
    /// The queue's own address, with the provenance of its node, written once by `new`;
    /// the pollers and the continuations keep this one.
    _this: *mut ProxyNetDevice,
    base: QpBase,
    _send_depth: usize,
    _cpu: u32,
    _dev: *const dyn Device,
    _moving: Vec<Packet>,
}

impl ProxyNetDevice {
    const SEND_QUEUE_LENGTH: usize = 128;

    /// Creates a proxy queue that forwards to the queue of shard `cpu` of `dev`, in its node
    /// (never moved), and registers it with its root pointer.
    ///
    /// `dev` must outlive the returned queue.
    pub fn new(cpu: u32, dev: *const dyn Device) -> NodePtr<ProxyNetDevice> {
        let node = NodePtr::new(ProxyNetDevice {
            _this: std::ptr::null_mut(),
            base: QpBase::new_default(),
            _send_depth: 0,
            _cpu: cpu,
            _dev: dev,
            _moving: Vec::new(),
        });
        let this = node.as_ptr();
        // SAFETY: just made; nothing else refers to it. `this` is the node's root pointer.
        unsafe { (*this)._this = this };
        // SAFETY: the node's root pointer; the node never moves and no reference to it is live.
        unsafe { qp_init(this as *mut dyn Qp, false) };
        // SAFETY: as above; a short access.
        unsafe { (*this)._moving.reserve(Self::SEND_QUEUE_LENGTH) };
        node
    }
}

impl Qp for ProxyNetDevice {
    fn base(&self) -> &QpBase {
        &self.base
    }
    fn base_mut(&mut self) -> &mut QpBase {
        &mut self.base
    }

    fn send(&mut self, _p: Packet) -> Future<()> {
        std::process::abort();
    }

    fn send_batch(&mut self, p: &mut CircularBuffer<Packet>) -> u32 {
        if !self._moving.is_empty() || self._send_depth == Self::SEND_QUEUE_LENGTH {
            return 0;
        }

        while !p.empty() && self._send_depth < Self::SEND_QUEUE_LENGTH {
            // SAFETY: the loop condition says the buffer is not empty.
            self._moving.push(unsafe { p.take_front().unwrap_unchecked() });
            self._send_depth += 1;
        }

        if !self._moving.is_empty() {
            // SAFETY: `dev` outlives its queues (the contract of `new`).
            let dev: *mut dyn Qp = unsafe { (*self._dev).queue_for_cpu(self._cpu) };
            let cpu = this_shard_id();
            // The qp's root pointer: the continuation runs after this call.
            let this = self._this;
            // Taken here, on this shard, so that the remote shard makes no reference to this
            // qp: it touches only the `_moving` elements (through `moving`, a pointer to the
            // vector's buffer, which `as_mut_ptr` gives without a reference to it), and this
            // shard leaves `_moving` alone while it is not empty (the early return above),
            // which is the exclusion that keeps the two shards apart. `_send_depth` is decremented
            // by the `free_on_cpu` callbacks, which run back on this shard; its pointer is
            // projected from the root pointer.
            let moving: *mut Packet = self._moving.as_mut_ptr();
            let nr = self._moving.len();
            // SAFETY: a place projection from the root pointer; it creates no reference.
            let sd: *mut usize = unsafe { std::ptr::addr_of_mut!((*this)._send_depth) };
            // SAFETY: the exclusion described above, with the same pointers; the target qp
            // same pointers; the target qp `dev` lives on the target shard and outlives the
            // work.
            let work = unsafe { CrossShard::new((moving, dev, sd)) };
            // FIXME: future is discarded
            let _ = Smp::submit_to(self._cpu, move || {
                let (moving, dev, sd) = work.into_inner();
                for i in 0..nr {
                    // SAFETY: see above; `i < nr` elements stay in place until `clear` below,
                    // which runs after this work completes.
                    let pk = unsafe {
                        (*moving.add(i)).free_on_cpu(cpu, move || {
                            // SAFETY: runs on the proxy's shard (see above); the proxy qp
                            // outlives its packets.
                            *sd -= 1;
                        })
                    };
                    // SAFETY: the target qp outlives the submitted work, which captures
                    // `dev`; this runs on its shard.
                    unsafe { (*dev).proxy_send(pk) };
                }
            })
            .then(move |_| {
                // SAFETY: back on the proxy's shard, from the reactor; the qp outlives the work.
                unsafe { (*this)._moving.clear() };
            });
        }

        self._moving.len() as u32
    }
}

/// Creates a proxy queue that sends through the queue of shard `master_cpu` of `dev`.
///
/// `dev` must outlive the returned queue.
pub fn create_proxy_net_device(master_cpu: u32, dev: *const dyn Device) -> QpPtr {
    let raw: *mut dyn Qp = ProxyNetDevice::new(master_cpu, dev).into_raw();
    // SAFETY: `raw` came from the node's `into_raw`; the node now owns it as a `dyn Qp`.
    unsafe { NodePtr::from_raw(raw) }
}
