//! 路由功能通过能力请求启动原生执行进程，调度及进程故障由根服务监督。

use alloc::{sync::Arc, vec::Vec};
use launch_protocol::{bgp, dhcp_server, dns_cache, ospf};
use native_runtime::{
    Channel, Job, Process,
    bootfs::Bootfs,
    error,
    executor::Executor,
    native_abi::{self, Status, rights, signals},
    sync::Mutex,
    system::monotonic,
    task::{CRASH_LIMIT, CRASH_WINDOW, CrashLoop, kill},
};
use service_directory::ServiceDirectory;

struct Spec {
    protocol: &'static [u8],
    name: &'static [u8],
    image: &'static [u8],
}

const SERVICES: [Spec; 4] = [
    Spec {
        protocol: service_directory::BGP_CONFIGURATOR,
        name: b"bgp",
        image: bgp::IMAGE,
    },
    Spec {
        protocol: service_directory::OSPF_CONFIGURATOR,
        name: b"ospf",
        image: ospf::IMAGE,
    },
    Spec {
        protocol: service_directory::DHCP_SERVER,
        name: b"dhcp-server",
        image: dhcp_server::IMAGE,
    },
    Spec {
        protocol: service_directory::DNS_CACHE,
        name: b"dns-cache",
        image: dns_cache::IMAGE,
    },
];

const STACK_SIZE: usize = 128 * 1024;

enum Instance {
    Stopped,
    Started(ServiceDirectory),
    BackoffUntil(i64),
}

struct State {
    instance: Instance,
    crashes: CrashLoop,
}

struct RoutingServices {
    executor: &'static Executor,
    root: Job,
    bootfs: &'static Bootfs,
    // OWNER: 每个协议的实例、启动和故障退避由同一把锁串行。只在准备进程和发布目录时持锁，
    // 协议 RPC 在锁外执行。缺少串行化时并发请求可启动多个工厂，退出也可能撤回另一实例的目录。
    states: [Mutex<State>; SERVICES.len()],
}

impl RoutingServices {
    fn new(executor: &'static Executor, root: Job, bootfs: &'static Bootfs) -> Self {
        Self {
            executor,
            root,
            bootfs,
            states: core::array::from_fn(|_| {
                Mutex::new(State {
                    instance: Instance::Stopped,
                    crashes: CrashLoop::new(),
                })
            }),
        }
    }

    fn forward(self: &Arc<Self>, protocol: &[u8], server: Channel) -> Result<(), Status> {
        let Some(index) = SERVICES.iter().position(|spec| spec.protocol == protocol) else {
            return super::route(protocol, server);
        };
        let directory = self.directory(index)?;
        directory.forward(protocol, server)
    }

    fn directory(self: &Arc<Self>, index: usize) -> Result<ServiceDirectory, Status> {
        let spec = &SERVICES[index];
        let mut state = self.states[index].lock();
        match &state.instance {
            Instance::Started(directory) => {
                return Ok(ServiceDirectory::new(
                    directory.channel().duplicate(rights::SAME)?,
                ));
            }
            Instance::BackoffUntil(deadline) if monotonic() < *deadline => {
                return Err(native_abi::ERR_SHOULD_WAIT);
            }
            _ => {}
        }
        let image = self.bootfs.image(spec.image)?;
        let (outgoing, server) = Channel::create()?;
        let directory = ServiceDirectory::new(outgoing.duplicate(rights::SAME)?);
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(1)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        handles.push(server.into_handle());
        let job = Job::create(&self.root)?;
        let process = super::launch_service(&job, &image, spec.name, handles, STACK_SIZE)?;
        state.instance = Instance::Started(ServiceDirectory::new(outgoing));
        let services = Arc::clone(self);
        self.executor.spawn(async move {
            services.monitor(index, job, process).await;
        });
        Ok(directory)
    }

    async fn monitor(&self, index: usize, job: Job, process: Process) {
        let _ = self
            .executor
            .on_signals(&process, signals::TERMINATED)
            .await;
        // 回收该工厂 Job 后才允许新请求启动替代实例，防止剩余任务继续持有网络能力。
        let label = core::str::from_utf8(SERVICES[index].name).unwrap_or("");
        let _ = kill(&job);
        while let Err(status) = self.executor.on_signals(&job, signals::TERMINATED).await {
            error!("{label} job termination wait failed: {status}");
            self.executor
                .sleep_until(monotonic().saturating_add(CRASH_WINDOW))
                .await;
        }
        let status = process.basic().map_or(-1, |basic| basic.return_code);
        let mut state = self.states[index].lock();
        state.instance = if state.crashes.record() {
            error!("{label} terminated {CRASH_LIMIT} times within 10 s (last status {status})");
            state.crashes = CrashLoop::new();
            Instance::BackoffUntil(monotonic().saturating_add(CRASH_WINDOW))
        } else {
            error!("{label} terminated with status {status}");
            Instance::Stopped
        };
    }
}

/// 构造配置服务的端点路由。闭包复用工厂的监督状态，配置服务重启时仍可连接现有实例。
///
/// 返回的连接函数接收协议名和服务端 channel，交付失败时返回原生状态码。
pub(super) fn router_connector(
    executor: &'static Executor,
    root: Job,
    bootfs: &'static Bootfs,
) -> impl Fn(&[u8], Channel) -> Result<(), Status> + Clone + Send + Sync + 'static {
    let routing = Arc::new(RoutingServices::new(executor, root, bootfs));
    move |protocol, server| routing.forward(protocol, server)
}
