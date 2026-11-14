#![no_std]
#![no_main]
#![feature(allocator_ext)]
//! 原生 RIB：接管接口意图与获授权来源会话，经同一求解及唯一 NET 事务发表。
//! 1. 配置来源只通过专用能力提交接口意图，不持执行侧安装能力。
//! 2. 预备租约排斥来源变化；NET 确认后才交换不可变来源，不复制 NET 事务阶段。
//! 3. 执行实例退出或确认结果不确定时退出本实例，由配置 owner 重放持久决定。
extern crate alloc;
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use native_runtime::{
    Channel, Event, Handle, Port, Startup,
    native_abi::{self, Status, signals},
    sync::Mutex,
};
use network_protocol::{NetworkClient, NetworkInstaller, RemoteNamespace};
use router_protocol::configuration::Name;
use routing_rib::sources::{PreparedRoutes, Routes};
use service_directory::ServiceDirectory;

mod source;
mod tracking;
mod transaction;
native_runtime::entry!(main);

struct State {
    routes: Routes,
    // OWNER: 一个在途来源预备租约；没有它，并发来源会用不同候选覆盖彼此。阶段及逆向资源仅归 NET。
    pending: Option<u64>,
    next: u64,
    sources: Vec<source::Registration>,
    configuration_source: u64,
    watchers: Vec<tracking::Registration>,
}
struct Rib {
    installer: NetworkInstaller,
    network: RemoteNamespace,
    // OWNER: 唯一已确认来源与预备租约。控制操作串行，NET 不反向调用 RIB，持有它等待 NET 不形成锁环。
    state: Mutex<State>,
    // OWNER: 预备租约释放的合并唤醒；接口事件在租约期间保留为待处理工作，不靠轮询竞争租约。
    released: Event,
    // OWNER: 来源析构只标记预留登记并唤醒此事件，不分配撤回正文或同步调用执行域。
    source_changes: Event,
}
impl Rib {
    fn new(installer: NetworkInstaller, network: RemoteNamespace) -> Result<Arc<Self>, Status> {
        let mut routes = Routes::default();
        let configuration_source = routes.reserve_source()?;
        Arc::try_new(Self {
            released: Event::create()?,
            source_changes: Event::create()?,
            installer,
            network,
            state: Mutex::new(State {
                routes,
                configuration_source,
                watchers: Vec::new(),
                pending: None,
                next: 0,
                sources: Vec::new(),
            }),
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)
    }

    fn facts(&self) -> Result<routing_rib::sources::Facts, Status> {
        // 调用者持有唯一 RIB 写租约/状态锁，逻辑配置不会在两次查询间改变；物理变化仍由 NET 预备核对。
        let error = |error| match error {
            network_protocol::SocketError::NoMemory => native_abi::ERR_NO_MEMORY,
            _ => native_abi::ERR_BAD_STATE,
        };
        Ok(routing_rib::sources::Facts {
            interfaces: self.network.interface_snapshots().map_err(error)?,
            addresses: self.network.address_snapshot(0).map_err(error)?,
            neighbors: self.network.neighbor_snapshot(0).map_err(error)?,
        })
    }
    fn stage(
        &self,
        state: &State,
        configuration: network_protocol::routing::Configuration<'_>,
        policy: network_protocol::ExecutionPolicy<'_>,
    ) -> Result<(Routes, network_protocol::PreparedConfiguration), Status> {
        let mut facts = self.facts()?;
        if let Some(ports) = policy.bridges {
            facts.project_bridges(ports)?;
        }
        let mut plans = Vec::new();
        plans
            .try_reserve_exact(configuration.interfaces().iter().len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        plans.extend(configuration.interfaces().iter());
        let mut replacement = Vec::new();
        replacement
            .try_reserve_exact(2)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for batch in [
            router_protocol::routing::Family::Ipv4,
            router_protocol::routing::Family::Ipv6,
        ]
        .into_iter()
        .filter_map(|family| configuration.routes(family))
        {
            replacement.push(Routes::capture_source(
                state.configuration_source,
                routing_rib::sources::CONFIGURATION_DOMAIN,
                routing_rib::sources::STATIC_PROTOCOL,
                1,
                batch,
            )?);
        }
        let tables = configuration.tables();
        let admission = configuration.into_policy();
        let PreparedRoutes { next, encoded, .. } =
            state
                .routes
                .prepare_configuration(&facts, &plans, &replacement, admission, tables)?;
        Ok((next, self.installer.prepare_configuration(encoded, policy)?))
    }

    fn refresh(&self) -> Result<RecomputeWork, Status> {
        let mut state = self.state.lock();
        if state.pending.is_some() {
            return Ok(RecomputeWork::WaitingForLease);
        }
        let mut removed = Vec::new();
        removed
            .try_reserve_exact(state.sources.len())
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        removed.extend(
            state
                .sources
                .iter()
                .filter(|source| source.retiring)
                .map(|source| source.id),
        );
        let PreparedRoutes { next, encoded, .. } =
            state
                .routes
                .prepare_sources(&self.facts()?, None, &removed)?;
        let execution = self
            .installer
            .prepare_configuration(encoded, network_protocol::ExecutionPolicy::default())?;
        if execution.activate().is_err() {
            let _ = execution.abort();
            return Err(native_abi::ERR_BAD_STATE);
        }
        transaction::confirm_execution(execution);
        let old = state.publish(next);
        state.sources.retain(|source| !source.retiring);
        drop(state);
        drop(old);
        Ok(RecomputeWork::Idle)
    }

    fn prepare(
        self: &Arc<Self>,
        configuration: network_protocol::routing::Configuration<'_>,
        policy: network_protocol::ExecutionPolicy<'_>,
    ) -> Result<Channel, Status> {
        let (client, server) = Channel::create()?;
        let (id, candidate, execution) = {
            let mut state = self.state.lock();
            if state.pending.is_some() {
                return Err(native_abi::ERR_SHOULD_WAIT);
            }
            let (candidate, execution) = self.stage(&state, configuration, policy)?;
            let id = state
                .next
                .checked_add(1)
                .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
            state.next = id;
            state.pending = Some(id);
            (id, candidate, execution)
        };
        // 租约 guard 只能在 state 解锁后创建/丢弃；分配或启动线程失败时关闭 NET 能力并归还租约，不在锁内 Drop 重入。
        transaction::Transaction::serve(self.clone(), id, None, candidate, execution, server)?;
        Ok(client)
    }
}
impl Drop for Rib {
    fn drop(&mut self) {
        // 观察线程只持 Weak；最后一个配置或来源能力关闭后唤醒它退出。
        let _ = self.released.signal(0, signals::USER_0);
    }
}

// OWNER: 名称只索引仍被配置、来源或事务能力持有的 RIB；不保存路由数据或保活空域。
struct Domains(Mutex<fallible_tree::FallibleMap<Name, Weak<Rib>>>);

struct Controller(Arc<Rib>, Option<Arc<Domains>>);
impl Controller {
    fn serve(self, channel: Channel) -> Result<(), Status> {
        service_rpc::serve(
            channel,
            1,
            b"rib-configuration",
            Arc::try_new(
                move |ordinal,
                      request: &mut service_rpc::Reader<'_>,
                      handles,
                      reply: &mut service_rpc::Writer| {
                    network_protocol::routing::dispatch_routing_configuration(
                        &self, ordinal, request, handles, reply,
                    )
                },
            )
            .map_err(|_| native_abi::ERR_NO_MEMORY)?,
        )
    }
}
impl network_protocol::routing::RoutingConfigurationService for Controller {
    fn set_forwarding_cpus(&self, mask: u64) -> Result<(), Status> {
        if self.1.is_none() {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        self.0.installer.set_forwarding_cpus(mask)
    }

    fn open_routing_domain(&self, name: Name) -> Result<(Channel, Channel), Status> {
        let directory = self.1.as_ref().ok_or(native_abi::ERR_ACCESS_DENIED)?;
        let rib = {
            let mut domains = directory.0.lock();
            if let Some(rib) = domains.get(&name).and_then(Weak::upgrade) {
                rib
            } else {
                domains.retain(|_, rib| rib.strong_count() != 0);
                let (installer, network) = self.0.installer.open_routing_domain(&name)?;
                let rib = Rib::new(installer, network)?;
                let (port, changes) = watch_port(&rib)?;
                let weak = Arc::downgrade(&rib);
                drop(native_runtime::thread::spawn(b"rib-domain", move || {
                    if let Err(status) = watch_changes(weak, port, changes) {
                        native_runtime::warn!("routing-rib: domain observer stopped: {status}");
                    }
                })?);
                domains
                    .try_insert(name, Arc::downgrade(&rib))
                    .map_err(|_| native_abi::ERR_NO_MEMORY)?;
                rib
            }
        };
        let network = rib.network.channel().duplicate(native_abi::rights::SAME)?;
        let (client, server) = Channel::create()?;
        Controller(rib, None).serve(server)?;
        Ok((client, network))
    }

    fn project_bridges(
        &self,
        ports: router_protocol::switching::Ports<'_>,
        plans: router_protocol::interface::InterfaceConfigurationBatch<'_>,
    ) -> Result<Vec<u8>, Status> {
        self.0.installer.project_bridges(ports, plans)
    }

    fn firewall_counters(
        &self,
        cursor: router_protocol::firewall::counters::Cursor,
    ) -> Result<Vec<u8>, Status> {
        self.0.installer.firewall_counters(cursor)
    }

    fn connection_changes(
        &self,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> Result<Vec<u8>, Status> {
        self.0.installer.connection_changes(cursor)
    }

    fn arm_connection_watch(
        &self,
        cursor: router_protocol::connections::changes::Cursor,
    ) -> Result<Option<Event>, Status> {
        self.0.installer.arm_connection_watch(cursor)
    }

    fn clear_connection(
        &self,
        identity: router_protocol::connections::Identity,
    ) -> Result<(), Status> {
        self.0.installer.clear_connection(identity)
    }

    fn connections(&self, cursor: router_protocol::connections::Cursor) -> Result<Vec<u8>, Status> {
        self.0.installer.connections(cursor)
    }

    fn track(
        &self,
        queries: Vec<router_protocol::tracking::Query>,
    ) -> Result<(Channel, Event), Status> {
        self.0.track(queries, None)
    }
    fn open_source(
        &self,
        grant: network_protocol::routing::Grant,
    ) -> Result<(Channel, Channel), Status> {
        let network = self
            .0
            .network
            .channel()
            .duplicate(native_abi::rights::SAME)?;
        Ok((self.0.open_source(grant)?, network))
    }
    fn install_device(
        &self,
        id: u64,
        domain: Option<Name>,
        description: Vec<u8>,
        handles: Vec<Handle>,
    ) -> Result<(u32, Channel, native_runtime::Event), Status> {
        if self.1.is_none() {
            return Err(native_abi::ERR_ACCESS_DENIED);
        }
        self.0
            .installer
            .install_device(id, domain.as_ref(), &description, handles)
    }
    fn prepare_configuration(
        &self,
        configuration: Option<network_protocol::routing::Configuration<'_>>,
        policy: network_protocol::ExecutionPolicy<'_>,
    ) -> Result<Channel, Status> {
        // 仅执行策略不改变候选目录；直接移交 NET 的同一写租约，避免全表求解及虚假的 RIB 代际。
        let Some(configuration) = configuration else {
            return Ok(self
                .0
                .installer
                .prepare_configuration(Vec::new(), policy)?
                .into_channel());
        };
        self.0.prepare(configuration, policy)
    }
}

// OWNER: 主线程唯一的未完成重选工作；不是接口状态缓存。租约释放才重试等待项，临时失败才有截止。
enum RecomputeWork {
    Idle,
    WaitingForLease,
    Retry(i64),
}

const ROUTING_INPUTS: u32 = NetworkInstaller::TOPOLOGY_CHANGED | NetworkInstaller::GATEWAYS_CHANGED;

fn watch_port(rib: &Rib) -> Result<(Port, Event), Status> {
    let port = Port::create()?;
    let changes = rib.installer.watch_routing_inputs()?;
    rib.installer
        .channel()
        .wait_async(&port, 0, signals::PEER_CLOSED)?;
    changes.wait_async(&port, 1, ROUTING_INPUTS)?;
    rib.released.wait_async(&port, 2, signals::USER_0)?;
    rib.source_changes.wait_async(&port, 3, signals::USER_0)?;
    Ok((port, changes))
}

fn watch_changes(rib: Weak<Rib>, port: Port, changes: Event) -> Result<(), Status> {
    let mut work = RecomputeWork::Retry(native_runtime::system::monotonic());
    loop {
        let deadline = match work {
            RecomputeWork::Retry(deadline) => deadline,
            _ => native_abi::DEADLINE_INFINITE,
        };
        let packet = port.wait(deadline);
        let Some(rib) = rib.upgrade() else {
            return Ok(());
        };
        match packet {
            Ok(packet) if packet.key == 0 => return Err(native_abi::ERR_PEER_CLOSED),
            Ok(packet) => {
                if packet.status != native_abi::OK {
                    return Err(packet.status);
                }
                match packet.key {
                    1 => {
                        // clear -> arm -> snapshot：计算期间的新变化仍留在 port，不把合并事件当作变化历史。
                        changes.signal(ROUTING_INPUTS, 0)?;
                        changes.wait_async(&port, 1, ROUTING_INPUTS)?;
                        work = RecomputeWork::Retry(native_runtime::system::monotonic());
                    }
                    2 => {
                        rib.released.signal(signals::USER_0, 0)?;
                        rib.released.wait_async(&port, 2, signals::USER_0)?;
                        if !matches!(work, RecomputeWork::Idle) {
                            work = RecomputeWork::Retry(native_runtime::system::monotonic());
                        }
                    }
                    3 => {
                        rib.source_changes.signal(signals::USER_0, 0)?;
                        rib.source_changes.wait_async(&port, 3, signals::USER_0)?;
                        work = RecomputeWork::Retry(native_runtime::system::monotonic());
                    }
                    _ => return Err(native_abi::ERR_BAD_STATE),
                }
            }
            Err(native_abi::ERR_TIMED_OUT) => {}
            Err(status) => return Err(status),
        }
        if matches!(work, RecomputeWork::Retry(deadline) if deadline <= native_runtime::system::monotonic())
        {
            work = rib.refresh().unwrap_or_else(|status| {
                native_runtime::warn!("routing-rib: source recomputation pending: {status}");
                RecomputeWork::Retry(
                    native_runtime::system::monotonic().saturating_add(500_000_000),
                )
            });
        }
    }
}

fn main(mut startup: Startup) -> i64 {
    match run(&mut startup) {
        Ok(()) => 0,
        Err(status) => {
            native_runtime::error!("routing-rib: service failed: {status}");
            -1
        }
    }
}
fn run(startup: &mut Startup) -> Result<(), Status> {
    let outgoing = Channel::from_handle(startup.take(launch_protocol::routing_rib::OUTGOING));
    let services = ServiceDirectory::new(Channel::from_handle(
        startup.take(launch_protocol::routing_rib::SERVICES),
    ));
    let installer = NetworkInstaller::new(services.connect(service_directory::NETWORK_INSTALLER)?);
    let network =
        NetworkClient::new(services.connect(service_directory::NETWORK)?).create_namespace(true)?;
    let rib = Rib::new(installer, network)?;
    let (port, changes) = watch_port(&rib)?;
    let controller = rib.clone();
    let domains = Arc::try_new(Domains(Mutex::new(fallible_tree::FallibleMap::new())))
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
    service_directory::serve_directory(
        outgoing,
        b"rib-outgoing",
        Arc::try_new(move |protocol: &[u8], channel| {
            if protocol != service_directory::ROUTING_CONFIGURATOR {
                return Err(native_abi::ERR_NOT_FOUND);
            }
            Controller(controller.clone(), Some(domains.clone())).serve(channel)
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?,
    )?;
    native_runtime::info!("routing-rib ready");
    // NET 退出后旧接口身份全部失效；不把上一实例的来源投递到新实例。
    watch_changes(Arc::downgrade(&rib), port, changes)
}
