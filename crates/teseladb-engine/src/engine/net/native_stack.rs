//! The native (userspace) network stack: its options, its creation on every shard and the
//! per-shard stack object.
//!
//! On shard 0, [`create_native_net_device`] creates the network device (virtio over vhost,
//! or DPDK with the `dpdk` feature) and hands each shard its queue; then every shard builds
//! its own [`NativeNetworkStack`]: an Ethernet interface, ARP and an IPv4 layer with TCP,
//! UDP and ICMP, configured statically or through DHCP. Each shard's stack is independent
//! (shared-nothing); the NIC's RSS and the stack's software forwarding deliver each flow to
//! one shard. [`register_native_stack`] registers the stack with the reactor's
//! network-stack registry under the name `native`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::sync::Arc;

use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, make_exception_future, make_ready_future,
};
use crate::engine::core::reactor::engine;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shard_id::this_shard_id;
use crate::engine::core::shared_ptr::{make_shared, shared_ptr_converting_move};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::timer::Timer;
use crate::engine::net::api::{
    DatagramChannel, FactoryFunc, ListenOptions, NetworkInterface, NetworkStack, NetworkStackEntry,
    ServerSocket, Socket, Statistics, UdpChannel,
};
use crate::engine::net::byteorder::ntohs;
use crate::engine::net::config::parse_config;
use crate::engine::net::dhcp::{Dhcp, Lease};
use crate::engine::net::dpdk::DpdkOptions;
#[cfg(feature = "dpdk")]
use crate::engine::net::dpdk::{create_dpdk_net_device, create_dpdk_net_device_from_config};
use crate::engine::net::ethernet::EthernetAddress;
use crate::engine::net::inet_address::{InetAddress, RuntimeError};
use crate::engine::net::ip::{IpPacketFilter, Ipv4, Ipv4Udp};
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::native_stack_impl::native_stack_net_stats;
use crate::engine::net::net::{Device, Interface, Qp};
use crate::engine::net::node_ptr::{CrossShard, NodePtr, StaticConst};
use crate::engine::net::proxy::create_proxy_net_device;
use crate::engine::net::socket_defs::{AF_INET, Ipv4Addr, SocketAddress, sa_family_t};
use crate::engine::net::stack::NetworkInterfaceImpl;
use crate::engine::net::tcp_stack::{tcpv4_listen, tcpv4_socket};
use crate::engine::net::virtio::{VirtioOptions, create_virtio_net_device};
use crate::engine::util::program_options::{AsOptionGroup, OptionGroup, Switch, Unused, Value};

/// Native stack configuration. `repr(C)` with the group first, so that the option group a
/// device is handed can be cast back to the options.
#[repr(C)]
pub struct NativeStackOptions {
    /// The option group these options belong to.
    pub group: OptionGroup,
    /// The configuration of the network devices (a ghost option).
    pub net_config: Value<String>,
    /// The file holding the configuration of the network devices (a ghost option).
    pub net_config_file: Value<String>,
    /// The tap device to connect to.
    ///
    /// Default: `tap0`.
    pub tap_device: Value<String>,
    /// The static IPv4 address to use.
    ///
    /// Default: `192.168.122.2`.
    pub host_ipv4_addr: Value<String>,
    /// The static IPv4 gateway to use.
    ///
    /// Default: `192.168.122.1`.
    pub gw_ipv4_addr: Value<String>,
    /// The static IPv4 netmask to use.
    ///
    /// Default: `255.255.255.0`.
    pub netmask_ipv4_addr: Value<String>,
    /// The size of the per-channel UDPv4 packet queue.
    ///
    /// Default: the default queue size of the IPv4 UDP layer.
    pub udpv4_queue_size: Value<i32>,
    /// Whether to use DHCP discovery.
    ///
    /// Default: `true`.
    pub dhcp: Value<bool>,
    /// The weight of a hardware network queue relative to a software queue.
    ///
    /// Values:
    /// * 0.0: no work
    /// * 1.0: equal share
    ///
    /// Default: 1.0.
    pub hw_queue_weight: Value<f32>,
    /// Whether to use the DPDK PMD drivers.
    ///
    /// Unused when the crate is built without DPDK support.
    pub dpdk_pmd: Switch,
    /// Whether to enable LRO (`on` or `off`).
    ///
    /// Default: `on`.
    pub lro: Value<String>,

    /// Virtio configuration.
    pub virtio_opts: VirtioOptions,
    /// DPDK configuration.
    ///
    /// Unused when the engine is built without the `dpdk` feature.
    pub dpdk_opts: DpdkOptions,

    /// Whether DPDK queues use hugepage-backed memory directly (not a command-line option;
    /// `false` by default).
    pub _hugepages: bool,
}

/// Creates the network device described by `opts` on shard 0, gives every shard its
/// queue, and creates the native stack on every shard once the link is ready.
pub fn create_native_net_device(opts: &'static NativeStackOptions) -> Result<(), ExceptionPtr> {
    let mut deprecated_config_used = true;

    let mut net_config = String::new();

    if opts.net_config.as_bool() {
        deprecated_config_used = false;
        net_config.push_str(opts.net_config.get_value());
    }
    if opts.net_config_file.as_bool() {
        deprecated_config_used = false;
        if let Ok(mut fs) = std::fs::File::open(opts.net_config_file.get_value()) {
            let _ = fs.read_to_string(&mut net_config);
        }
    }

    let mut dev: Option<Box<dyn Device>> = None;

    if deprecated_config_used {
        #[cfg(feature = "dpdk")]
        {
            if opts.dpdk_pmd.as_bool() {
                dev = Some(create_dpdk_net_device(
                    *opts.dpdk_opts.dpdk_port_index.get_value() as u16,
                    Smp::count() as u16,
                    !(opts.lro.as_bool() && opts.lro.get_value() == "off"),
                    !(opts.dpdk_opts.hw_fc.as_bool() && opts.dpdk_opts.hw_fc.get_value() == "off"),
                )?);
            } else {
                dev = Some(create_virtio_net_device(&opts.virtio_opts, &opts.lro)?);
            }
        }
        #[cfg(not(feature = "dpdk"))]
        {
            dev = Some(create_virtio_net_device(&opts.virtio_opts, &opts.lro)?);
        }
    } else {
        let device_configs = parse_config(&mut net_config.as_bytes())?;

        if device_configs.len() > 1 {
            return Err(ExceptionPtr::from(RuntimeError(
                "only one network interface is supported".to_string(),
            )));
        }

        #[cfg(feature = "dpdk")]
        for device_config in device_configs.values() {
            let hw_config = &device_config.hw_cfg;
            if hw_config.port_index.is_some() || !hw_config.pci_address.is_empty() {
                dev = Some(create_dpdk_net_device_from_config(hw_config)?);
            } else {
                return Err(ExceptionPtr::from(RuntimeError(
                    "only DPDK supports new configuration format".to_string(),
                )));
            }
        }
        #[cfg(not(feature = "dpdk"))]
        if !device_configs.is_empty() {
            return Err(ExceptionPtr::from(RuntimeError(
                "only DPDK supports new configuration format".to_string(),
            )));
        }
    }

    // Every use of the semaphore is on this shard (the continuations of `submit_to` run
    // here), so a non-atomic shared pointer is enough.
    let sem = make_shared(Semaphore::new(0));
    let Some(dev) = dev else {
        // without a device there is nothing to give the shards
        return Err(ExceptionPtr::from(RuntimeError("no network device".to_string())));
    };
    let sdev: Arc<dyn Device> = Arc::from(dev);
    // set_local_queue on all shard in the background,
    // signal when done.
    // FIXME: handle exceptions
    for i in 0..Smp::count() {
        // SAFETY: the device is shared by every shard by design (each touches only its own
        // queue) and the options live for the whole program.
        let fwd = unsafe { CrossShard::new((sdev.clone(), opts)) };
        let sem = sem.clone();
        let _ = Smp::submit_to(i, move || {
            let (sdev, opts) = fwd.into_inner();
            let qid = this_shard_id() as u16;
            if qid < sdev.hw_queues_count() {
                // A failure fails this shard's future, so the semaphore is not signaled for
                // it.
                let qp = match sdev.init_local_queue(opts, qid) {
                    Ok(q) => q,
                    Err(e) => return make_exception_future(e),
                };
                let mut cpu_weights: BTreeMap<u32, f32> = BTreeMap::new();
                let hqc = sdev.hw_queues_count() as u32;
                let mut i = hqc + qid as u32 % hqc;
                while i < Smp::count() {
                    cpu_weights.insert(i, 1.0);
                    i += hqc;
                }
                cpu_weights.insert(qid as u32, *opts.hw_queue_weight.get_value());
                // SAFETY: the qp's root pointer; it lives in its node and nothing else refers
                // to it yet.
                unsafe { <dyn Qp>::configure_proxies(qp.as_ptr(), &cpu_weights) };
                sdev.set_local_queue(qp);
            } else {
                let master = qid % sdev.hw_queues_count();
                let dp: *const dyn Device = Arc::as_ptr(&sdev);
                sdev.set_local_queue(create_proxy_net_device(master as u32, dp));
            }
            make_ready_future(())
        })
        .then(move |_| {
            // SAFETY: `sem` keeps the semaphore alive; no other reference to it is in use.
            unsafe { sem.get_mut_unchecked() }.signal(1);
            make_ready_future(())
        });
    }
    // wait for all shards to set their local queue,
    // then when link is ready, communicate the native_stack to the caller
    // via `create_native_stack` (that sets the ready_promise value)
    let sem2 = sem.clone();
    // SAFETY: as above.
    let _ = unsafe { sem2.get_mut_unchecked() }.wait(Smp::count() as usize).then(move |()| {
        let sdev2 = sdev.clone();
        // FIXME: future is discarded
        let _ = sdev.link_ready().then(move |()| {
            for i in 0..Smp::count() {
                // SAFETY: as above (the shared device and the program-long options).
                let fwd = unsafe { CrossShard::new((sdev2.clone(), opts)) };
                // FIXME: future is discarded
                let _ = Smp::submit_to(i, move || {
                    let (sdev, opts) = fwd.into_inner();
                    create_native_stack(opts, sdev);
                });
            }
            make_ready_future(())
        });
        drop(sem);
        make_ready_future(())
    });
    Ok(())
}

thread_local! {
    /// The stack of this shard, ready once its interface is configured.
    static READY_PROMISE: RefCell<Promise<Box<dyn NetworkStack>>> = RefCell::new(Promise::new());
}

/// The native network stack of one shard, as the reactor holds it.
///
/// A handle to the stack's node. The reactor owns the stack through a
/// `Box<dyn NetworkStack>` that moves (into the ready promise, then into the reactor), and
/// each move of a `Box` reasserts its uniqueness, which would invalidate the pointers that
/// the in-place IPv4 layer, its callbacks and the stack's own continuations keep. The stack
/// therefore lives in a node that never moves.
pub struct NativeNetworkStack {
    _impl: NodePtr<NativeNetworkStackImpl>,
}

/// The members of `native_network_stack`, in their node.
/// The state of a [`NativeNetworkStack`], in its node.
pub struct NativeNetworkStackImpl {
    /// The address of the node, written once by `new`; the continuations and the timer keep
    /// this one, never a pointer from `&mut self`.
    _this: *mut NativeNetworkStackImpl,
    _netif: NodePtr<Interface>,
    _inet: Ipv4,
    _dhcp: bool,
    _config: Promise<()>,
    _timer: Timer,
}

impl NativeNetworkStack {
    /// Creates this shard's stack (on shard 0, first the device) and resolves to it once the
    /// interface is configured.
    ///
    /// # Errors
    /// The returned future fails if the device cannot be created.
    pub fn create(opts: &'static NativeStackOptions) -> Future<Box<dyn NetworkStack>> {
        if this_shard_id() == 0
            && let Err(e) = create_native_net_device(opts)
        {
            return crate::engine::core::future::make_exception_future(e);
        }
        READY_PROMISE.with(|p| p.borrow_mut().get_future())
    }

    /// The root pointer of this shard's native stack. Callers make short accesses through it
    /// (`arp_learn` also runs while the stack's IPv4 layer is on the call stack, so no `&mut`
    /// to the whole stack is formed here).
    fn local() -> *mut NativeNetworkStackImpl {
        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        let ns: *mut dyn NetworkStack = unsafe { (*engine()).net() };
        // SAFETY: these helpers only run on shards whose network stack is the native one;
        // only the handle's node pointer is read.
        unsafe { (*(ns as *mut NativeNetworkStack))._impl.as_ptr() }
    }

    /// The stack, for a call from the reactor.
    #[inline]
    fn st(&mut self) -> &mut NativeNetworkStackImpl {
        // SAFETY: the network stack's entry points run from the reactor, with no other
        // reference to the stack live.
        unsafe { self._impl.get_mut() }
    }
}

impl NativeNetworkStackImpl {
    fn set_ipv4_packet_filter(&mut self, filter: Option<*mut dyn IpPacketFilter>) {
        self._inet.set_packet_filter(filter);
    }

    /// Records that `l3` is at `l2`.
    pub fn arp_learn(&mut self, l2: EthernetAddress, l3: Ipv4Address) {
        self._inet.learn(l2, l3);
    }
}

impl NetworkStack for NativeNetworkStack {
    fn listen(
        &mut self,
        sa: SocketAddress,
        opts: ListenOptions,
    ) -> Result<ServerSocket, ExceptionPtr> {
        assert!(sa.family() == AF_INET || sa.is_unspecified());
        let tcp = self.st()._inet.get_tcp();
        // SAFETY: the tcp layer lives in its own node; no reference to it is live here, and
        // listening does not enter tcbs.
        Ok(tcpv4_listen(unsafe { &mut *tcp }, ntohs(sa.as_posix_sockaddr_in().sin_port), opts))
    }

    fn socket(&mut self) -> Socket {
        let tcp = self.st()._inet.get_tcp();
        // SAFETY: as in listen.
        tcpv4_socket(unsafe { &mut *tcp })
    }

    fn make_udp_channel(&mut self, addr: &SocketAddress) -> Result<UdpChannel, ExceptionPtr> {
        self.st()._inet.get_udp().make_channel(Ipv4Addr::from_socket_address(addr))
    }

    fn make_unbound_datagram_channel(
        &mut self,
        family: sa_family_t,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        if family != AF_INET {
            return Err(ExceptionPtr::from(RuntimeError("Unsupported address family".to_string())));
        }

        self.st()._inet.get_udp().make_channel(Ipv4Addr::default())
    }

    fn make_bound_datagram_channel(
        &mut self,
        local: &SocketAddress,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        self.st()._inet.get_udp().make_channel(Ipv4Addr::from_socket_address(local))
    }

    fn initialize(&mut self) -> Future<()> {
        // The stack's root pointer: the continuation may run later.
        let this = self._impl.as_ptr();
        make_ready_future(()).then(move |()| {
            // SAFETY: the stack lives as long as the reactor that initializes it; this runs
            // from the reactor (or in line, with only the handle borrowed), with no reference
            // to the stack's node live, and each access is short.
            unsafe {
                if !(*this)._dhcp {
                    return make_ready_future(());
                }

                // Only run actual discover on main cpu.
                // All other cpus must simply for main thread to complete and signal them.
                if this_shard_id() == 0 {
                    // FIXME: future is discarded
                    let _ = NativeNetworkStackImpl::run_dhcp(this, false, Lease::default());
                }
                (*this)._config.get_future()
            }
        })
    }

    fn has_per_core_namespace(&mut self) -> bool {
        true
    }

    fn network_interfaces(&mut self) -> Result<Vec<NetworkInterface>, ExceptionPtr> {
        // The interface is always set, so its data is always available.
        // SAFETY: the interface data (numbers, addresses and bytes) has no interior
        // mutability; its heap blocks are only read.
        static NWIF: StaticConst<NativeNetworkInterfaceData> = unsafe { StaticConst::new() };
        let st: &NativeNetworkStackImpl = self.st();
        let nwif = NWIF.get_or_init(|| NativeNetworkInterfaceData::new(st));

        // SAFETY: the cast keeps the pointer into the same object.
        let impl_ = unsafe {
            shared_ptr_converting_move(
                make_shared(NativeNetworkInterface { _data: nwif.clone() }),
                |p| p as *mut dyn NetworkInterfaceImpl,
            )
        };
        let res: Vec<NetworkInterface> = vec![NetworkInterface::new(impl_)];
        Ok(res)
    }

    fn stats(&mut self, scheduling_group_id: u32) -> Statistics {
        Statistics {
            bytes_sent: native_stack_net_stats::BYTES_SENT
                .with(|b| b[scheduling_group_id as usize].get()),
            bytes_received: native_stack_net_stats::BYTES_RECEIVED
                .with(|b| b[scheduling_group_id as usize].get()),
        }
    }

    fn clear_stats(&mut self, scheduling_group_id: u32) {
        native_stack_net_stats::BYTES_SENT.with(|b| b[scheduling_group_id as usize].set(0));
        native_stack_net_stats::BYTES_RECEIVED.with(|b| b[scheduling_group_id as usize].set(0));
    }
}

impl NativeNetworkStack {
    /// Builds the stack over `dev` with `opts`; the IPv4 layer is constructed in place, in
    /// the stack's node, since it points to the interface and to itself.
    ///
    /// # Errors
    /// Fails if a static address option does not parse. The options are checked before the
    /// interface is built, because the interface registers itself with `dev` (its receive
    /// callback and its packet provider) and nothing could take that back.
    pub fn new(opts: &NativeStackOptions, dev: Arc<dyn Device>) -> Result<Self, ExceptionPtr> {
        let dhcp = opts.host_ipv4_addr.defaulted()
            && opts.gw_ipv4_addr.defaulted()
            && opts.netmask_ipv4_addr.defaulted()
            && *opts.dhcp.get_value();
        let static_addresses = if dhcp {
            None
        } else {
            Some((
                Ipv4Address::from_string(opts.host_ipv4_addr.get_value())?,
                Ipv4Address::from_string(opts.gw_ipv4_addr.get_value())?,
                Ipv4Address::from_string(opts.netmask_ipv4_addr.get_value())?,
            ))
        };
        let raw: *mut MaybeUninit<NativeNetworkStackImpl> = Box::into_raw(Box::new_uninit());
        let p = raw.cast::<NativeNetworkStackImpl>();
        // SAFETY: every field is written before the node is used; `_inet` is constructed in
        // place after `_netif`, which it points to. `p` is the node's root pointer, owned
        // from here on by the `NodePtr` (whose `Box::from_raw` frees it with the same layout).
        let node = unsafe {
            use std::ptr::addr_of_mut;
            addr_of_mut!((*p)._this).write(p);
            addr_of_mut!((*p)._netif).write(Interface::new(dev));
            let netif: *mut Interface = (*p)._netif.as_ptr();
            Ipv4::init_at(addr_of_mut!((*p)._inet), netif);
            addr_of_mut!((*p)._dhcp).write(false);
            addr_of_mut!((*p)._config).write(Promise::new());
            addr_of_mut!((*p)._timer).write(Timer::new());
            NodePtr::from_raw(p)
        };
        let mut this = NativeNetworkStack { _impl: node };
        let st = this.st();
        st._inet.get_udp().set_queue_size(*opts.udpv4_queue_size.get_value());
        st._dhcp = dhcp;
        if let Some((host, gw, netmask)) = static_addresses {
            st._inet.set_host_address(host);
            st._inet.set_gw_address(gw);
            st._inet.set_netmask_address(netmask);
        }
        Ok(this)
    }
}

impl NativeNetworkStackImpl {
    /// # Safety
    /// `this` is the root pointer of this shard's live stack, with no reference to it live.
    unsafe fn run_dhcp(this: *mut Self, is_renew: bool, res: Lease) -> Future<()> {
        // SAFETY: guaranteed by the caller; the client keeps the ipv4 layer's address, which
        // is projected from the root pointer.
        let mut d = match unsafe { Dhcp::new(std::ptr::addr_of_mut!((*this)._inet)) } {
            Ok(d) => d,
            Err(e) => return make_exception_future(e),
        };
        // Hijack the ip-stack.
        // SAFETY: the same filter pointer goes to every shard: the filter (the DHCP client,
        // on shard 0) outlives the hijack, which the second `invoke_on_all` below ends
        // before the client is destroyed, and each shard enters it from its packet path.
        let f = unsafe { CrossShard::new(d.get_ipv4_filter()) };
        Smp::invoke_on_all(move || {
            let ns = NativeNetworkStack::local();
            // SAFETY: a short access to this shard's stack from the reactor.
            unsafe { (*ns).set_ipv4_packet_filter(Some(f.into_inner())) };
        })
        .then(move |()| {
            let fut = if is_renew {
                d.renew(&res, Dhcp::DEFAULT_TIMEOUT)
            } else {
                d.discover(Dhcp::DEFAULT_TIMEOUT)
            };
            fut.then(move |lease: Option<Lease>| {
                Smp::invoke_on_all(|| {
                    let ns = NativeNetworkStack::local();
                    // SAFETY: a short access to this shard's stack from the reactor.
                    unsafe { (*ns).set_ipv4_packet_filter(None) };
                })
                .then(move |()| {
                    // SAFETY: the stack lives as long as the reactor; this runs from it with no
                    // reference to the stack live, through the root pointer.
                    unsafe { Self::on_dhcp(this, lease, is_renew) };
                    make_ready_future(())
                })
            })
            .finally(move || {
                drop(d);
            })
        })
    }

    /// # Safety
    /// `this` is the root pointer of this shard's live stack, with no reference to it live.
    unsafe fn on_dhcp(this: *mut Self, lease: Option<Lease>, is_renew: bool) {
        // SAFETY: guaranteed by the caller; the reference ends before any continuation runs.
        let me = unsafe { &mut *this };
        if let Some(res) = &lease {
            me._inet.set_host_address(res.ip);
            me._inet.set_gw_address(res.gateway);
            me._inet.set_netmask_address(res.netmask);
        }
        // Signal waiters.
        if !is_renew {
            me._config.set_value(());
        }

        if this_shard_id() == 0 {
            // And the other cpus, which, in the case of initial discovery,
            // will be waiting for us.
            for i in 1..Smp::count() {
                let lease = lease.clone();
                let _ = Smp::submit_to(i, move || {
                    let ns = NativeNetworkStack::local();
                    // SAFETY: the root pointer of shard `i`'s stack, entered from its reactor
                    // with no reference to it live.
                    unsafe { Self::on_dhcp(ns, lease, is_renew) };
                });
            }
            if let Some(res) = lease {
                // And set up to renew the lease later on.
                let lease_time = res.lease_time;
                // SAFETY: the stack lives in its node and never moves, so its timer does not
                // either.
                unsafe { Pin::new_unchecked(&mut me._timer) }.set_callback(move || {
                    // SAFETY: the timer is owned by the stack, which outlives it; the callback
                    // runs from the reactor with no other reference live, through the root
                    // pointer `this`.
                    let me = unsafe { &mut *this };
                    me._config = Promise::new();
                    // callback ignores future result
                    // SAFETY: as above; `me` is not used after this call, so `this` is the only
                    // access path to the stack during it.
                    let _ = unsafe { Self::run_dhcp(this, true, res.clone()) };
                });
                // SAFETY: as above.
                unsafe { Pin::new_unchecked(&mut me._timer) }.arm_in(lease_time);
            }
        }
    }
}

/// Records on every shard that `l3` is at `l2`.
pub fn arp_learn(l2: EthernetAddress, l3: Ipv4Address) {
    // Run arp_learn on all shard in the background
    let _ = Smp::invoke_on_all(move || {
        let ns = NativeNetworkStack::local();
        // SAFETY: a short access to this shard's stack; no reference to its ipv4 layer is live
        // (the ARP layer learns on its own shard through `arp_learn_on_others`).
        unsafe { (*ns).arp_learn(l2, l3) };
    });
}

/// Runs `arp_learn` on every shard but this one, for the ARP layer of this shard, which
/// learns the address itself: it runs inside that layer's `received`, where reaching the
/// layer again through the stack would alias it. The local learning happens first and the
/// other shards learn later.
pub fn arp_learn_on_others(l2: EthernetAddress, l3: Ipv4Address) {
    // Run arp_learn on all shard in the background
    let _ = Smp::invoke_on_others(move || {
        let ns = NativeNetworkStack::local();
        // SAFETY: a short access to that shard's stack from its reactor.
        unsafe { (*ns).arp_learn(l2, l3) };
    });
}

/// Creates the native stack of this shard and hands it to `NativeNetworkStack::create`.
pub fn create_native_stack(opts: &NativeStackOptions, dev: Arc<dyn Device>) {
    match NativeNetworkStack::new(opts, dev) {
        Ok(s) => {
            READY_PROMISE.with(|p| p.borrow_mut().set_value(Box::new(s) as Box<dyn NetworkStack>))
        }
        Err(e) => READY_PROMISE.with(|p| p.borrow_mut().set_exception(e)),
    }
}

impl Default for NativeStackOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl NativeStackOptions {
    /// The options with their defaults, not yet linked (see
    /// [`NativeStackOptions::link`]).
    pub fn new() -> Self {
        NativeStackOptions {
            group: OptionGroup::new("Native networking stack options"),
            // these two are ghost options
            net_config: Value::new_unused("net-config", Unused),
            net_config_file: Value::new_unused("net-config-file", Unused),
            tap_device: Value::new(
                "tap-device",
                Some("tap0".to_string()),
                "tap device to connect to",
            ),
            host_ipv4_addr: Value::new(
                "host-ipv4-addr",
                Some("192.168.122.2".to_string()),
                "static IPv4 address to use",
            ),
            gw_ipv4_addr: Value::new(
                "gw-ipv4-addr",
                Some("192.168.122.1".to_string()),
                "static IPv4 gateway to use",
            ),
            netmask_ipv4_addr: Value::new(
                "netmask-ipv4-addr",
                Some("255.255.255.0".to_string()),
                "static IPv4 netmask to use",
            ),
            udpv4_queue_size: Value::new(
                "udpv4-queue-size",
                Some(Ipv4Udp::DEFAULT_QUEUE_SIZE),
                "Default size of the UDPv4 per-channel packet queue",
            ),
            dhcp: Value::new("dhcp", Some(true), "Use DHCP discovery"),
            hw_queue_weight: Value::new(
                "hw-queue-weight",
                Some(1.0f32),
                "Weighing of a hardware network queue relative to a software queue (0=no work, 1=equal share)",
            ),
            #[cfg(feature = "dpdk")]
            dpdk_pmd: Switch::new("dpdk-pmd", "Use DPDK PMD drivers"),
            #[cfg(not(feature = "dpdk"))]
            dpdk_pmd: Switch::new_unused("dpdk-pmd", Unused),
            lro: Value::new("lro", Some("on".to_string()), "Enable LRO"),
            virtio_opts: VirtioOptions::new(),
            dpdk_opts: DpdkOptions::new(),
            _hugepages: false,
        }
    }

    /// Links the group under `parent_group` and registers every value and subgroup with it,
    /// in place, in declaration order.
    pub fn link(self: Pin<&mut Self>, parent_group: Option<Pin<&mut OptionGroup>>) {
        crate::link_option_group!(
            self,
            parent_group,
            group,
            net_config,
            net_config_file,
            tap_device,
            host_ipv4_addr,
            gw_ipv4_addr,
            netmask_ipv4_addr,
            udpv4_queue_size,
            dhcp,
            hw_queue_weight,
            dpdk_pmd,
            lro,
            [virtio_opts],
            [dpdk_opts]
        );
    }
}

impl AsOptionGroup for NativeStackOptions {
    fn group(&self) -> &OptionGroup {
        &self.group
    }
    fn group_mut(&mut self) -> &mut OptionGroup {
        &mut self.group
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The entry of the native stack in the registry of network stacks.
pub fn register_native_stack() -> NetworkStackEntry {
    // Linked once in its box, where it stays (the reactor pins the box as it is).
    let mut opts = Box::new(NativeStackOptions::new());
    // SAFETY: the options do not move out of their heap block from here on.
    unsafe { Pin::new_unchecked(&mut *opts) }.link(None);
    let optsp: *const NativeStackOptions = &*opts;
    NetworkStackEntry {
        name: Sstring::from("native"),
        opts,
        factory: FactoryFunc::new(move |_ops: &OptionGroup| {
            // SAFETY: the options live in the registry entry for the whole program; the
            // factory is only handed this group.
            NativeNetworkStack::create(unsafe { &*optsp })
        }),
        is_default: false,
    }
}

/// The data a `native_network_interface` copies from its stack.
#[derive(Clone)]
struct NativeNetworkInterfaceData {
    _mtu: u32,
    _addresses: Vec<InetAddress>,
    _hardware_address: Vec<u8>,
}

impl NativeNetworkInterfaceData {
    fn new(stack: &NativeNetworkStackImpl) -> Self {
        // SAFETY: the interface outlives the stack's ipv4 layer.
        let netif = unsafe { &*stack._inet.netif() };
        let mac = netif.hw_address().mac;
        NativeNetworkInterfaceData {
            _mtu: netif.hw_features().mtu as u32,
            _addresses: vec![InetAddress::from_ipv4_address(&stack._inet.host_address())],
            _hardware_address: mac.to_vec(),
        }
    }
}

/// The network interface description (`if0`) the native stack reports.
struct NativeNetworkInterface {
    _data: NativeNetworkInterfaceData,
}

/// The name of the native stack's only interface, `if0`.
fn if0() -> &'static Sstring {
    // SAFETY: an `Sstring` has no interior mutability; its heap block is only read.
    static NAME: StaticConst<Sstring> = unsafe { StaticConst::new() };
    NAME.get_or_init(|| Sstring::from("if0"))
}

impl NetworkInterfaceImpl for NativeNetworkInterface {
    fn index(&self) -> u32 {
        0
    }
    fn mtu(&self) -> u32 {
        self._data._mtu
    }
    fn name(&self) -> &Sstring {
        if0()
    }
    fn display_name(&self) -> &Sstring {
        self.name()
    }
    fn addresses(&self) -> &Vec<InetAddress> {
        &self._data._addresses
    }
    fn hardware_address(&self) -> Vec<u8> {
        self._data._hardware_address.clone()
    }
    fn is_loopback(&self) -> bool {
        false
    }
    fn is_virtual(&self) -> bool {
        false
    }
    fn is_up(&self) -> bool {
        true
    }
    fn supports_ipv6(&self) -> bool {
        false
    }
}
