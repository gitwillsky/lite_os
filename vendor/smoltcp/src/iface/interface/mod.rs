// Heads up! Before working on this file you should read the parts
// of RFC 1122 that discuss Ethernet, ARP and IP for any IPv4 work
// and RFCs 8200 and 4861 for any IPv6 and NDISC work.

#[cfg(test)]
mod tests;

#[cfg(feature = "medium-ethernet")]
mod ethernet;
#[cfg(feature = "medium-ieee802154")]
mod ieee802154;

#[cfg(feature = "proto-ipv4")]
mod ipv4;
#[cfg(feature = "proto-ipv6")]
mod ipv6;
#[cfg(feature = "proto-sixlowpan")]
mod sixlowpan;

#[cfg(feature = "multicast")]
pub(crate) mod multicast;
#[cfg(feature = "socket-tcp")]
mod tcp;
#[cfg(any(feature = "socket-udp", feature = "socket-dns"))]
mod udp;

use super::packet::*;

use core::result::Result;
use heapless::Vec;

#[cfg(feature = "_proto-fragmentation")]
use super::fragmentation::FragKey;
#[cfg(feature = "_proto-fragmentation")]
use super::fragmentation::PacketAssemblerSet;
use super::fragmentation::{Fragmenter, FragmentsBuffer};

#[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
use super::neighbor::{Answer as NeighborAnswer, Cache as NeighborCache};
use super::socket_set::SocketSet;
use crate::config::IFACE_MAX_ADDR_COUNT;
#[cfg(feature = "proto-sixlowpan")]
use crate::config::IFACE_MAX_SIXLOWPAN_ADDRESS_CONTEXT_COUNT;
use crate::iface::Routes;
#[cfg(feature = "proto-ipv6-slaac")]
use crate::iface::Slaac;
use crate::phy::PacketMeta;
use crate::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use crate::rand::Rand;
use crate::socket::*;
use crate::time::{Duration, Instant};

use crate::wire::*;

macro_rules! check {
    ($e:expr) => {
        match $e {
            Ok(x) => x,
            Err(_) => {
                // concat!/stringify! doesn't work with defmt macros
                #[cfg(not(feature = "defmt"))]
                net_trace!(concat!("iface: malformed ", stringify!($e)));
                #[cfg(feature = "defmt")]
                net_trace!("iface: malformed");
                return Default::default();
            }
        }
    };
}
use check;

/// Result returned by [`Interface::poll`].
///
/// This contains information on whether socket states might have changed.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PollResult {
    /// Socket state is guaranteed to not have changed.
    None,
    /// You should check the state of sockets again for received data or completion of operations.
    SocketStateChanged,
}

/// Result returned by [`Interface::poll_ingress_single`].
///
/// This contains information on whether a packet was processed or not,
/// and whether it might've affected socket states.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum PollIngressSingleResult {
    /// No packet was processed. You don't need to call [`Interface::poll_ingress_single`]
    /// again, until more packets arrive.
    ///
    /// Socket state is guaranteed to not have changed.
    None,
    /// A packet was processed.
    ///
    /// There may be more packets in the device's RX queue, so you should call [`Interface::poll_ingress_single`] again.
    ///
    /// Socket state is guaranteed to not have changed.
    PacketProcessed,
    /// A packet was processed, which might have caused socket state to change.
    ///
    /// There may be more packets in the device's RX queue, so you should call [`Interface::poll_ingress_single`] again.
    ///
    /// You should check the state of sockets again for received data or completion of operations.
    SocketStateChanged,
}

/// 本次实际路由提供的transport事实；只在dispatch/RX guard有效，不保存FIB副本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IpRouteContext {
    pub mtu: usize,
    pub advertised_mss: usize,
    pub receive_window: usize,
    /// 原生RTAX_RTT，单位为八分之一毫秒；零表示未配置。
    pub initial_rtt: u32,
}
/// 唯一外部FIB对本次RX的已冻结结论；不在库保存/查询另一张kernel路由表。
#[cfg(feature = "proto-ipv4")]
#[derive(Clone, Copy)]
pub struct Ipv4IngressFacts {
    pub destination: Ipv4Address,
    pub interface_index: u32,
    pub local: bool,
    pub broadcast: bool,
    pub specific_destination: Option<Ipv4Address>,
    /// 同一个FIB冻结的反向route MTU；本次接收只可能匹配unbound或实际logical iif绑定。
    pub reverse_route: Option<IpRouteContext>,
    pub bound_reverse_route: Option<IpRouteContext>,
}
/// 原codec产生的借用IPv4回复；caller只能在callback内编码到自己的有预算队列。
#[cfg(feature = "proto-ipv4")]
pub struct Ipv4Reply<'a> {
    packet: Packet<'a>,
    caps: DeviceCapabilities,
    bound_interface: Option<u32>,
}
#[cfg(feature = "proto-ipv4")]
impl Ipv4Reply<'_> {
    /// 用原Packet codec包装实际TCP reset；不保留socket/port引用，checksum由软件完整产生。
    #[cfg(feature = "socket-tcp")]
    pub fn tcp_reset(
        packet: (IpRepr, TcpRepr<'static>),
        bound_interface: Option<u32>,
    ) -> Ipv4Reply<'static> {
        Ipv4Reply {
            packet: Packet::new(packet.0, IpPayload::Tcp(packet.1)),
            caps: DeviceCapabilities::default(),
            bound_interface,
        }
    }
    /// 实际匹配TCP socket的原生bound device；无socket回复保持None。
    pub fn bound_interface(&self) -> Option<u32> {
        self.bound_interface
    }
    /// 实际完整IP packet长度；没有新协议codec。
    pub fn buffer_len(&self) -> usize {
        self.packet.ip_repr().buffer_len()
    }
    /// 复用原Packet的header/payload/checksum编码；output须等于buffer_len。
    pub fn emit(&self, output: &mut [u8]) {
        let repr = self.packet.ip_repr();
        assert_eq!(output.len(), repr.buffer_len());
        repr.emit(&mut *output, &self.caps.checksum);
        self.packet
            .emit_payload(&repr, &mut output[repr.header_len()..], &self.caps);
    }
}

/// A  network interface.
///
/// The network interface logically owns a number of other data structures; to avoid
/// a dependency on heap allocation, it instead owns a `BorrowMut<[T]>`, which can be
/// a `&mut [T]`, or `Vec<T>` if a heap is available.
pub struct Interface {
    pub(crate) inner: InterfaceInner,
    fragments: FragmentsBuffer,
    fragmenter: Fragmenter,
}

/// The device independent part of an Ethernet network interface.
///
/// Separating the device from the data required for processing and dispatching makes
/// it possible to borrow them independently. For example, the tx and rx tokens borrow
/// the `device` mutably until they're used, which makes it impossible to call other
/// methods on the `Interface` in this time (since its `device` field is borrowed
/// exclusively). However, it is still possible to call methods on its `inner` field.
pub struct InterfaceInner {
    caps: DeviceCapabilities,
    now: Instant,
    rand: Rand,
    // OWNER: 实际设备的scope identity；仅供准入mechanism，不含socket/route policy副本。
    interface_index: u32,
    // OWNER: 只在当前dispatch有效的唯一外部FIB结论，egress/ingress结束必须清空。
    egress_route: Option<(IpAddress, u32, bool, Option<IpRouteContext>)>,
    #[cfg(feature = "proto-ipv4")]
    ingress_facts: Option<Ipv4IngressFacts>,
    // OWNER: 本次parser实际匹配socket的reply flow oif；开始/结束RX必须清除。
    reply_bound_interface: Option<u32>,
    // OWNER: configured native ifa_broadcast 的mechanism输入；None保持library默认Cidr配置。
    #[cfg(feature = "proto-ipv4")]
    ipv4_broadcast: Option<Ipv4Address>,

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    neighbor_cache: NeighborCache,
    hardware_addr: HardwareAddress,
    #[cfg(feature = "medium-ieee802154")]
    sequence_no: u8,
    #[cfg(feature = "medium-ieee802154")]
    pan_id: Option<Ieee802154Pan>,
    #[cfg(feature = "proto-ipv4-fragmentation")]
    ipv4_id: u16,
    #[cfg(feature = "proto-sixlowpan")]
    sixlowpan_address_context:
        Vec<SixlowpanAddressContext, IFACE_MAX_SIXLOWPAN_ADDRESS_CONTEXT_COUNT>,
    #[cfg(feature = "proto-sixlowpan-fragmentation")]
    tag: u16,
    ip_addrs: Vec<IpCidr, IFACE_MAX_ADDR_COUNT>,
    any_ip: bool,
    #[cfg(feature = "proto-ipv6-slaac")]
    slaac_enabled: bool,
    #[cfg(feature = "proto-ipv6-slaac")]
    slaac: Slaac,
    #[cfg(feature = "proto-ipv6-slaac")]
    slaac_updated: Instant,
    routes: Routes,
    #[cfg(feature = "multicast")]
    multicast: multicast::State,
}

/// Configuration structure used for creating a network interface.
#[non_exhaustive]
pub struct Config {
    /// Random seed.
    ///
    /// It is strongly recommended that the random seed is different on each boot,
    /// to avoid problems with TCP port/sequence collisions.
    ///
    /// The seed doesn't have to be cryptographically secure.
    pub random_seed: u64,

    /// Set the Hardware address the interface will use.
    ///
    /// # Panics
    /// Creating the interface panics if the address is not unicast.
    pub hardware_addr: HardwareAddress,

    /// Set the IEEE802.15.4 PAN ID the interface will use.
    ///
    /// **NOTE**: we use the same PAN ID for destination and source.
    #[cfg(feature = "medium-ieee802154")]
    pub pan_id: Option<Ieee802154Pan>,

    /// Enable stateless address autoconfiguration on the interface.
    #[cfg(feature = "proto-ipv6")]
    pub slaac: bool,
}

impl Config {
    pub fn new(hardware_addr: HardwareAddress) -> Self {
        Config {
            random_seed: 0,
            hardware_addr,
            #[cfg(feature = "medium-ieee802154")]
            pan_id: None,
            #[cfg(feature = "proto-ipv6")]
            slaac: false,
        }
    }
}

impl Interface {
    /// Create a network interface using the previously provided configuration.
    ///
    /// # Panics
    /// This function panics if the [`Config::hardware_address`] does not match
    /// the medium of the device.
    pub fn new(config: Config, device: &mut (impl Device + ?Sized), now: Instant) -> Self {
        let caps = device.capabilities();
        assert_eq!(
            config.hardware_addr.medium(),
            caps.medium,
            "The hardware address does not match the medium of the interface."
        );

        let mut rand = Rand::new(config.random_seed);

        #[cfg(feature = "medium-ieee802154")]
        let mut sequence_no;
        #[cfg(feature = "medium-ieee802154")]
        loop {
            sequence_no = (rand.rand_u32() & 0xff) as u8;
            if sequence_no != 0 {
                break;
            }
        }

        #[cfg(feature = "proto-sixlowpan")]
        let mut tag;

        #[cfg(feature = "proto-sixlowpan")]
        loop {
            tag = rand.rand_u16();
            if tag != 0 {
                break;
            }
        }

        #[cfg(feature = "proto-ipv4")]
        let mut ipv4_id;

        #[cfg(feature = "proto-ipv4")]
        loop {
            ipv4_id = rand.rand_u16();
            if ipv4_id != 0 {
                break;
            }
        }

        Interface {
            fragments: FragmentsBuffer {
                #[cfg(feature = "proto-sixlowpan")]
                decompress_buf: [0u8; sixlowpan::MAX_DECOMPRESSED_LEN],

                #[cfg(feature = "_proto-fragmentation")]
                assembler: PacketAssemblerSet::new(),
                #[cfg(feature = "_proto-fragmentation")]
                reassembly_timeout: Duration::from_secs(60),
            },
            fragmenter: Fragmenter::new(),
            inner: InterfaceInner {
                now,
                caps,
                interface_index: 0,
                egress_route: None,
                #[cfg(feature = "proto-ipv4")]
                ingress_facts: None,
                reply_bound_interface: None,
                #[cfg(feature = "proto-ipv4")]
                ipv4_broadcast: None,
                hardware_addr: config.hardware_addr,
                ip_addrs: Vec::new(),
                any_ip: false,
                routes: Routes::new(),
                #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
                neighbor_cache: NeighborCache::new(),
                #[cfg(feature = "multicast")]
                multicast: multicast::State::new(),
                #[cfg(feature = "medium-ieee802154")]
                sequence_no,
                #[cfg(feature = "medium-ieee802154")]
                pan_id: config.pan_id,
                #[cfg(feature = "proto-sixlowpan-fragmentation")]
                tag,
                #[cfg(feature = "proto-ipv4-fragmentation")]
                ipv4_id,
                #[cfg(feature = "proto-sixlowpan")]
                sixlowpan_address_context: Vec::new(),
                #[cfg(feature = "proto-ipv6-slaac")]
                slaac_enabled: config.slaac,
                #[cfg(feature = "proto-ipv6-slaac")]
                slaac: Slaac::new(),
                #[cfg(feature = "proto-ipv6-slaac")]
                slaac_updated: Instant::from_millis(0),
                rand,
            },
        }
    }

    /// Get the socket context.
    ///
    /// The context is needed for some socket methods.
    pub fn context(&mut self) -> &mut InterfaceInner {
        &mut self.inner
    }

    /// 从同一实际设备owner刷新配置后的capabilities；medium不能在现存Interface上改变。
    /// cached caps仅用于本次协议分段/codec，不拥有另一份可配置MTU。
    pub fn set_device_capabilities(&mut self, caps: DeviceCapabilities) {
        assert_eq!(
            self.inner.caps.medium, caps.medium,
            "existing interface medium changed"
        );
        self.inner.caps = caps;
    }

    /// 装配实际设备identity；默认0供不要求设备范围的library caller使用。
    pub fn set_interface_index(&mut self, index: u32) {
        self.inner.interface_index = index;
    }

    /// 使用实际配置的IPv4广播地址，不从prefix重建明确设置的native地址。
    #[cfg(feature = "proto-ipv4")]
    pub fn set_ipv4_broadcast_address(&mut self, address: Option<Ipv4Address>) {
        self.inner.ipv4_broadcast = address;
    }

    /// Get the HardwareAddress address of the interface.
    ///
    /// # Panics
    /// This function panics if the medium is not Ethernet or Ieee802154.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn hardware_addr(&self) -> HardwareAddress {
        #[cfg(all(feature = "medium-ethernet", not(feature = "medium-ieee802154")))]
        assert!(self.inner.caps.medium == Medium::Ethernet);
        #[cfg(all(feature = "medium-ieee802154", not(feature = "medium-ethernet")))]
        assert!(self.inner.caps.medium == Medium::Ieee802154);

        #[cfg(all(feature = "medium-ieee802154", feature = "medium-ethernet"))]
        assert!(
            self.inner.caps.medium == Medium::Ethernet
                || self.inner.caps.medium == Medium::Ieee802154
        );

        self.inner.hardware_addr
    }

    /// Set the HardwareAddress address of the interface.
    ///
    /// # Panics
    /// This function panics if the address is not unicast, and if the medium is not Ethernet or
    /// Ieee802154.
    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    pub fn set_hardware_addr(&mut self, addr: HardwareAddress) {
        #[cfg(all(feature = "medium-ethernet", not(feature = "medium-ieee802154")))]
        assert!(self.inner.caps.medium == Medium::Ethernet);
        #[cfg(all(feature = "medium-ieee802154", not(feature = "medium-ethernet")))]
        assert!(self.inner.caps.medium == Medium::Ieee802154);

        #[cfg(all(feature = "medium-ieee802154", feature = "medium-ethernet"))]
        assert!(
            self.inner.caps.medium == Medium::Ethernet
                || self.inner.caps.medium == Medium::Ieee802154
        );

        InterfaceInner::check_hardware_addr(&addr);
        self.inner.hardware_addr = addr;
    }

    /// Get the IP addresses of the interface.
    pub fn ip_addrs(&self) -> &[IpCidr] {
        self.inner.ip_addrs.as_ref()
    }

    /// Get the first IPv4 address if present.
    #[cfg(feature = "proto-ipv4")]
    pub fn ipv4_addr(&self) -> Option<Ipv4Address> {
        self.inner.ipv4_addr()
    }

    /// Get the first IPv6 address if present.
    #[cfg(feature = "proto-ipv6")]
    pub fn ipv6_addr(&self) -> Option<Ipv6Address> {
        self.inner.ipv6_addr()
    }

    /// Get an address from the interface that could be used as source address.
    /// For IPv4, this function tries to find a registered IPv4 address in the same
    /// subnet as the destination, falling back to the first IPv4 address if none is
    /// found. For IPv6, the selection is based on RFC6724.
    pub fn get_source_address(&self, dst_addr: &IpAddress) -> Option<IpAddress> {
        self.inner.get_source_address(dst_addr)
    }

    /// Get an IPv4 source address based on a destination address. This function tries
    /// to find the first IPv4 address from the interface that is in the same subnet as
    /// the destination address. If no such address is found, the first IPv4 address
    /// from the interface is returned.
    #[cfg(feature = "proto-ipv4")]
    pub fn get_source_address_ipv4(&self, dst_addr: &Ipv4Address) -> Option<Ipv4Address> {
        self.inner.get_source_address_ipv4(dst_addr)
    }

    /// Get an address from the interface that could be used as source address. The selection is
    /// based on RFC6724.
    #[cfg(feature = "proto-ipv6")]
    pub fn get_source_address_ipv6(&self, dst_addr: &Ipv6Address) -> Ipv6Address {
        self.inner.get_source_address_ipv6(dst_addr)
    }

    /// Update the IP addresses of the interface.
    ///
    /// # Panics
    /// This function panics if any of the addresses are not unicast.
    pub fn update_ip_addrs<F: FnOnce(&mut Vec<IpCidr, IFACE_MAX_ADDR_COUNT>)>(&mut self, f: F) {
        f(&mut self.inner.ip_addrs);
        InterfaceInner::flush_neighbor_cache(&mut self.inner);
        InterfaceInner::check_ip_addrs(&self.inner.ip_addrs);

        #[cfg(all(
            feature = "proto-ipv6",
            feature = "multicast",
            feature = "medium-ethernet"
        ))]
        if self.inner.caps.medium == Medium::Ethernet {
            self.update_solicited_node_groups();
        }
    }

    /// Check whether the interface has the given IP address assigned.
    pub fn has_ip_addr<T: Into<IpAddress>>(&self, addr: T) -> bool {
        self.inner.has_ip_addr(addr)
    }

    pub fn routes(&self) -> &Routes {
        &self.inner.routes
    }

    pub fn routes_mut(&mut self) -> &mut Routes {
        &mut self.inner.routes
    }

    /// Enable or disable the AnyIP capability.
    ///
    /// AnyIP allowins packets to be received
    /// locally on IP addresses other than the interface's configured [ip_addrs].
    /// When AnyIP is enabled and a route prefix in [`routes`](Self::routes) specifies one of
    /// the interface's [`ip_addrs`](Self::ip_addrs) as its gateway, the interface will accept
    /// packets addressed to that prefix.
    pub fn set_any_ip(&mut self, any_ip: bool) {
        self.inner.any_ip = any_ip;
    }

    /// Get whether AnyIP is enabled.
    ///
    /// See [`set_any_ip`](Self::set_any_ip) for details on AnyIP
    pub fn any_ip(&self) -> bool {
        self.inner.any_ip
    }

    /// Get the packet reassembly timeout.
    #[cfg(feature = "_proto-fragmentation")]
    pub fn reassembly_timeout(&self) -> Duration {
        self.fragments.reassembly_timeout
    }

    /// Set the packet reassembly timeout.
    #[cfg(feature = "_proto-fragmentation")]
    pub fn set_reassembly_timeout(&mut self, timeout: Duration) {
        if timeout > Duration::from_secs(60) {
            net_debug!(
                "RFC 4944 specifies that the reassembly timeout MUST be set to a maximum of 60 seconds"
            );
        }
        self.fragments.reassembly_timeout = timeout;
    }

    /// Transmit packets queued in the sockets, and receive packets queued
    /// in the device.
    ///
    /// This function returns a value indicating whether the state of any socket
    /// might have changed.
    ///
    /// ## DoS warning
    ///
    /// This function processes all packets in the device's queue. This can
    /// be an unbounded amount of work if packets arrive faster than they're
    /// processed.
    ///
    /// If this is a concern for your application (i.e. your environment doesn't
    /// have preemptive scheduling, or `poll()` is called from a main loop where
    /// other important things are processed), you may use the lower-level methods
    /// [`poll_egress()`](Self::poll_egress), [`poll_maintenance()`](Self::poll_maintenance)
    /// and [`poll_ingress_single()`](Self::poll_ingress_single).
    /// This allows you to insert yields or process other events between processing
    /// individual ingress packets.
    pub fn poll(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
    ) -> PollResult {
        self.inner.now = timestamp;

        let mut res = PollResult::None;

        self.poll_maintenance(timestamp);

        // Process ingress while there's packets available.
        loop {
            match self.socket_ingress(device, sockets) {
                PollIngressSingleResult::None => break,
                PollIngressSingleResult::PacketProcessed => {}
                PollIngressSingleResult::SocketStateChanged => res = PollResult::SocketStateChanged,
            }
        }

        // Process egress.
        loop {
            match self.poll_egress(timestamp, device, sockets) {
                PollResult::None => break,
                PollResult::SocketStateChanged => res = PollResult::SocketStateChanged,
            }
        }

        res
    }

    /// Transmit packets queued in the sockets.
    ///
    /// This function returns a value indicating whether the state of any socket
    /// might have changed.
    ///
    /// This is guaranteed to always perform a bounded amount of work.
    pub fn poll_egress(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
    ) -> PollResult {
        self.poll_egress_filtered(timestamp, device, sockets, |_, _, _| true)
    }

    /// 唯一kernel route owner在dispatch之前选择实际设备；未选中不能修改TCP/queue。
    pub fn poll_egress_filtered(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
        permitted: impl FnMut(crate::iface::SocketHandle, &mut Socket<'_>, &mut InterfaceInner) -> bool,
    ) -> PollResult {
        self.inner.now = timestamp;

        match self.inner.caps.medium {
            #[cfg(feature = "medium-ieee802154")]
            Medium::Ieee802154 => {
                #[cfg(feature = "proto-sixlowpan-fragmentation")]
                self.sixlowpan_egress(device);
            }
            #[cfg(any(feature = "medium-ethernet", feature = "medium-ip"))]
            _ => {
                #[cfg(feature = "proto-ipv4-fragmentation")]
                self.ipv4_egress(device);
            }
        }

        #[cfg(feature = "proto-ipv6-slaac")]
        if self.inner.slaac_enabled {
            self.ndisc_rs_egress(device);
        }

        #[cfg(feature = "multicast")]
        self.multicast_egress(device);

        let result = self.socket_egress(device, sockets, permitted);
        self.inner.egress_route = None;
        result
    }

    /// Process one incoming packet queued in the device.
    ///
    /// Returns a value indicating:
    /// - whether a packet was processed, in which case you have to call this method again in case there's more packets queued.
    /// - whether the state of any socket might have changed.
    ///
    /// Since it processes at most one packet, this is guaranteed to always perform a bounded amount of work.
    pub fn poll_ingress_single(
        &mut self,
        timestamp: Instant,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
    ) -> PollIngressSingleResult {
        self.inner.now = timestamp;

        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.remove_expired(timestamp);

        self.socket_ingress(device, sockets)
    }

    /// Maintain stateful processing on the device.
    ///
    /// This is guaranteed to always perform a bounded amount of work.
    pub fn poll_maintenance(&mut self, timestamp: Instant) {
        self.inner.now = timestamp;

        #[cfg(feature = "_proto-fragmentation")]
        self.fragments.assembler.remove_expired(timestamp);

        #[cfg(feature = "proto-ipv6-slaac")]
        if self.inner.slaac.sync_required(timestamp) {
            self.sync_slaac_state(timestamp)
        }
    }

    /// Return a _soft deadline_ for calling [poll] the next time.
    /// The [Instant] returned is the time at which you should call [poll] next.
    /// It is harmless (but wastes energy) to call it before the [Instant], and
    /// potentially harmful (impacting quality of service) to call it after the
    /// [Instant]
    ///
    /// [poll]: #method.poll
    /// [Instant]: struct.Instant.html
    pub fn poll_at(&mut self, timestamp: Instant, sockets: &SocketSet<'_>) -> Option<Instant> {
        self.poll_at_filtered(timestamp, sockets, |_, _, _| true)
    }
    /// timer与egress借用同一外部FIB事实；不能用空的per-interface Routes重判static next-hop。
    pub fn poll_at_filtered(
        &mut self,
        timestamp: Instant,
        sockets: &SocketSet<'_>,
        mut permitted: impl FnMut(crate::iface::SocketHandle, &Socket<'_>, &mut InterfaceInner) -> bool,
    ) -> Option<Instant> {
        self.inner.now = timestamp;

        #[cfg(feature = "_proto-fragmentation")]
        if !self.fragmenter.is_empty() {
            return Some(Instant::from_millis(0));
        }

        #[allow(unused_mut)]
        let mut res = sockets
            .items()
            .filter_map(|item| {
                self.inner.egress_route = None;
                if !permitted(item.meta.handle, &item.socket, &mut self.inner) {
                    return None;
                }

                let socket_poll_at = item.socket.poll_at(&mut self.inner);
                match item.meta.poll_at(
                    socket_poll_at,
                    |ip_addr| self.inner.has_neighbor(&ip_addr),
                    timestamp,
                ) {
                    PollAt::Ingress => None,
                    PollAt::Time(instant) => Some(instant),
                    PollAt::Now => Some(Instant::from_millis(0)),
                }
            })
            .min();

        #[cfg(feature = "proto-ipv6-slaac")]
        if self.inner.slaac_enabled {
            res = res.min(self.inner.slaac.poll_at(timestamp));
        }

        self.inner.egress_route = None;
        res
    }

    /// Return an _advisory wait time_ for calling [poll] the next time.
    /// The [Duration] returned is the time left to wait before calling [poll] next.
    /// It is harmless (but wastes energy) to call it before the [Duration] has passed,
    /// and potentially harmful (impacting quality of service) to call it after the
    /// [Duration] has passed.
    ///
    /// [poll]: #method.poll
    /// [Duration]: struct.Duration.html
    pub fn poll_delay(&mut self, timestamp: Instant, sockets: &SocketSet<'_>) -> Option<Duration> {
        match self.poll_at(timestamp, sockets) {
            Some(poll_at) if timestamp < poll_at => Some(poll_at - timestamp),
            Some(_) => Some(Duration::from_millis(0)),
            _ => None,
        }
    }

    fn socket_ingress(
        &mut self,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
    ) -> PollIngressSingleResult {
        let Some((rx_token, tx_token)) = device.receive(self.inner.now) else {
            return PollIngressSingleResult::None;
        };

        self.poll_ingress_received(sockets, rx_token, tx_token, |_, _| None, |_| false)
    }

    /// 消费已经准备的actual tokens；普通poll和kernel routed ingress共享一个解析/dispatch算法。
    /// routing在完整协议校验之前只投影header事实，协议有效性仍由原parser拥有。
    #[cfg(feature = "proto-ipv4")]
    pub fn poll_ingress_received<Rx: RxToken, Tx: TxToken>(
        &mut self,
        sockets: &mut SocketSet<'_>,
        rx_token: Rx,
        tx_token: Tx,
        mut routing: impl FnMut(&[u8], PacketMeta) -> Option<Ipv4IngressFacts>,
        mut reply: impl FnMut(&Ipv4Reply<'_>) -> bool,
    ) -> PollIngressSingleResult {
        let rx_meta = rx_token.meta();
        let result = rx_token.consume(|frame| {
            if frame.is_empty() {
                return PollIngressSingleResult::PacketProcessed;
            }
            self.inner.reply_bound_interface = None;
            self.inner.ingress_facts = routing(frame, rx_meta);
            match self.inner.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => {
                    if let Some(packet) =
                        self.inner
                            .process_ethernet(sockets, rx_meta, frame, &mut self.fragments)
                    {
                        let packet = match packet {
                            EthernetPacket::Ip(packet) => {
                                let wrapper = Ipv4Reply {
                                    packet,
                                    caps: self.inner.caps.clone(),
                                    bound_interface: self.inner.reply_bound_interface,
                                };
                                if reply(&wrapper) {
                                    return PollIngressSingleResult::SocketStateChanged;
                                }
                                EthernetPacket::Ip(wrapper.packet)
                            }
                            packet => packet,
                        };
                        if let Err(err) =
                            self.inner.dispatch(tx_token, packet, &mut self.fragmenter)
                        {
                            net_debug!("Failed to send response: {:?}", err);
                        }
                    }
                }
                #[cfg(feature = "medium-ip")]
                Medium::Ip => {
                    if let Some(packet) =
                        self.inner
                            .process_ip(sockets, rx_meta, frame, &mut self.fragments)
                    {
                        let wrapper = Ipv4Reply {
                            packet,
                            caps: self.inner.caps.clone(),
                            bound_interface: self.inner.reply_bound_interface,
                        };
                        if !reply(&wrapper)
                            && let Err(err) = self.inner.dispatch_ip(
                                tx_token,
                                PacketMeta::default(),
                                wrapper.packet,
                                &mut self.fragmenter,
                            )
                        {
                            net_debug!("Failed to send response: {:?}", err);
                        }
                    }
                }
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => {
                    if let Some(packet) =
                        self.inner
                            .process_ieee802154(sockets, rx_meta, frame, &mut self.fragments)
                        && let Err(err) = self.inner.dispatch_ip(
                            tx_token,
                            PacketMeta::default(),
                            packet,
                            &mut self.fragmenter,
                        )
                    {
                        net_debug!("Failed to send response: {:?}", err);
                    }
                }
            }
            PollIngressSingleResult::SocketStateChanged
        });
        self.inner.ingress_facts = None;
        self.inner.reply_bound_interface = None;
        result
    }

    /// 同一原Packet/neighbor/dispatch机制发送有预算的kernel回复，失败保留caller队列。
    #[cfg(feature = "proto-ipv4")]
    pub fn dispatch_ipv4_packet<Tx: TxToken>(
        &mut self,
        token: Tx,
        packet: &[u8],
        next_hop: Ipv4Address,
        logical_interface: u32,
    ) -> bool {
        let Ok(ip) = Ipv4Packet::new_checked(packet) else {
            return true;
        };
        let Ok(repr) = Ipv4Repr::parse(&ip, &ChecksumCapabilities::default()) else {
            return true;
        };
        if packet.len() > self.inner.caps.ip_mtu() {
            return true;
        }
        self.inner
            .set_egress_route(next_hop.into(), logical_interface, false, None);
        let result = self
            .inner
            .dispatch_ip(
                token,
                PacketMeta::default(),
                Packet::new_ipv4(repr, IpPayload::Raw(ip.payload())),
                &mut self.fragmenter,
            )
            .is_ok();
        self.inner.egress_route = None;
        result
    }

    fn socket_egress(
        &mut self,
        device: &mut (impl Device + ?Sized),
        sockets: &mut SocketSet<'_>,
        mut permitted: impl FnMut(
            crate::iface::SocketHandle,
            &mut Socket<'_>,
            &mut InterfaceInner,
        ) -> bool,
    ) -> PollResult {
        let _caps = device.capabilities();

        #[derive(Clone, Copy)]
        enum EgressError {
            Exhausted,
            Dispatch(DispatchError),
        }

        #[cfg(feature = "socket-tcp")]
        impl From<EgressError> for crate::socket::tcp::OutputFailure {
            fn from(failure: EgressError) -> Self {
                match failure {
                    EgressError::Exhausted => Self::LocalCongestion,
                    EgressError::Dispatch(_) => Self::PathUnavailable,
                }
            }
        }

        let mut result = PollResult::None;
        for item in sockets.items_mut() {
            // 必须早于neighbor bookkeeping及transport dispatch；错误iface不能reset TCP。
            self.inner.egress_route = None;
            if !permitted(item.meta.handle, &mut item.socket, &mut self.inner) {
                continue;
            }
            if !item
                .meta
                .egress_permitted(self.inner.now, |ip_addr| self.inner.has_neighbor(&ip_addr))
            {
                #[cfg(feature = "socket-tcp")]
                if let Socket::Tcp(socket) = &mut item.socket {
                    // 邻居冷却不能留下已经到期的探测造成 poll busy-loop。
                    socket.output_unavailable(
                        self.inner.now,
                        crate::socket::tcp::OutputFailure::PathUnavailable,
                    );
                }
                continue;
            }

            let mut neighbor_addr = None;
            let mut respond = |inner: &mut InterfaceInner,
                               meta: PacketMeta,
                               response: Packet,
                               preserved: Option<&[u8]>| {
                neighbor_addr = Some(response.ip_repr().dst_addr());
                let t = device.transmit(inner.now).ok_or_else(|| {
                    net_debug!("failed to transmit IP: device exhausted");
                    EgressError::Exhausted
                })?;

                inner
                    .dispatch_ip_frame(t, meta, response, preserved, &mut self.fragmenter)
                    .map_err(EgressError::Dispatch)?;

                result = PollResult::SocketStateChanged;

                Ok(())
            };

            let result = match &mut item.socket {
                #[cfg(feature = "socket-raw")]
                Socket::Raw(socket) => {
                    socket.dispatch(&mut self.inner, |inner, (ip, raw, preserved)| {
                        respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new(ip, IpPayload::Raw(raw)),
                            preserved,
                        )
                    })
                }
                #[cfg(feature = "socket-icmp")]
                Socket::Icmp(socket) => {
                    socket.dispatch(&mut self.inner, |inner, response| match response {
                        #[cfg(feature = "proto-ipv4")]
                        (IpRepr::Ipv4(ipv4_repr), IcmpRepr::Ipv4(icmpv4_repr)) => respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv4(ipv4_repr, IpPayload::Icmpv4(icmpv4_repr)),
                            None,
                        ),
                        #[cfg(feature = "proto-ipv6")]
                        (IpRepr::Ipv6(ipv6_repr), IcmpRepr::Ipv6(icmpv6_repr)) => respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv6(ipv6_repr, IpPayload::Icmpv6(icmpv6_repr)),
                            None,
                        ),
                        #[allow(unreachable_patterns)]
                        _ => unreachable!(),
                    })
                }
                #[cfg(feature = "socket-udp")]
                Socket::Udp(socket) => {
                    socket.dispatch(&mut self.inner, |inner, meta, (ip, udp, payload)| {
                        respond(
                            inner,
                            meta,
                            Packet::new(ip, IpPayload::Udp(udp, payload)),
                            None,
                        )
                    })
                }
                #[cfg(feature = "socket-tcp")]
                Socket::Tcp(socket) => socket.dispatch(&mut self.inner, |inner, (ip, tcp)| {
                    respond(
                        inner,
                        PacketMeta::default(),
                        Packet::new(ip, IpPayload::Tcp(tcp)),
                        None,
                    )
                }),
                #[cfg(feature = "socket-dhcpv4")]
                Socket::Dhcpv4(socket) => {
                    socket.dispatch(&mut self.inner, |inner, (ip, udp, dhcp)| {
                        respond(
                            inner,
                            PacketMeta::default(),
                            Packet::new_ipv4(ip, IpPayload::Dhcpv4(udp, dhcp)),
                            None,
                        )
                    })
                }
                #[cfg(feature = "socket-dns")]
                Socket::Dns(socket) => socket.dispatch(&mut self.inner, |inner, (ip, udp, dns)| {
                    respond(
                        inner,
                        PacketMeta::default(),
                        Packet::new(ip, IpPayload::Udp(udp, dns)),
                        None,
                    )
                }),
            };

            match result {
                Err(EgressError::Exhausted) => break, // Device buffer full.
                Err(EgressError::Dispatch(DispatchError::NeighborPending)) => {
                    // `NeighborCache` already takes care of rate limiting the neighbor discovery
                    // requests from the socket. However, without an additional rate limiting
                    // mechanism, we would spin on every socket that has yet to discover its
                    // neighbor.
                    item.meta.neighbor_missing(
                        self.inner.now,
                        neighbor_addr.expect("non-IP response packet"),
                    );
                }
                Err(EgressError::Dispatch(DispatchError::NoRoute)) => {}
                Ok(()) => {}
            }
        }
        result
    }
}

impl InterfaceInner {
    /// 当前实际ingress设备identity；由owning SocketSet caller装配，不从Task查询。
    pub(crate) fn interface_index(&self) -> u32 {
        #[cfg(feature = "proto-ipv4")]
        if let Some(facts) = self.ingress_facts {
            return facts.interface_index;
        }
        self.interface_index
    }
    /// 本次唯一外部FIB选择；socket filter在任何neighbor/transport mutation之前设置。
    pub fn set_egress_route(
        &mut self,
        next_hop: IpAddress,
        logical_interface: u32,
        broadcast: bool,
        transport: Option<IpRouteContext>,
    ) {
        self.egress_route = Some((next_hop, logical_interface, broadcast, transport));
    }
    /// 结束直接active-open的临时FIB投影；普通poll在每slot结束也清除同一字段。
    pub fn clear_egress_route(&mut self) {
        self.egress_route = None;
    }
    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn now(&self) -> Instant {
        self.now
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn hardware_addr(&self) -> HardwareAddress {
        self.hardware_addr
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn checksum_caps(&self) -> ChecksumCapabilities {
        self.caps.checksum.clone()
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn ip_mtu(&self) -> usize {
        if let Some(route) = self.transport_route() {
            return route.mtu;
        }
        self.caps.ip_mtu()
    }

    pub(crate) fn transport_route(&self) -> Option<IpRouteContext> {
        #[cfg(feature = "proto-ipv4")]
        if let Some(facts) = self.ingress_facts {
            return if self.reply_bound_interface.is_some() {
                facts.bound_reverse_route
            } else {
                facts.reverse_route
            };
        }
        self.egress_route.and_then(|(_, _, _, route)| route)
    }

    #[allow(unused)] // unused depending on which sockets are enabled, and in tests
    pub(crate) fn rand(&mut self) -> &mut Rand {
        &mut self.rand
    }

    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn get_source_address(&self, dst_addr: &IpAddress) -> Option<IpAddress> {
        match dst_addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(addr) => self.get_source_address_ipv4(addr).map(|a| a.into()),
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(addr) => Some(self.get_source_address_ipv6(addr).into()),
        }
    }

    /// 入队时冻结IP_PKTINFO specific destination；单播LOCAL保持原目的地址，
    /// 广播/组播从实际入口的源地址选择机制取值，不能在recv时读取已更改的地址。
    #[cfg(any(feature = "socket-udp", feature = "socket-raw"))]
    pub(crate) fn ingress_spec_address(&self, repr: &IpRepr) -> Option<IpAddress> {
        #[cfg(feature = "proto-ipv4")]
        if let Some(facts) = self.ingress_facts {
            return facts.specific_destination.map(IpAddress::Ipv4);
        }
        let destination = repr.dst_addr();
        if !self.is_broadcast(&destination) && !destination.is_multicast() {
            Some(destination)
        } else {
            self.get_source_address(&repr.src_addr())
        }
    }

    #[cfg(test)]
    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn set_now(&mut self, now: Instant) {
        self.now = now
    }

    #[cfg(test)]
    #[allow(unused)] // unused depending on which sockets are enabled
    pub(crate) fn set_ip_addrs(&mut self, addrs: Vec<IpCidr, IFACE_MAX_ADDR_COUNT>) {
        self.ip_addrs = addrs;
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    fn check_hardware_addr(addr: &HardwareAddress) {
        if !addr.is_unicast() {
            panic!("Hardware address {addr} is not unicast")
        }
    }

    fn check_ip_addrs(addrs: &[IpCidr]) {
        for cidr in addrs {
            let valid = match cidr {
                // native inet_abc_len接受limited broadcast/Class E；组播仍不作为local地址。
                #[cfg(feature = "proto-ipv4")]
                IpCidr::Ipv4(cidr) => !cidr.address().is_multicast(),
                #[cfg(feature = "proto-ipv6")]
                IpCidr::Ipv6(cidr) => {
                    cidr.address().is_unicast() || cidr.address().is_unspecified()
                }
            };
            assert!(valid, "IP address {} is multicast", cidr.address());
        }
    }

    /// Check whether the interface has the given IP address assigned.
    ///
    /// Always returns true if [`InterfaceInner::any_ip`].
    pub(crate) fn has_ip_addr<T: Into<IpAddress>>(&self, addr: T) -> bool {
        // If any IP is set to true, we don't bother about checking the IP.
        if self.any_ip {
            return true;
        }

        let addr = addr.into();
        self.ip_addrs.iter().any(|probe| probe.address() == addr)
    }

    /// Check whether the interface listens to given destination multicast IP address.
    fn has_multicast_group<T: Into<IpAddress>>(&self, addr: T) -> bool {
        let addr = addr.into();

        #[cfg(feature = "multicast")]
        if self.multicast.has_multicast_group(addr) {
            return true;
        }

        match addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(key) => key == IPV4_MULTICAST_ALL_SYSTEMS,
            #[cfg(feature = "proto-rpl")]
            IpAddress::Ipv6(IPV6_LINK_LOCAL_ALL_RPL_NODES) => true,
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(key) => {
                key == IPV6_LINK_LOCAL_ALL_NODES || self.has_solicited_node(key)
            }
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    #[cfg(feature = "medium-ip")]
    fn process_ip<'frame>(
        &mut self,
        sockets: &mut SocketSet,
        meta: PacketMeta,
        ip_payload: &'frame [u8],
        frag: &'frame mut FragmentsBuffer,
    ) -> Option<Packet<'frame>> {
        match IpVersion::of_packet(ip_payload) {
            #[cfg(feature = "proto-ipv4")]
            Ok(IpVersion::Ipv4) => {
                let ipv4_packet = check!(Ipv4Packet::new_checked(ip_payload));
                self.process_ipv4(sockets, meta, HardwareAddress::Ip, &ipv4_packet, frag)
            }
            #[cfg(feature = "proto-ipv6")]
            Ok(IpVersion::Ipv6) => {
                let ipv6_packet = check!(Ipv6Packet::new_checked(ip_payload));
                self.process_ipv6(sockets, meta, HardwareAddress::Ip, &ipv6_packet)
            }
            // Drop all other traffic.
            _ => None,
        }
    }

    // IPv6 输入没有 IPv4 wire header，仍进入同一个 RAW enqueue/filter engine。
    #[cfg(all(feature = "socket-raw", feature = "proto-ipv6"))]
    fn raw_socket_filter(
        &mut self,
        sockets: &mut SocketSet,
        ip_repr: &IpRepr,
        ip_payload: &[u8],
    ) -> bool {
        self.raw_socket_filter_frame(sockets, ip_repr, ip_payload, None)
    }

    #[cfg(feature = "socket-raw")]
    fn raw_socket_filter_frame(
        &mut self,
        sockets: &mut SocketSet,
        ip_repr: &IpRepr,
        ip_payload: &[u8],
        preserved: Option<&[u8]>,
    ) -> bool {
        let mut handled_by_raw_socket = false;

        // Pass every IP packet to all raw sockets we have registered.
        for raw_socket in sockets
            .items_mut()
            .filter_map(|i| raw::Socket::downcast_mut(&mut i.socket))
        {
            if raw_socket.accepts(ip_repr) && raw_socket.accepts_interface(self.interface_index()) {
                raw_socket.process(self, ip_repr, ip_payload, preserved);
                handled_by_raw_socket = true;
            }
        }
        handled_by_raw_socket
    }

    /// Checks if an address is broadcast, taking into account ipv4 subnet-local
    /// broadcast addresses.
    pub(crate) fn is_broadcast(&self, address: &IpAddress) -> bool {
        match address {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(address) => self.is_broadcast_v4(*address),
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(_) => false,
        }
    }

    #[cfg(feature = "medium-ethernet")]
    fn dispatch<Tx>(
        &mut self,
        tx_token: Tx,
        packet: EthernetPacket,
        frag: &mut Fragmenter,
    ) -> Result<(), DispatchError>
    where
        Tx: TxToken,
    {
        match packet {
            #[cfg(feature = "proto-ipv4")]
            EthernetPacket::Arp(arp_repr) => {
                let dst_hardware_addr = match arp_repr {
                    ArpRepr::EthernetIpv4 {
                        target_hardware_addr,
                        ..
                    } => target_hardware_addr,
                };

                self.dispatch_ethernet(tx_token, arp_repr.buffer_len(), |mut frame| {
                    frame.set_dst_addr(dst_hardware_addr);
                    frame.set_ethertype(EthernetProtocol::Arp);

                    let mut packet = ArpPacket::new_unchecked(frame.payload_mut());
                    arp_repr.emit(&mut packet);
                })
            }
            EthernetPacket::Ip(packet) => {
                self.dispatch_ip(tx_token, PacketMeta::default(), packet, frag)
            }
        }
    }

    fn in_same_network(&self, addr: &IpAddress) -> bool {
        self.ip_addrs.iter().any(|cidr| cidr.contains_addr(addr))
    }

    fn route(&self, addr: &IpAddress, timestamp: Instant) -> Option<IpAddress> {
        if let Some((next_hop, _, _, _)) = self.egress_route {
            return Some(next_hop);
        }
        // Send directly.
        // note: no need to use `self.is_broadcast()` to check for subnet-local broadcast addrs
        //       here because `in_same_network` will already return true.
        if self.in_same_network(addr) || addr.is_broadcast() {
            return Some(*addr);
        }

        // Route via a router.
        self.routes.lookup(addr, timestamp)
    }

    fn has_neighbor(&self, addr: &IpAddress) -> bool {
        match self.route(addr, self.now) {
            Some(_routed_addr) => match self.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => self.neighbor_cache.lookup(&_routed_addr, self.now).found(),
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => self.neighbor_cache.lookup(&_routed_addr, self.now).found(),
                #[cfg(feature = "medium-ip")]
                Medium::Ip => true,
            },
            None => false,
        }
    }

    #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
    fn lookup_hardware_addr<Tx>(
        &mut self,
        tx_token: Tx,
        dst_addr: &IpAddress,
        _fragmenter: &mut Fragmenter,
    ) -> Result<(HardwareAddress, Tx), DispatchError>
    where
        Tx: TxToken,
    {
        if self.is_broadcast(dst_addr) {
            let hardware_addr = match self.caps.medium {
                #[cfg(feature = "medium-ethernet")]
                Medium::Ethernet => HardwareAddress::Ethernet(EthernetAddress::BROADCAST),
                #[cfg(feature = "medium-ieee802154")]
                Medium::Ieee802154 => HardwareAddress::Ieee802154(Ieee802154Address::BROADCAST),
                #[cfg(feature = "medium-ip")]
                Medium::Ip => unreachable!(),
            };

            return Ok((hardware_addr, tx_token));
        }

        if dst_addr.is_multicast() {
            let hardware_addr = match *dst_addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(addr) => match self.caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let b = addr.octets();
                        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
                            0x01,
                            0x00,
                            0x5e,
                            b[1] & 0x7F,
                            b[2],
                            b[3],
                        ]))
                    }
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => unreachable!(),
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => unreachable!(),
                },
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(addr) => match self.caps.medium {
                    #[cfg(feature = "medium-ethernet")]
                    Medium::Ethernet => {
                        let b = addr.octets();
                        HardwareAddress::Ethernet(EthernetAddress::from_bytes(&[
                            0x33, 0x33, b[12], b[13], b[14], b[15],
                        ]))
                    }
                    #[cfg(feature = "medium-ieee802154")]
                    Medium::Ieee802154 => {
                        // Not sure if this is correct
                        HardwareAddress::Ieee802154(Ieee802154Address::BROADCAST)
                    }
                    #[cfg(feature = "medium-ip")]
                    Medium::Ip => unreachable!(),
                },
            };

            return Ok((hardware_addr, tx_token));
        }

        let dst_addr = self
            .route(dst_addr, self.now)
            .ok_or(DispatchError::NoRoute)?;

        match self.neighbor_cache.lookup(&dst_addr, self.now) {
            NeighborAnswer::Found(hardware_addr) => return Ok((hardware_addr, tx_token)),
            NeighborAnswer::RateLimited => return Err(DispatchError::NeighborPending),
            _ => (), // XXX
        }

        match dst_addr {
            #[cfg(all(feature = "medium-ethernet", feature = "proto-ipv4"))]
            IpAddress::Ipv4(dst_addr) if matches!(self.caps.medium, Medium::Ethernet) => {
                net_debug!(
                    "address {} not in neighbor cache, sending ARP request",
                    dst_addr
                );
                let src_hardware_addr = self.hardware_addr.ethernet_or_panic();

                let arp_repr = ArpRepr::EthernetIpv4 {
                    operation: ArpOperation::Request,
                    source_hardware_addr: src_hardware_addr,
                    source_protocol_addr: self
                        .get_source_address_ipv4(&dst_addr)
                        .ok_or(DispatchError::NoRoute)?,
                    target_hardware_addr: EthernetAddress::BROADCAST,
                    target_protocol_addr: dst_addr,
                };

                if let Err(e) =
                    self.dispatch_ethernet(tx_token, arp_repr.buffer_len(), |mut frame| {
                        frame.set_dst_addr(EthernetAddress::BROADCAST);
                        frame.set_ethertype(EthernetProtocol::Arp);

                        arp_repr.emit(&mut ArpPacket::new_unchecked(frame.payload_mut()))
                    })
                {
                    net_debug!("Failed to dispatch ARP request: {:?}", e);
                    return Err(DispatchError::NeighborPending);
                }
            }

            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(dst_addr) => {
                net_debug!(
                    "address {} not in neighbor cache, sending Neighbor Solicitation",
                    dst_addr
                );

                let solicit = Icmpv6Repr::Ndisc(NdiscRepr::NeighborSolicit {
                    target_addr: dst_addr,
                    lladdr: Some(self.hardware_addr.into()),
                });

                let packet = Packet::new_ipv6(
                    Ipv6Repr {
                        src_addr: self.get_source_address_ipv6(&dst_addr),
                        dst_addr: dst_addr.solicited_node(),
                        next_header: IpProtocol::Icmpv6,
                        payload_len: solicit.buffer_len(),
                        hop_limit: 0xff,
                    },
                    IpPayload::Icmpv6(solicit),
                );

                if let Err(e) =
                    self.dispatch_ip(tx_token, PacketMeta::default(), packet, _fragmenter)
                {
                    net_debug!("Failed to dispatch NDISC solicit: {:?}", e);
                    return Err(DispatchError::NeighborPending);
                }
            }

            #[allow(unreachable_patterns)]
            _ => (),
        }

        // The request got dispatched, limit the rate on the cache.
        self.neighbor_cache.limit_rate(self.now);
        Err(DispatchError::NeighborPending)
    }

    fn flush_neighbor_cache(&mut self) {
        #[cfg(any(feature = "medium-ethernet", feature = "medium-ieee802154"))]
        self.neighbor_cache.flush()
    }

    fn dispatch_ip<Tx: TxToken>(
        &mut self,
        tx_token: Tx,
        meta: PacketMeta,
        packet: Packet,
        frag: &mut Fragmenter,
    ) -> Result<(), DispatchError> {
        self.dispatch_ip_frame(tx_token, meta, packet, None, frag)
    }

    // 唯一 IP/neighbor/link dispatch owner；RAW 提供已验证的完整基础 header，
    // 普通协议提供原 Packet codec，二者不另建 TX/ARP/FIB 或绕过设备回压。
    fn dispatch_ip_frame<Tx: TxToken>(
        &mut self,
        // NOTE(unused_mut): tx_token isn't always mutated, depending on
        // the feature set that is used.
        #[allow(unused_mut)] mut tx_token: Tx,
        mut meta: PacketMeta,
        packet: Packet,
        preserved: Option<&[u8]>,
        frag: &mut Fragmenter,
    ) -> Result<(), DispatchError> {
        if let Some((_, logical, _, _)) = self.egress_route {
            meta.interface_index = Some(logical);
        }
        tx_token.set_meta(meta);
        let mut ip_repr = packet.ip_repr();
        assert!(!ip_repr.dst_addr().is_unspecified());

        // Dispatch IEEE802.15.4:

        #[cfg(feature = "medium-ieee802154")]
        if matches!(self.caps.medium, Medium::Ieee802154) {
            let (addr, tx_token) =
                self.lookup_hardware_addr(tx_token, &ip_repr.dst_addr(), frag)?;
            let addr = addr.ieee802154_or_panic();

            self.dispatch_ieee802154(addr, tx_token, meta, packet, frag);
            return Ok(());
        }

        // Dispatch IP/Ethernet:

        let caps = self.caps.clone();

        #[cfg(feature = "proto-ipv4-fragmentation")]
        let ipv4_id = self.next_ipv4_frag_ident();

        // First we calculate the total length that we will have to emit.
        let mut total_len = ip_repr.buffer_len();

        // Add the size of the Ethernet header if the medium is Ethernet.
        #[cfg(feature = "medium-ethernet")]
        if matches!(self.caps.medium, Medium::Ethernet) {
            total_len = EthernetFrame::<&[u8]>::buffer_len(total_len);
        }

        // If the medium is Ethernet, then we need to retrieve the destination hardware address.
        #[cfg(feature = "medium-ethernet")]
        let (dst_hardware_addr, mut tx_token) = match self.caps.medium {
            Medium::Ethernet => {
                match self.lookup_hardware_addr(tx_token, &ip_repr.dst_addr(), frag)? {
                    (HardwareAddress::Ethernet(addr), tx_token) => (addr, tx_token),
                    (_, _) => unreachable!(),
                }
            }
            _ => (EthernetAddress([0; 6]), tx_token),
        };

        // Emit function for the Ethernet header.
        #[cfg(feature = "medium-ethernet")]
        let emit_ethernet = |repr: &IpRepr, tx_buffer: &mut [u8]| {
            let mut frame = EthernetFrame::new_unchecked(tx_buffer);

            let src_addr = self.hardware_addr.ethernet_or_panic();
            frame.set_src_addr(src_addr);
            frame.set_dst_addr(dst_hardware_addr);

            match repr.version() {
                #[cfg(feature = "proto-ipv4")]
                IpVersion::Ipv4 => frame.set_ethertype(EthernetProtocol::Ipv4),
                #[cfg(feature = "proto-ipv6")]
                IpVersion::Ipv6 => frame.set_ethertype(EthernetProtocol::Ipv6),
            }
        };

        // Emit function for the IP header and payload.
        let emit_ip = |repr: &IpRepr, tx_buffer: &mut [u8]| {
            if let Some(bytes) = preserved {
                debug_assert_eq!(bytes.len(), repr.buffer_len());
                tx_buffer[..bytes.len()].copy_from_slice(bytes);
                return;
            }
            repr.emit(&mut *tx_buffer, &self.caps.checksum);

            let payload = &mut tx_buffer[repr.header_len()..];
            packet.emit_payload(repr, payload, &caps)
        };

        let total_ip_len = ip_repr.buffer_len();

        match &mut ip_repr {
            #[cfg(feature = "proto-ipv4")]
            IpRepr::Ipv4(_repr) => {
                // If we have an IPv4 packet, then we need to check if we need to fragment it.
                if total_ip_len > self.caps.ip_mtu() {
                    #[cfg(feature = "proto-ipv4-fragmentation")]
                    {
                        net_debug!("start fragmentation");

                        // Calculate how much we will send now (including the Ethernet header).

                        let ip_header_len = _repr.buffer_len();
                        let first_frag_data_len =
                            self.caps.max_ipv4_fragment_size(_repr.buffer_len());
                        let first_frag_ip_len = first_frag_data_len + ip_header_len;
                        let mut tx_len = first_frag_ip_len;
                        #[cfg(feature = "medium-ethernet")]
                        if matches!(caps.medium, Medium::Ethernet) {
                            tx_len += EthernetFrame::<&[u8]>::header_len();
                        }

                        if frag.buffer.len() < total_ip_len {
                            net_debug!(
                                "Fragmentation buffer is too small, at least {} needed. Dropping",
                                total_ip_len
                            );
                            return Ok(());
                        }

                        #[cfg(feature = "medium-ethernet")]
                        {
                            frag.ipv4.dst_hardware_addr = dst_hardware_addr;
                        }

                        // Save the total packet len (without the Ethernet header, but with the first
                        // IP header).
                        frag.packet_len = total_ip_len;

                        // Save the IP header for other fragments.
                        frag.ipv4.repr = *_repr;

                        // Modify the IP header
                        _repr.payload_len = first_frag_data_len;

                        // Save the number of bytes we will send now.
                        frag.sent_bytes = first_frag_ip_len;

                        // Emit the IP header to the buffer.
                        emit_ip(&ip_repr, &mut frag.buffer);

                        let mut ipv4_packet = Ipv4Packet::new_unchecked(&mut frag.buffer[..]);
                        frag.ipv4.ident = ipv4_id;
                        ipv4_packet.set_ident(ipv4_id);
                        ipv4_packet.set_more_frags(true);
                        ipv4_packet.set_dont_frag(false);
                        ipv4_packet.set_frag_offset(0);

                        if caps.checksum.ipv4.tx() {
                            ipv4_packet.fill_checksum();
                        }

                        // Transmit the first packet.
                        tx_token.consume(tx_len, |mut tx_buffer| {
                            #[cfg(feature = "medium-ethernet")]
                            if matches!(self.caps.medium, Medium::Ethernet) {
                                emit_ethernet(&ip_repr, tx_buffer);
                                tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                            }

                            // Change the offset for the next packet.
                            frag.ipv4.frag_offset = (first_frag_ip_len - ip_header_len) as u16;

                            // Copy the IP header and the payload.
                            tx_buffer[..first_frag_ip_len]
                                .copy_from_slice(&frag.buffer[..first_frag_ip_len]);
                        });

                        Ok(())
                    }

                    #[cfg(not(feature = "proto-ipv4-fragmentation"))]
                    {
                        net_debug!(
                            "Enable the `proto-ipv4-fragmentation` feature for fragmentation support."
                        );
                        Ok(())
                    }
                } else {
                    tx_token.set_meta(meta);

                    // No fragmentation is required.
                    tx_token.consume(total_len, |mut tx_buffer| {
                        #[cfg(feature = "medium-ethernet")]
                        if matches!(self.caps.medium, Medium::Ethernet) {
                            emit_ethernet(&ip_repr, tx_buffer);
                            tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                        }

                        emit_ip(&ip_repr, tx_buffer);
                    });

                    Ok(())
                }
            }
            // We don't support IPv6 fragmentation yet.
            #[cfg(feature = "proto-ipv6")]
            IpRepr::Ipv6(_) => {
                // Check if we need to fragment it.
                if total_ip_len > self.caps.ip_mtu() {
                    net_debug!("IPv6 fragmentation support is unimplemented. Dropping.");
                    Ok(())
                } else {
                    tx_token.consume(total_len, |mut tx_buffer| {
                        #[cfg(feature = "medium-ethernet")]
                        if matches!(self.caps.medium, Medium::Ethernet) {
                            emit_ethernet(&ip_repr, tx_buffer);
                            tx_buffer = &mut tx_buffer[EthernetFrame::<&[u8]>::header_len()..];
                        }

                        emit_ip(&ip_repr, tx_buffer);
                    });
                    Ok(())
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum DispatchError {
    /// No route to dispatch this packet. Retrying won't help unless
    /// configuration is changed.
    NoRoute,
    /// We do have a route to dispatch this packet, but we haven't discovered
    /// the neighbor for it yet. Discovery has been initiated, dispatch
    /// should be retried later.
    NeighborPending,
}
