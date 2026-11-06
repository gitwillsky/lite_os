#![no_std]
#![no_main]

//! netcfg：网络配置（Fuchsia netcfg）。
//!
//! 1. 从根服务路由来的网卡目录打开每块网卡，经 netstack 的安装协议装进默认网络；
//! 2. 按“用户设置 → bootfs 默认策略 → DHCP”决定每个接口的配置：静态地址直接配置，DHCP 经 dhcp-client
//!    取得租约后配置；用户设置持久化在 fshost 派生的私有数据目录；
//! 3. 一个事件循环线程以一个 Port 等待：netstack 断开（重连、重新安装网卡、重新配置）、设备目录变化
//!    （热插拔网卡）与各接口的租约事件；
//! 4. 经 outgoing 提供 `liteos.network.Settings`：快照、修改与等待变化（linuxd 适配给控制面板，并据此生成
//!    Linux 的 `/etc/resolv.conf`）。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod policy;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_protocol::directory::{Class, DirectoryClient};
use filesystem_protocol::Storage;
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status, packet, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Port, Startup, Vmo};
use netcfg_protocol::{
    Decoder, DhcpAction, Encoder, InterfaceConfig, InterfaceState, LeaseEvent, LeaseStatus,
    LeaseTiming, Method, Snapshot,
};
use network_protocol::{
    Ipv4RouteError, Ipv4RouteKind, Ipv4RouteMetrics, Ipv4RouteRequest, NetworkClient,
    NetworkInstaller, RemoteNamespace, RouteChange,
};
use service_directory::ServiceDirectory;

native_runtime::entry!(main);

/// Port key：netstack 安装连接断开。
const KEY_NETSTACK: u64 = 1;
/// Port key：设备目录变化。
const KEY_DIRECTORY: u64 = 2;
/// Port key 的起点：各接口 DHCP 会话的租约事件 channel 与接口 lifetime channel（每个一个新 key）。
const KEY_DHCP_FIRST: u64 = 1 << 32;
/// dhcp-client 的 DHCP 请求序号（与 dhcp-client 一致）。
const DHCP_START: u32 = 1;
/// Bounds the settings RPC even when no DHCP server answers discovery.
const DHCP_COMMAND_TIMEOUT: i64 = 15_000_000_000;
/// 默认路由的路由协议标识（`RTPROT_DHCP` / `RTPROT_BOOT`）。
const RTPROT_DHCP: u8 = 16;
const RTPROT_BOOT: u8 = 3;
/// 主路由表（`RT_TABLE_MAIN`）。
const RT_TABLE_MAIN: u32 = 254;

/// 到 netstack 当前实例的连接。
struct Link {
    installer: NetworkInstaller,
    network: RemoteNamespace,
}

/// 一个已安装的接口。
struct Interface {
    name: Vec<u8>,
    mac: [u8; 6],
    index: u32,
    config: InterfaceConfig,
    address: Ipv4Addr,
    prefix_length: u8,
    gateway: Ipv4Addr,
    dns: Vec<Ipv4Addr>,
    // 当前地址配置生效的单调时刻（纳秒）；没有地址时为 None。
    connected: Option<i64>,
    // 只有成功配置的租约提供 DHCP 后缀；手动后缀由 config 拥有。
    lease: Option<(LeaseTiming, alloc::string::String)>,
    // OWNER: 本接口的 DHCP 会话（Port key 与租约事件 channel）；丢弃即关闭 channel，dhcp-client 随之停止。
    dhcp: Option<(u64, Channel)>,
    // netstack 交出的接口 lifetime（Port key 与 channel）：网卡会话关闭、接口被移除时 `PEER_CLOSED`。
    // 缺失时网卡热拔后本记录与 DHCP 会话残留，重新出现的网卡被当作第二个接口。
    lifetime: (u64, Channel),
}

struct Inner {
    generation: u64,
    // OWNER: 用户设置的唯一副本；修改后先持久化再应用。
    saved: policy::Settings,
    link: Option<Link>,
    interfaces: Vec<Interface>,
    // 已处理的最后一个设备目录 ID；netstack 重启后清零，重新打开全部网卡。
    last_device: u64,
    next_key: u64,
    // OWNER: 当前代际的变化通知；每次变化置位旧 Event 并换成新的，WAIT 的等待者互不吞掉唤醒。
    changed: Arc<Event>,
}

struct Netcfg {
    services: ServiceDirectory,
    directory: DirectoryClient,
    storage: Storage,
    defaults: Vec<policy::Entry>,
    port: Port,
    watch: Event,
    inner: Mutex<Inner>,
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::netcfg;
    let outgoing = Channel::from_handle(startup.take(netcfg::OUTGOING));
    let services = ServiceDirectory::new(Channel::from_handle(startup.take(netcfg::SERVICES)));
    let directory =
        DirectoryClient::new(Channel::from_handle(startup.take(netcfg::DEVICE_DIRECTORY)));
    let storage = Storage::new(Channel::from_handle(startup.take(netcfg::STORAGE)));
    let defaults = policy::parse_defaults(&read_config(&Vmo::from_handle(
        startup.take(netcfg::CONFIG),
    )));
    let saved = match storage.read(policy::SETTINGS_FILE) {
        Ok(Some(bytes)) => policy::decode_settings(&bytes).unwrap_or_else(|_| {
            warn!("netcfg: unreadable settings file ignored");
            policy::Settings::default()
        }),
        Ok(None) => policy::Settings::default(),
        Err(status) => {
            warn!("netcfg: settings unavailable: {status}");
            policy::Settings::default()
        }
    };
    let netcfg = Arc::new(Netcfg {
        services,
        watch: directory.watch().expect("netcfg device watch"),
        directory,
        storage,
        defaults,
        port: Port::create().expect("netcfg port"),
        inner: Mutex::new(Inner {
            generation: 1,
            saved,
            link: None,
            interfaces: Vec::new(),
            last_device: 0,
            next_key: KEY_DHCP_FIRST,
            changed: Arc::new(Event::create().expect("netcfg change event")),
        }),
    });
    let settings = netcfg.clone();
    service_directory::serve_directory(
        outgoing,
        b"netcfg-outgoing",
        Arc::new(move |protocol: &[u8], channel: Channel| {
            if protocol != service_directory::NETWORK_SETTINGS {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            serve_settings(settings.clone(), channel)
        }),
    )
    .expect("netcfg outgoing service");
    info!("netcfg ready");
    netcfg.run()
}

/// bootfs 默认策略的文本（VMO 按页取整，去掉末尾的零填充）。
fn read_config(vmo: &Vmo) -> Vec<u8> {
    let size = vmo.size().unwrap_or(0) as usize;
    let mut bytes = alloc::vec![0; size];
    if vmo.read(&mut bytes, 0).is_err() {
        return Vec::new();
    }
    let end = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |end| end + 1);
    bytes.truncate(end);
    bytes
}

fn prefix_mask(prefix_length: u8) -> Ipv4Addr {
    Ipv4Addr::from(
        u32::MAX
            .checked_shl(32 - u32::from(prefix_length))
            .unwrap_or(0),
    )
}

fn default_route(index: u32, gateway: Ipv4Addr, protocol: u8) -> Ipv4RouteRequest {
    Ipv4RouteRequest {
        destination: Ipv4Addr::UNSPECIFIED,
        prefix_length: 0,
        table: RT_TABLE_MAIN,
        metric: index,
        scope: 0,
        protocol,
        kind: Ipv4RouteKind::Unicast,
        interface_index: Some(index),
        gateway: Some(gateway),
        preferred_source: None,
        on_link: false,
        metrics: Ipv4RouteMetrics::default(),
    }
}

impl Netcfg {
    /// 事件循环；只在根服务判定崩溃循环后整体关机时随进程结束。
    fn run(&self) -> i64 {
        loop {
            self.connect();
            loop {
                let packet = match self.port.wait(DEADLINE_INFINITE) {
                    Ok(packet) => packet,
                    Err(status) => {
                        error!("netcfg: port wait failed: {status}");
                        return -1;
                    }
                };
                if packet.kind != packet::SIGNAL_ONE || packet.status != native_abi::OK {
                    continue;
                }
                match packet.key {
                    KEY_NETSTACK => break,
                    KEY_DIRECTORY => self.devices_changed(),
                    key if self.interface_removed(key) => {}
                    key => self.lease_ready(key),
                }
            }
            warn!("netcfg: netstack connection closed; reinstalling interfaces");
            let mut inner = self.inner.lock();
            inner.link = None;
            inner.interfaces.clear();
            inner.last_device = 0;
            Self::changed(&mut inner);
        }
    }

    /// 连接 netstack 当前实例（重启期间根服务让连接等待），安装全部网卡并应用配置。
    fn connect(&self) {
        let link = loop {
            match self.open_link() {
                Ok(link) => break link,
                Err(status) => {
                    error!("netcfg: netstack connection failed: {status}");
                    let _ = native_runtime::system::sleep_until(
                        native_runtime::system::monotonic() + 1_000_000_000,
                    );
                }
            }
        };
        let _ = link
            .installer
            .channel()
            .wait_async(&self.port, KEY_NETSTACK, signals::PEER_CLOSED);
        self.inner.lock().link = Some(link);
        self.devices_changed();
    }

    fn open_link(&self) -> Result<Link, Status> {
        let installer = NetworkInstaller::new(
            self.services
                .connect(service_directory::NETWORK_INSTALLER)?,
        );
        let control = NetworkClient::new(self.services.connect(service_directory::NETWORK)?);
        let network = control.create_namespace(true)?;
        Ok(Link { installer, network })
    }

    /// 先清除再登记目录变化等待，然后接入所有新网卡；读取之后到达的变化不会丢失。
    fn devices_changed(&self) {
        let _ = self.watch.signal(signals::USER_0, 0);
        let _ = self
            .watch
            .wait_async(&self.port, KEY_DIRECTORY, signals::USER_0);
        loop {
            let after = self.inner.lock().last_device;
            let entries = match self.directory.list(after) {
                Ok((_, entries)) if !entries.is_empty() => entries,
                Ok(_) => return,
                Err(status) => {
                    error!("netcfg: device directory failed: {status}");
                    return;
                }
            };
            for entry in entries {
                self.inner.lock().last_device = entry.id;
                if entry.class != Class::Network {
                    continue;
                }
                if let Err(status) = self.install(&entry) {
                    error!(
                        "netcfg: network device {} install failed: {status}",
                        entry.id
                    );
                }
            }
        }
    }

    fn install(&self, entry: &device_protocol::directory::Entry) -> Result<(), Status> {
        let device = self.directory.open(entry)?;
        let mut inner = self.inner.lock();
        let Some(link) = inner.link.as_ref() else {
            return Err(native_abi::ERR_PEER_CLOSED);
        };
        let (index, lifetime) = link
            .installer
            .install_device(&device.description, device.handles)?;
        let key = inner.next_key;
        inner.next_key += 1;
        lifetime.wait_async(&self.port, key, signals::PEER_CLOSED)?;
        let Some(link) = inner.link.as_ref() else {
            return Err(native_abi::ERR_PEER_CLOSED);
        };
        let snapshot = link
            .network
            .interface_snapshot(index)
            .map_err(|_| native_abi::ERR_IO)?;
        let length = snapshot
            .name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(16);
        let name = snapshot.name[..length].to_vec();
        let config = policy::resolve(&inner.saved.interfaces, &self.defaults, &name);
        info!(
            "netcfg: installed {} as interface {index}",
            core::str::from_utf8(&name).unwrap_or("<non-utf8>")
        );
        inner.interfaces.push(Interface {
            name,
            mac: snapshot.mac,
            index,
            config,
            address: Ipv4Addr::UNSPECIFIED,
            prefix_length: 0,
            gateway: Ipv4Addr::UNSPECIFIED,
            dns: Vec::new(),
            connected: None,
            lease: None,
            dhcp: None,
            lifetime: (key, lifetime),
        });
        let position = inner.interfaces.len() - 1;
        self.apply(&mut inner, position);
        Ok(())
    }

    /// 按接口当前设置重新配置它：先撤销旧地址、默认路由与 DHCP 会话，再按方法配置。
    fn apply(&self, inner: &mut Inner, position: usize) {
        let key = inner.next_key;
        let Inner {
            link, interfaces, ..
        } = &mut *inner;
        let Some(link) = link.as_ref() else {
            return;
        };
        let interface = &mut interfaces[position];
        interface.dhcp = None;
        let _ = Self::clear_address(&link.network, interface);
        let network = &link.network;
        let index = interface.index;
        let result = match interface.config.method {
            Method::Disabled => network
                .configure_up(index, false)
                .map_err(|_| native_abi::ERR_IO),
            Method::Static => {
                let config = interface.config.clone();
                network
                    .configure_up(index, true)
                    .map_err(|_| native_abi::ERR_IO)
                    .and_then(|()| {
                        Self::configure_address(
                            network,
                            interface,
                            config.address,
                            config.prefix_length,
                            config.gateway,
                            RTPROT_BOOT,
                        )
                    })
                    .map(|()| interface.dns = config.dns)
            }
            Method::Dhcp => network
                .configure_up(index, true)
                .map_err(|_| native_abi::ERR_IO)
                .and_then(|()| self.start_dhcp(interface, key)),
        };
        if interface.config.method == Method::Dhcp {
            inner.next_key += 1;
        }
        if let Err(status) = result {
            error!("netcfg: interface {index} configuration failed: {status}");
        }
        Self::changed(inner);
    }

    fn start_dhcp(&self, interface: &mut Interface, key: u64) -> Result<(), Status> {
        let dhcp = self.services.connect(service_directory::DHCP)?;
        let mut request = service_rpc::Writer::request(DHCP_START);
        request.u32(interface.index).bytes(&interface.mac);
        let mut reply = service_rpc::call(&dhcp, &request, Vec::new())?;
        let events = Channel::from_handle(service_rpc::take_handle(&mut reply.handles, 0)?);
        events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED)?;
        interface.dhcp = Some((key, events));
        Ok(())
    }

    fn clear_address(network: &RemoteNamespace, interface: &mut Interface) -> Result<(), Status> {
        let route = if !interface.gateway.is_unspecified() {
            let protocol = if interface.lease.is_some() {
                RTPROT_DHCP
            } else {
                RTPROT_BOOT
            };
            network.configure_route(
                default_route(interface.index, interface.gateway, protocol),
                RouteChange {
                    delete: true,
                    ..RouteChange::default()
                },
            )
        } else {
            Ok(())
        };
        let address = if interface.address.is_unspecified() {
            Ok(())
        } else {
            network.configure_address(interface.index, Ipv4Addr::UNSPECIFIED)
        };
        interface.address = Ipv4Addr::UNSPECIFIED;
        interface.prefix_length = 0;
        interface.gateway = Ipv4Addr::UNSPECIFIED;
        interface.dns.clear();
        interface.connected = None;
        interface.lease = None;
        route
            .or_else(|error| {
                if error == Ipv4RouteError::NotFound {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|_| native_abi::ERR_IO)
            .and_then(|()| address.map_err(|_| native_abi::ERR_IO))
    }

    fn configure_address(
        network: &RemoteNamespace,
        interface: &mut Interface,
        address: Ipv4Addr,
        prefix_length: u8,
        gateway: Ipv4Addr,
        protocol: u8,
    ) -> Result<(), Status> {
        let index = interface.index;
        network
            .configure_address(index, address)
            .and_then(|()| network.configure_netmask(index, prefix_mask(prefix_length)))
            .map_err(|_| native_abi::ERR_IO)?;
        interface.address = address;
        interface.prefix_length = prefix_length;
        interface.connected = Some(native_runtime::system::monotonic());
        if !gateway.is_unspecified() {
            network
                .configure_route(
                    default_route(index, gateway, protocol),
                    RouteChange {
                        create: true,
                        ..RouteChange::default()
                    },
                )
                .map_err(|_| native_abi::ERR_IO)?;
            interface.gateway = gateway;
        }
        Ok(())
    }

    /// 处理一个接口的租约事件；会话已被替换（设置变化）时忽略过时的 key。
    /// 接口被 netstack 移除（网卡会话关闭）：丢弃记录与 DHCP 会话；网卡重新出现时由目录变化重新安装。
    ///
    /// # Returns
    ///
    /// `key` 是某个接口的 lifetime 时为 true。
    fn interface_removed(&self, key: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(position) = inner
            .interfaces
            .iter()
            .position(|interface| interface.lifetime.0 == key)
        else {
            return false;
        };
        let interface = inner.interfaces.remove(position);
        info!(
            "netcfg: interface {} removed",
            core::str::from_utf8(&interface.name).unwrap_or("<non-utf8>")
        );
        Self::changed(&mut inner);
        true
    }

    fn lease_ready(&self, key: u64) {
        let mut inner = self.inner.lock();
        let Some(position) = inner.interfaces.iter().position(|interface| {
            interface
                .dhcp
                .as_ref()
                .is_some_and(|(held, _)| *held == key)
        }) else {
            return;
        };
        let message = inner.interfaces[position]
            .dhcp
            .as_ref()
            .map(|(_, events)| events.read());
        let event = match message {
            Some(Ok(message)) if message.handles.len() <= 1 => {
                let mut decoder = Decoder::new(&message.bytes);
                decoder
                    .lease()
                    .ok()
                    .filter(|_| decoder.finish().is_ok())
                    .map(|event| {
                        let reply = message.handles.into_iter().next().map(Channel::from_handle);
                        (event, reply)
                    })
            }
            // dhcp-client 终止：重新请求（根服务重启它期间连接会等待）。
            Some(Err(native_abi::ERR_PEER_CLOSED)) => {
                warn!("netcfg: dhcp-client session closed; restarting DHCP");
                self.apply(&mut inner, position);
                return;
            }
            _ => None,
        };
        let Inner {
            link, interfaces, ..
        } = &mut *inner;
        let interface = &mut interfaces[position];
        if let Some((_, events)) = interface.dhcp.as_ref() {
            let _ = events.wait_async(&self.port, key, signals::READABLE | signals::PEER_CLOSED);
        }
        let (Some((event, reply)), Some(link)) = (event, link.as_ref()) else {
            return;
        };
        let network = &link.network;
        let result = match event {
            LeaseEvent::Lost => Self::clear_address(network, interface),
            LeaseEvent::Bound {
                address,
                prefix_length,
                gateway,
                dns,
                dns_suffix,
                timing,
            } => {
                let same_address = (
                    interface.address,
                    interface.prefix_length,
                    interface.gateway,
                ) == (address, prefix_length, gateway);
                // 续租未改变地址或路由时保留现有配置及连接时长。
                let result = if same_address {
                    Ok(())
                } else {
                    let _ = Self::clear_address(network, interface);
                    Self::configure_address(
                        network,
                        interface,
                        address,
                        prefix_length,
                        gateway,
                        RTPROT_DHCP,
                    )
                };
                result.map(|()| {
                    // 静态 DNS 覆盖租约中的 DNS（Windows“使用下面的 DNS 服务器地址”）。
                    interface.dns = if interface.config.dns.is_empty() {
                        dns
                    } else {
                        interface.config.dns.clone()
                    };
                    interface.lease = Some((timing, dns_suffix));
                })
            }
        };
        Self::changed(&mut inner);
        let status = result.err().unwrap_or(native_abi::OK);
        if status != native_abi::OK {
            error!("netcfg: lease configuration failed: {status}");
        }
        if let Some(reply) = reply {
            let _ = reply.write(&status.to_le_bytes(), Vec::new());
        }
    }

    fn changed(inner: &mut Inner) {
        inner.generation += 1;
        let next = Arc::new(Event::create().expect("netcfg change event"));
        let previous = core::mem::replace(&mut inner.changed, next);
        let _ = previous.signal(0, signals::USER_0);
    }

    fn snapshot(inner: &Inner) -> Snapshot {
        let now = native_runtime::system::monotonic();
        let utc_offset = native_runtime::system::utc().saturating_sub(now);
        Snapshot {
            generation: inner.generation,
            search_list: inner.saved.search_list.clone(),
            primary_dns_suffix: inner.saved.primary_dns_suffix.clone(),
            append_parent_suffixes: inner.saved.append_parent_suffixes,
            interfaces: inner
                .interfaces
                .iter()
                .map(|interface| InterfaceState {
                    name: interface.name.clone(),
                    connection_name: inner
                        .saved
                        .names
                        .iter()
                        .find(|(name, _)| *name == interface.name)
                        .map_or_else(Vec::new, |(_, name)| name.as_bytes().to_vec()),
                    mac: interface.mac,
                    config: interface.config.clone(),
                    address: interface.address,
                    prefix_length: interface.prefix_length,
                    gateway: interface.gateway,
                    dns: interface.dns.clone(),
                    dns_suffix: if interface.address.is_unspecified() {
                        Default::default()
                    } else if interface.config.dns_suffix.is_empty() {
                        interface
                            .lease
                            .as_ref()
                            .map_or_else(Default::default, |(_, suffix)| suffix.clone())
                    } else {
                        interface.config.dns_suffix.clone()
                    },
                    connected_seconds: interface.connected.map_or(0, |since| {
                        u32::try_from(now.saturating_sub(since) / 1_000_000_000).unwrap_or(u32::MAX)
                    }),
                    lease: interface.lease.as_ref().map(|(lease, _)| {
                        let obtained_at = lease
                            .requested_at_ns
                            .saturating_add(utc_offset)
                            .div_euclid(1_000_000_000);
                        LeaseStatus {
                            server: lease.server,
                            obtained_at,
                            expires_at: (lease.duration_seconds != u32::MAX).then(|| {
                                obtained_at.saturating_add(i64::from(lease.duration_seconds))
                            }),
                        }
                    }),
                })
                .collect(),
        }
    }

    /// 先持久化本次设置，再发布新的配置代际。
    fn set(&self, update: netcfg_protocol::Update<'_>) -> Result<(), Status> {
        let netcfg_protocol::Update {
            interface: name,
            config,
            search_list,
            primary_dns_suffix,
            append_parent_suffixes,
        } = update;
        if config.as_ref().is_some_and(|config| {
            name.is_empty() || name.len() >= netcfg_protocol::NAME_MAX || !policy::valid(config)
        }) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let mut inner = self.inner.lock();
        let mut saved = inner.saved.clone();
        if let Some(config) = &config {
            match saved
                .interfaces
                .iter_mut()
                .find(|(entry, _)| entry.as_slice() == name)
            {
                Some((_, entry)) => *entry = config.clone(),
                None => saved.interfaces.push((name.to_vec(), config.clone())),
            }
        }
        if let Some(search_list) = search_list {
            saved.search_list = search_list;
        }
        if let Some(suffix) = primary_dns_suffix {
            saved.primary_dns_suffix = suffix;
        }
        if let Some(append) = append_parent_suffixes {
            saved.append_parent_suffixes = append;
        }
        self.storage
            .write(policy::SETTINGS_FILE, &policy::encode_settings(&saved))?;
        inner.saved = saved;
        if let Some(config) = config
            && let Some(position) = inner
                .interfaces
                .iter()
                .position(|interface| interface.name.as_slice() == name)
        {
            let current = &mut inner.interfaces[position].config;
            let suffix_only = current.dns_suffix != config.dns_suffix
                && current.method == config.method
                && current.address == config.address
                && current.prefix_length == config.prefix_length
                && current.gateway == config.gateway
                && current.dns == config.dns;
            *current = config;
            if suffix_only {
                // 只改搜索后缀时，现有地址、路由和 DHCP 会话继续有效。
                Self::changed(&mut inner);
            } else {
                self.apply(&mut inner, position);
            }
        } else {
            Self::changed(&mut inner);
        }
        Ok(())
    }

    /// 持久化连接名后发布新代际。IP 配置与 DHCP 会话继续使用原接口。
    fn rename(&self, interface: &[u8], name: &[u8]) -> Result<(), Status> {
        let name =
            netcfg_protocol::connection_name(name).map_err(|_| native_abi::ERR_INVALID_ARGS)?;
        let mut inner = self.inner.lock();
        if !inner.interfaces.iter().any(|entry| entry.name == interface) {
            return Err(native_abi::ERR_NOT_FOUND);
        }
        let folded = name.to_lowercase();
        if inner
            .saved
            .names
            .iter()
            .any(|(entry, label)| entry != interface && label.to_lowercase() == folded)
        {
            return Err(native_abi::ERR_ALREADY_BOUND);
        }
        let mut saved = inner.saved.clone();
        match saved.names.iter_mut().find(|(entry, _)| entry == interface) {
            Some((_, label)) if label == name => return Ok(()),
            Some((_, label)) => *label = name.into(),
            None => saved.names.push((interface.to_vec(), name.into())),
        }
        self.storage
            .write(policy::SETTINGS_FILE, &policy::encode_settings(&saved))?;
        inner.saved = saved;
        Self::changed(&mut inner);
        Ok(())
    }

    /// 等待代际超过 `generation`，返回新的代际。
    fn wait(&self, generation: u64) -> Result<u64, Status> {
        loop {
            let event = {
                let inner = self.inner.lock();
                if inner.generation > generation {
                    return Ok(inner.generation);
                }
                inner.changed.clone()
            };
            event.wait_one(signals::USER_0, DEADLINE_INFINITE)?;
        }
    }

    /// Send an operation to the installed session. Its reply is completed only
    /// after the resulting lease event has been applied by the configuration owner.
    fn dhcp_command(&self, name: &[u8], action: DhcpAction) -> Result<(), Status> {
        let (reply, completion) = Channel::create()?;
        {
            let inner = self.inner.lock();
            let interface = inner
                .interfaces
                .iter()
                .find(|interface| interface.name == name)
                .ok_or(native_abi::ERR_NOT_FOUND)?;
            let (_, session) = interface.dhcp.as_ref().ok_or(native_abi::ERR_BAD_STATE)?;
            session.write(&[action as u8], alloc::vec![completion.into_handle()])?;
        }
        let deadline = native_runtime::system::monotonic().saturating_add(DHCP_COMMAND_TIMEOUT);
        let response = reply.read_wait(deadline)?;
        let status = Status::from_le_bytes(
            response
                .bytes
                .as_slice()
                .try_into()
                .map_err(|_| native_abi::ERR_INVALID_ARGS)?,
        );
        if status == native_abi::OK {
            Ok(())
        } else {
            Err(status)
        }
    }
}

fn serve_settings(netcfg: Arc<Netcfg>, channel: Channel) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        2,
        b"netcfg-settings",
        Arc::new(
            move |ordinal: u32,
                  request: &mut service_rpc::Reader<'_>,
                  _: Vec<native_runtime::Handle>,
                  reply: &mut service_rpc::Writer| {
                match ordinal {
                    netcfg_protocol::ordinal::GET => {
                        let mut encoder = Encoder::default();
                        encoder.snapshot(&Netcfg::snapshot(&netcfg.inner.lock()));
                        reply.bytes(&encoder.0);
                    }
                    netcfg_protocol::ordinal::SET => {
                        let mut decoder = Decoder::new(request.bytes()?);
                        let update = decoder.update().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        decoder.finish().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        netcfg.set(update)?;
                    }
                    netcfg_protocol::ordinal::WAIT => {
                        reply.u64(netcfg.wait(request.u64()?)?);
                    }
                    netcfg_protocol::ordinal::DHCP => {
                        let name = request.bytes()?;
                        let action = DhcpAction::decode(request.u8()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        netcfg.dhcp_command(name, action)?;
                    }
                    netcfg_protocol::ordinal::RENAME => {
                        netcfg.rename(request.bytes()?, request.bytes()?)?;
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        ),
    )
}
