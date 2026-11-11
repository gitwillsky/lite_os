#![no_std]
#![no_main]
#![feature(allocator_ext)]

//! router-config：接口配置的唯一 owner，持有设备授权、租约与持久数据能力。
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

mod administration;
mod backup;
mod bgp;
mod delegation;
mod dhcp_server;
mod dns_cache;
mod leases;
mod policy;
mod routing;
mod transaction;
use transaction::{StageError, StorageRollback};

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::net::Ipv4Addr;

use device_protocol::directory::{Class, DirectoryClient};
use filesystem_protocol::{Storage, StorageService};
use native_runtime::native_abi::{self, DEADLINE_INFINITE, Status, packet, signals};
use native_runtime::sync::Mutex;
use native_runtime::{Channel, Event, Port, Startup, Vmo};
use network_protocol::routing::RoutingConfigurator;
use network_protocol::{NetworkClient, PreparedConfiguration, RemoteNamespace, SocketError};
use router_protocol::configuration::Configuration;
use router_protocol::interface::InterfaceConfiguration;
use router_protocol::{
    Decoder, DhcpAction, Encoder, InterfaceConfig, InterfaceState, LeaseEvent, LeaseStatus,
    LeaseTiming, Method, Snapshot,
};
use service_directory::ServiceDirectory;

native_runtime::entry!(main);

/// Port key：netstack 安装连接断开。
const KEY_NETSTACK: u64 = 1;
/// Port key：设备目录变化。
const KEY_DIRECTORY: u64 = 2;
/// 配置失败后有实际重试工作，唤醒事件循环重新计算唯一截止时间。
const KEY_CONFIGURATION: u64 = 3;
/// 私有存储启动能力关闭；只能由组件监督器重新派生，不周期重试失效句柄。
const KEY_STORAGE: u64 = 4;
/// 原生 RIB 终止后重新连接并重放持久决定；NET 仍活着时不重复安装设备。
const KEY_ROUTING: u64 = 5;
/// BGP 域能力终止；仅重放已决定的 BGP 配置，不重建网卡或 DHCP。
const KEY_BGP: u64 = 6;
/// 通告 DNS 集合变更，唤醒设置快照观察者。
const KEY_DNS: u64 = 7;
/// Port key 的起点：各接口 DHCP 会话的租约事件 channel 与接口 lifetime channel（每个一个新 key）。
const KEY_DHCP_FIRST: u64 = 1 << 32;
/// dhcp-client 的 DHCP 请求序号（与 dhcp-client 一致）。
const DHCP_START: u32 = 1;
/// Bounds the settings RPC even when no DHCP server answers discovery.
const DHCP_COMMAND_TIMEOUT: i64 = 15_000_000_000;
/// 默认路由的路由协议标识（`RTPROT_DHCP` / `RTPROT_BOOT`）。
const RTPROT_DHCP: u8 = 16;
const RTPROT_BOOT: u8 = 3;

/// 到 netstack 当前实例的连接。
struct Link {
    installer: RoutingConfigurator,
    network: RemoteNamespace,
    dns_changed: Event,
}

/// 已成功应用的 DHCP 租约；保存服务器提供的 DNS，手动覆盖不改写租约事实。
struct Lease {
    timing: LeaseTiming,
    dns_suffix: alloc::string::String,
    dns: Vec<Ipv4Addr>,
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
    // 当前地址配置生效的单调时刻（纳秒）；没有地址时为 None。
    connected: Option<i64>,
    // 只有成功配置的租约提供 DHCP 后缀；手动后缀由 config 拥有。
    lease: Option<Lease>,
    // OWNER: 尚未确认应用的当前配置；仅有待处理工作时设截止，不是周期轮询。
    retry_at: Option<i64>,
    // OWNER: 本接口的 DHCP 会话（Port key 与租约事件 channel）；丢弃即关闭 channel，dhcp-client 随之停止。
    dhcp: Option<(u64, Channel)>,
    dhcp_server: Option<dhcp_server::PoolSession>,
    // netstack 交出的接口 lifetime（Port key 与 channel）：网卡会话关闭、接口被移除时 `PEER_CLOSED`。
    // 缺失时网卡热拔后本记录与 DHCP 会话残留，重新出现的网卡被当作第二个接口。
    lifetime: (u64, Channel),
    link_changed: Event,
}

impl Interface {
    /// 仅执行服务确认后发布生效地址；租约由随后成功的 Bound 事件单独持有。
    fn publish_address(
        &mut self,
        address: Option<Ipv4Addr>,
        prefix_length: u8,
        route: Option<router_protocol::interface::DefaultRoute>,
    ) {
        self.address = address.unwrap_or(Ipv4Addr::UNSPECIFIED);
        self.prefix_length = prefix_length;
        self.gateway = route.map_or(Ipv4Addr::UNSPECIFIED, |route| route.gateway);
        self.connected = address.map(|_| native_runtime::system::monotonic());
        self.lease = None;
        self.retry_at = None;
    }

    /// 仅解析设置变化可复用已应用的网络；失败后保留的期望配置不能阻止再次提交重试。
    fn preserves_network(&self, config: &InterfaceConfig) -> bool {
        self.retry_at.is_none() && self.network_matches(config)
    }

    /// 当前确认地址/会话能否承载同一策略；重试时仍会重放完整接口计划，不把旧 ACK 当成执行结果。
    fn network_matches(&self, config: &InterfaceConfig) -> bool {
        self.config.method == config.method
            && match config.method {
                Method::Disabled => {
                    self.address.is_unspecified()
                        && self.dhcp.is_none()
                        && self.connected.is_none()
                        && self.lease.is_none()
                }
                Method::Dhcp => self.dhcp.is_some(),
                Method::Static => {
                    self.lease.is_none()
                        && self.connected.is_some()
                        && self.address == config.address
                        && self.prefix_length == config.prefix_length
                        && self.gateway == config.gateway
                }
            }
    }

    /// DNS 只在快照时由配置和成功租约投影，不维护需手工同步的生效副本。
    fn nameservers(&self) -> &[Ipv4Addr] {
        if self.address.is_unspecified() {
            &[]
        } else if self.config.method == Method::Dhcp && self.config.dns.is_empty() {
            self.lease
                .as_ref()
                .map_or(&[], |lease| lease.dns.as_slice())
        } else {
            &self.config.dns
        }
    }
}

struct Inner {
    generation: u64,
    // OWNER: 已决定的配置；只有持久化成功后替换。None 表示加载失败：保留管理查询，所有设备禁用，禁止局部覆盖未知配置。
    saved: Option<Arc<Configuration>>,
    // OWNER: 失败写入尚未确认撤销的原始文件；恢复前不允许另一次持久配置提交。
    rollback: Option<StorageRollback>,
    // OWNER: 尚未确认的完整路由意图工作（静态来源/准入策略）；无接口时也必须推进，不是链路/下一跳状态缓存。
    source_retry_at: Option<i64>,
    // OWNER: 当前 BGP 域能力及实际未完成的应用截止；期望配置只从 saved 借用，不保存同步副本。
    bgp: Option<network_protocol::bgp::BgpInstance>,
    bgp_retry_at: Option<i64>,
    delegation: delegation::Delegation,
    dns: dns_cache::DnsExecution,
    // OWNER: 安装失败的目录声明与唯一重试截止；没有此队列，推进发现游标会永久漏掉 OOM/回复丢失的设备。
    pending_devices: Vec<device_protocol::directory::Entry>,
    device_retry_at: Option<i64>,
    // OWNER: 目录等待的独立登记句柄；重装前关闭会取消原 observer，None 代表有实际重登记工作，不能复制 bool 在线状态。
    watch_wait: Option<Event>,
    link: Option<Link>,
    interfaces: Vec<Interface>,
    // 已处理的最后一个设备目录 ID；netstack 重启后清零，重新打开全部网卡。
    last_device: u64,
    next_key: u64,
    // OWNER: 当前代际的变化通知；每次变化置位旧 Event 并换成新的，WAIT 的等待者互不吞掉唤醒。
    changed: Arc<Event>,
}

struct RouterConfig {
    services: ServiceDirectory,
    directory: DirectoryClient,
    storage: Storage,
    defaults: Vec<policy::Entry>,
    // OWNER: 自动备份执行状态；不持此短状态锁执行文件 I/O，也不在持有它时取得配置锁。
    backup: Mutex<backup::Scheduler>,
    port: Port,
    watch: Event,
    // OWNER: 已在启动时武装的控制唤醒；配置请求可唤醒无限等待中的事件循环，不新建线程。
    control: Event,
    inner: Mutex<Inner>,
}

fn main(mut startup: Startup) -> i64 {
    use launch_protocol::router_config;
    let outgoing = Channel::from_handle(startup.take(router_config::OUTGOING));
    let services =
        ServiceDirectory::new(Channel::from_handle(startup.take(router_config::SERVICES)));
    let directory = DirectoryClient::new(Channel::from_handle(
        startup.take(router_config::DEVICE_DIRECTORY),
    ));
    let storage = Storage::new(Channel::from_handle(startup.take(router_config::STORAGE)));
    let defaults = policy::parse_defaults(&read_config(&Vmo::from_handle(
        startup.take(router_config::CONFIG),
    )));
    let saved = match storage.read(policy::SETTINGS_FILE) {
        Ok(Some(bytes)) => match policy::decode_settings(&bytes) {
            Ok(configuration) => Some(configuration),
            Err(error) => {
                // 配置错误不能退回 DHCP：这会在错误网络上启用接口，改变持久配置的安全决定。
                // 保留管理查询，不让关键服务的配置错误触发整机重启；授权整份导入可修复并恢复。
                error!("router-config: persisted configuration rejected: {error}");
                None
            }
        },
        Ok(None) => Some(Configuration::default()),
        Err(status) => {
            error!("router-config: persisted configuration unavailable: {status}");
            None
        }
    };
    let router_config = Arc::new(RouterConfig {
        services,
        watch: directory.watch().expect("router-config device watch"),
        control: Event::create().expect("router-config control event"),
        directory,
        storage,
        defaults,
        backup: Mutex::new(backup::Scheduler::default()),
        port: Port::create().expect("router-config port"),
        inner: Mutex::new(Inner {
            generation: 1,
            saved: saved
                .map(|saved| Arc::try_new(saved).expect("router-config saved configuration")),
            rollback: None,
            pending_devices: Vec::new(),
            device_retry_at: None,
            source_retry_at: None,
            bgp: None,
            bgp_retry_at: None,
            delegation: Default::default(),
            dns: Default::default(),
            watch_wait: None,
            link: None,
            interfaces: Vec::new(),
            last_device: 0,
            next_key: KEY_DHCP_FIRST,
            changed: Arc::new(Event::create().expect("router-config change event")),
        }),
    });
    router_config
        .control
        .wait_async(&router_config.port, KEY_CONFIGURATION, signals::USER_0)
        .expect("router-config control wait");
    router_config
        .storage
        .channel()
        .wait_async(&router_config.port, KEY_STORAGE, signals::PEER_CLOSED)
        .expect("router-config storage lifetime wait");
    let settings = router_config.clone();
    service_directory::serve_directory(
        outgoing,
        b"router-config-outgoing",
        Arc::new(move |protocol: &[u8], channel: Channel| {
            if protocol != service_directory::NETWORK_SETTINGS {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            serve_settings(settings.clone(), channel)
        }),
    )
    .expect("router-config outgoing service");
    let backups = router_config.clone();
    native_runtime::thread::spawn(b"router-backup", move || backups.run_backups())
        .expect("router-config backup worker");
    info!("router-config ready");
    router_config.run()
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

/// 原生配置错误的控制面投影，不把 OOM、冲突或无效参数混同为 I/O 成功。
fn configuration_status(error: SocketError) -> Status {
    match error {
        SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
        SocketError::Again => native_abi::ERR_SHOULD_WAIT,
        SocketError::Invalid => native_abi::ERR_INVALID_ARGS,
        SocketError::NoDevice => native_abi::ERR_NOT_FOUND,
        _ => native_abi::ERR_IO,
    }
}

fn device_installation_retryable(status: Status) -> bool {
    !matches!(
        status,
        native_abi::ERR_NOT_FOUND
            | native_abi::ERR_INVALID_ARGS
            | native_abi::ERR_NOT_SUPPORTED
            | native_abi::ERR_ACCESS_DENIED
    )
}

impl RouterConfig {
    /// 事件循环；只在根服务判定崩溃循环后整体关机时随进程结束。
    fn run(&self) -> i64 {
        loop {
            self.connect();
            loop {
                let packet = match self.port.wait(self.retry_deadline()) {
                    Ok(packet) => packet,
                    Err(native_abi::ERR_TIMED_OUT) => {
                        self.retry_pending();
                        continue;
                    }
                    Err(status) => {
                        error!("router-config: port wait failed: {status}");
                        return -1;
                    }
                };
                if packet.kind != packet::SIGNAL_ONE || packet.status != native_abi::OK {
                    continue;
                }
                match packet.key {
                    KEY_STORAGE => {
                        error!(
                            "router-config: private storage capability closed; restarting component"
                        );
                        return -1;
                    }
                    KEY_NETSTACK => {
                        if self.inner.lock().link.as_ref().is_some_and(|link| {
                            link.network.channel().wait_one(signals::PEER_CLOSED, 0)
                                != Err(native_abi::ERR_TIMED_OUT)
                        }) {
                            break;
                        }
                    }
                    KEY_ROUTING => {
                        let closed = self.inner.lock().link.as_ref().is_some_and(|link| {
                            link.installer.channel().wait_one(signals::PEER_CLOSED, 0)
                                != Err(native_abi::ERR_TIMED_OUT)
                        });
                        if closed && let Err(status) = self.reconnect_routing() {
                            error!("router-config: RIB reconnection failed: {status}");
                            return -1;
                        }
                    }
                    KEY_DIRECTORY => self.devices_changed(),
                    KEY_BGP => self.bgp_closed(),
                    KEY_DNS => {
                        let mut inner = self.inner.lock();
                        if let Some(link) = &inner.link
                            && link.dns_changed.wait_one(signals::USER_0, 0).is_ok()
                        {
                            let result =
                                link.dns_changed.signal(signals::USER_0, 0).and_then(|()| {
                                    link.dns_changed.wait_async(
                                        &self.port,
                                        KEY_DNS,
                                        signals::USER_0,
                                    )
                                });
                            if let Err(status) = result {
                                error!("router-config: DNS observer rearm failed: {status}");
                                return -1;
                            }
                            Self::changed(&mut inner);
                        }
                    }
                    KEY_CONFIGURATION => {
                        let _ = self.control.signal(signals::USER_0, 0);
                        if let Err(status) =
                            self.control
                                .wait_async(&self.port, KEY_CONFIGURATION, signals::USER_0)
                        {
                            error!("router-config: control rearm failed: {status}");
                            return -1;
                        }
                        self.retry_pending();
                    }
                    key if self.interface_removed(key)
                        || self.delegation_ready(key)
                        || self.pool_closed(key)
                        || self.dns_closed(key) => {}
                    key => self.lease_ready(key),
                }
            }
            warn!("router-config: netstack connection closed; reinstalling interfaces");
            let mut inner = self.inner.lock();
            inner.link = None;
            inner.pending_devices.clear();
            inner.device_retry_at = None;
            inner.interfaces.clear();
            inner.delegation = Default::default();
            inner.dns = Default::default();
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
                    error!("router-config: netstack connection failed: {status}");
                    let _ = native_runtime::system::sleep_until(
                        native_runtime::system::monotonic() + 1_000_000_000,
                    );
                }
            }
        };
        {
            let mut inner = self.inner.lock();
            inner.link = Some(link);
            self.queue_sources(&mut inner);
            self.queue_bgp(&mut inner);
            self.queue_dns(&mut inner);
        }
        self.devices_changed();
    }

    fn open_link(&self) -> Result<Link, Status> {
        let installer = RoutingConfigurator::new(
            self.services
                .connect(service_directory::ROUTING_CONFIGURATOR)?,
        );
        let control = NetworkClient::new(self.services.connect(service_directory::NETWORK)?);
        let network = control.create_namespace(true)?;
        let dns_changed = network.watch_dns().map_err(configuration_status)?;
        dns_changed.wait_async(&self.port, KEY_DNS, signals::USER_0)?;
        network
            .channel()
            .wait_async(&self.port, KEY_NETSTACK, signals::PEER_CLOSED)?;
        installer
            .channel()
            .wait_async(&self.port, KEY_ROUTING, signals::PEER_CLOSED)?;
        Ok(Link {
            installer,
            network,
            dns_changed,
        })
    }

    /// RIB 重启不代表网卡已退出；只重放持久接口决定，不再打开正在使用的设备。
    fn reconnect_routing(&self) -> Result<(), Status> {
        let installer = RoutingConfigurator::new(
            self.services
                .connect(service_directory::ROUTING_CONFIGURATOR)?,
        );
        installer
            .channel()
            .wait_async(&self.port, KEY_ROUTING, signals::PEER_CLOSED)?;
        let mut inner = self.inner.lock();
        inner
            .link
            .as_mut()
            .ok_or(native_abi::ERR_BAD_STATE)?
            .installer = installer;
        for interface in &mut inner.interfaces {
            self.queue_application(interface);
        }
        self.queue_sources(&mut inner);
        Ok(())
    }

    /// 先清除再登记目录变化等待，然后接入所有新网卡；读取之后到达的变化不会丢失。
    fn devices_changed(&self) {
        {
            let mut inner = self.inner.lock();
            inner.watch_wait = None;
            let result = (|| {
                self.watch.signal(signals::USER_0, 0)?;
                let wait = self.watch.duplicate(native_abi::rights::SAME)?;
                wait.wait_async(&self.port, KEY_DIRECTORY, signals::USER_0)?;
                Ok::<_, Status>(wait)
            })();
            match result {
                Ok(wait) => inner.watch_wait = Some(wait),
                Err(status) => error!("router-config: directory watch pending: {status}"),
            }
        }
        self.discover_devices();
    }

    /// 一次发现/重试：失败声明在推进游标前入队；单个坏设备不阻塞其后的其他设备。
    fn discover_devices(&self) {
        let mut pending = {
            let mut inner = self.inner.lock();
            inner.device_retry_at = None;
            core::mem::take(&mut inner.pending_devices)
        };
        pending.retain(|entry| match self.install(entry) {
            Ok(()) | Err(native_abi::ERR_NOT_FOUND) => false,
            Err(status) if device_installation_retryable(status) => true,
            Err(status) => {
                error!(
                    "router-config: device {} installation rejected: {status}",
                    entry.id
                );
                false
            }
        });
        {
            let mut inner = self.inner.lock();
            inner.pending_devices = pending;
            if !inner.pending_devices.is_empty() || inner.watch_wait.is_none() {
                self.queue_devices(&mut inner);
            }
        }
        loop {
            let after = self.inner.lock().last_device;
            let entries = match self.directory.list(after) {
                Ok((_, entries)) if !entries.is_empty() => entries,
                Ok(_) => return,
                Err(status) => {
                    error!("router-config: device directory failed: {status}");
                    self.queue_devices(&mut self.inner.lock());
                    return;
                }
            };
            if self
                .inner
                .lock()
                .pending_devices
                .try_reserve(entries.len())
                .is_err()
            {
                self.queue_devices(&mut self.inner.lock());
                return;
            }
            for entry in entries {
                let id = entry.id;
                if entry.class == Class::Network
                    && let Err(status) = self.install(&entry)
                {
                    error!(
                        "router-config: network device {} install failed: {status}",
                        entry.id
                    );
                    if device_installation_retryable(status) {
                        let mut inner = self.inner.lock();
                        inner.pending_devices.push(entry);
                        self.queue_devices(&mut inner);
                    }
                }
                self.inner.lock().last_device = id;
            }
        }
    }

    fn install(&self, entry: &device_protocol::directory::Entry) -> Result<(), Status> {
        let mut inner = self.inner.lock();
        inner
            .interfaces
            .try_reserve(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut name = Vec::new();
        name.try_reserve_exact(router_protocol::NAME_MAX)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let mut delegated = Vec::new();
        delegated
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        delegated.push(self.directory.duplicate()?.into_handle());
        let Some(link) = inner.link.as_ref() else {
            return Err(native_abi::ERR_PEER_CLOSED);
        };
        let (index, lifetime, link_changed) =
            link.installer
                .install_device(entry.id, &entry.description, delegated)?;
        if inner
            .interfaces
            .iter()
            .any(|interface| interface.index == index)
        {
            return Ok(());
        }
        let key = inner.next_key;
        inner.next_key = key.checked_add(1).ok_or(native_abi::ERR_OUT_OF_RANGE)?;
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
        name.extend_from_slice(&snapshot.name[..length]);
        let config = match &inner.saved {
            Some(saved) => policy::resolve(saved, &self.defaults, &name, snapshot.mac)
                .map(|(config, _)| config)
                // 设备有效但当前持久策略不能匹配；保留发现工作，配置修正后能继续，不当作坏设备永久跳过。
                .map_err(|_| native_abi::ERR_BAD_STATE)?,
            None => InterfaceConfig {
                method: Method::Disabled,
                ..InterfaceConfig::dhcp()
            },
        };
        info!(
            "router-config: installed {} as interface {index}",
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
            connected: None,
            lease: None,
            retry_at: None,
            dhcp: None,
            dhcp_server: None,
            lifetime: (key, lifetime),
            link_changed,
        });
        let position = inner.interfaces.len() - 1;
        // 安装已登记，配置应用失败由接口原重试 owner 接管，不能再排一次设备安装或重复建立 DHCP。
        let _ = self.apply(&mut inner, position);
        self.queue_delegation(&mut inner);
        Self::changed(&mut inner);
        Ok(())
    }

    /// 实际接口寿命结束后丢弃对应记录与 DHCP 会话；设备重现由目录发现重建。
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
        self.delegation_interface_removed(&mut inner, interface.index);
        self.queue_dns(&mut inner);
        info!(
            "router-config: interface {} removed",
            core::str::from_utf8(&interface.name).unwrap_or("<non-utf8>")
        );
        Self::changed(&mut inner);
        true
    }

    fn changed(inner: &mut Inner) {
        let next = Self::prepare_notification(inner).expect("router-config change event");
        Self::publish_notification(inner, next);
    }

    /// 持久提交前预备通知与代际空间，避免成功后才因 Event OOM 无法发布。
    fn prepare_notification(inner: &Inner) -> Result<Arc<Event>, Status> {
        inner
            .generation
            .checked_add(1)
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Arc::try_new(Event::create()?).map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    fn publish_notification(inner: &mut Inner, next: Arc<Event>) {
        inner.generation = inner
            .generation
            .checked_add(1)
            .expect("notification generation prepared");
        let previous = core::mem::replace(&mut inner.changed, next);
        let _ = previous.signal(0, signals::USER_0);
    }

    fn snapshot(inner: &Inner) -> Result<Snapshot, Status> {
        let now = native_runtime::system::monotonic();
        let utc_offset = native_runtime::system::utc().saturating_sub(now);
        let learned_dns = inner
            .link
            .as_ref()
            .map(|link| link.network.learned_dns())
            .transpose()
            .map_err(configuration_status)?
            .unwrap_or_default();
        Ok(Snapshot {
            generation: inner.generation,
            configuration_status: if inner.saved.is_none() {
                router_protocol::ConfigurationStatus::Rejected
            } else if inner.rollback.is_some() {
                router_protocol::ConfigurationStatus::RollingBack
            } else if inner.source_retry_at.is_some()
                || inner.bgp_retry_at.is_some()
                || inner.dns.retry_at.is_some()
                || inner.delegation.retry_at.is_some()
                || inner
                    .interfaces
                    .iter()
                    .any(|interface| interface.retry_at.is_some())
            {
                router_protocol::ConfigurationStatus::Applying
            } else {
                router_protocol::ConfigurationStatus::Ready
            },
            search_list: inner
                .saved
                .as_ref()
                .map_or_else(Vec::new, |saved| saved.dns.search_list.clone()),
            primary_dns_suffix: inner
                .saved
                .as_ref()
                .map_or_else(Default::default, |saved| saved.dns.primary_suffix.clone()),
            append_parent_suffixes: inner
                .saved
                .as_ref()
                .is_some_and(|saved| saved.dns.append_parent_suffixes),
            learned_dns,
            interfaces: inner
                .interfaces
                .iter()
                .map(|interface| InterfaceState {
                    name: interface.name.clone(),
                    connection_name: inner
                        .saved
                        .as_ref()
                        .and_then(|saved| {
                            saved
                                .matching(&interface.name, interface.mac)
                                .ok()
                                .flatten()
                        })
                        .and_then(|profile| profile.display_name.as_ref())
                        .map_or_else(Vec::new, |name| name.as_bytes().to_vec()),
                    mac: interface.mac,
                    config: interface.config.clone(),
                    address: interface.address,
                    prefix_length: interface.prefix_length,
                    gateway: interface.gateway,
                    dns: interface.nameservers().to_vec(),
                    dns_suffix: if interface.address.is_unspecified() {
                        Default::default()
                    } else if interface.config.dns_suffix.is_empty() {
                        interface
                            .lease
                            .as_ref()
                            .map_or_else(Default::default, |lease| lease.dns_suffix.clone())
                    } else {
                        interface.config.dns_suffix.clone()
                    },
                    connected_seconds: interface.connected.map_or(0, |since| {
                        u32::try_from(now.saturating_sub(since) / 1_000_000_000).unwrap_or(u32::MAX)
                    }),
                    lease: interface.lease.as_ref().map(|lease| {
                        let timing = &lease.timing;
                        let obtained_at = timing
                            .requested_at_ns
                            .saturating_add(utc_offset)
                            .div_euclid(1_000_000_000);
                        LeaseStatus {
                            server: timing.server,
                            obtained_at,
                            expires_at: (timing.duration_seconds != u32::MAX).then(|| {
                                obtained_at.saturating_add(i64::from(timing.duration_seconds))
                            }),
                        }
                    }),
                })
                .collect(),
        })
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

fn serve_settings(router_config: Arc<RouterConfig>, channel: Channel) -> Result<(), Status> {
    service_rpc::serve(
        channel,
        2,
        b"router-config-settings",
        Arc::new(
            move |ordinal: u32,
                  request: &mut service_rpc::Reader<'_>,
                  handles: Vec<native_runtime::Handle>,
                  reply: &mut service_rpc::Writer| {
                if router_protocol::ordinal::is_configuration(ordinal) {
                    return administration::handle(
                        &router_config,
                        ordinal,
                        request,
                        handles,
                        reply,
                    );
                }
                if !handles.is_empty() {
                    return Err(native_abi::ERR_INVALID_ARGS);
                }
                match ordinal {
                    router_protocol::ordinal::GET => {
                        request.finish()?;
                        let mut encoder = Encoder::default();
                        encoder.snapshot(&RouterConfig::snapshot(&router_config.inner.lock())?);
                        reply.bytes(&encoder.0);
                    }
                    router_protocol::ordinal::SET => {
                        let mut decoder = Decoder::new(request.bytes()?);
                        request.finish()?;
                        let update = decoder.update().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        decoder.finish().map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        router_config.set(update)?;
                    }
                    router_protocol::ordinal::WAIT => {
                        let generation = request.u64()?;
                        request.finish()?;
                        reply.u64(router_config.wait(generation)?);
                    }
                    router_protocol::ordinal::WAIT_CONNECTION_CHANGES => {
                        let cursor = request.bytes()?;
                        request.finish()?;
                        administration::wait_connection_changes(&router_config, cursor)?;
                    }
                    router_protocol::ordinal::DHCP => {
                        let name = request.bytes()?;
                        let action = DhcpAction::decode(request.u8()?)
                            .map_err(|_| native_abi::ERR_INVALID_ARGS)?;
                        request.finish()?;
                        router_config.dhcp_command(name, action)?;
                    }
                    router_protocol::ordinal::RENAME => {
                        let interface = request.bytes()?;
                        let name = request.bytes()?;
                        request.finish()?;
                        router_config.rename(interface, name)?;
                    }
                    _ => return Err(native_abi::ERR_NOT_SUPPORTED),
                }
                Ok(Vec::new())
            },
        ),
    )
}
