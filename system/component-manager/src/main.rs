#![no_std]
#![no_main]

//! 根服务：内核启动的唯一进程。
//!
//! 1. 从启动消息取得根 Job、根 resource、bootfs 与 DTB；
//! 2. 为根 Job 建立异常 channel：任何系统服务未处理的异常都在这里记录，随后交还内核终止该进程；
//! 3. 启动 driver-manager（持有 DTB 与设备目录服务端，按节点启动驱动宿主），根服务持有完整目录连接并按设备类路由：
//!    Network 类给 netcfg，Block 类给 fshost，Pcm 类给 audio-core，其余给 linuxd；
//! 4. 启动 fshost（交给它 ext4 实例的 Job 与映像，由它为每块盘启动 ext4），以及受监督的 netstack、dhcp-client、
//!    netcfg、audio-core 与 linuxd；
//!    每个消费者拿到只含它声明协议的路由目录（[`ROUTES`] 与各 `*_USES`），连接转发给提供者的当前实例
//!    （能力路由，对应 Fuchsia component_manager）；netcfg 另得 fshost 派生的私有数据目录与 bootfs 默认策略；
//! 5. 受监督服务终止后重启（linuxd 先终止整个 Linux Job），`native_runtime::task::CRASH_WINDOW` 内达到 `CRASH_LIMIT` 次
//!    判定为崩溃循环并关机；driver-manager、fshost 终止时系统无法继续服务：记录退出状态并关机。

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec;

use device_protocol::directory::{ALL_CLASSES, Class, DirectoryClient};
use launch_protocol::{
    audio_core, dhcp_client, driver_host, driver_manager, ext4, fshost, linuxd, netcfg, netstack,
};
use native_runtime::executor::Executor;
use native_runtime::native_abi::{self, ExceptionReport, Status, rights, signals, startup};
use native_runtime::sync::{Mutex, Once};
use native_runtime::task::{CRASH_LIMIT, CrashLoop};
use native_runtime::{
    Channel, Event, Handle, Job, Process, Resource, Startup, Vmar, Vmo, error, info, warn,
};
use service_directory::ServiceDirectory;

native_runtime::entry!(main);

const LINUXD_STACK_SIZE: usize = 512 * 1024;
const DRIVER_MANAGER_STACK_SIZE: usize = 256 * 1024;
const NETSTACK_STACK_SIZE: usize = 256 * 1024;
const FSHOST_STACK_SIZE: usize = 128 * 1024;
const NETCFG_STACK_SIZE: usize = 128 * 1024;
const DHCP_STACK_SIZE: usize = 128 * 1024;
const AUDIO_STACK_SIZE: usize = 128 * 1024;

// OWNER: bootfs 映射永不解除；监督任务重启服务时重复从中取映像。
static BOOTFS: Once<native_runtime::bootfs::Bootfs> = Once::new();

/// 受监督服务的当前实例：路由目录经它把连接转发给活着的实例。
struct Supervised {
    // OWNER: 当前实例的 outgoing；实例终止后为 None，重启完成后换成新实例。
    outgoing: Mutex<Option<Arc<ServiceDirectory>>>,
    // FLAG(USER_0): 当前实例可用；缺失时等待重启的连接请求只能轮询。
    available: Event,
}

impl Supervised {
    fn new() -> Self {
        Self {
            outgoing: Mutex::new(None),
            available: Event::create().expect("supervision event"),
        }
    }

    fn install(&self, outgoing: ServiceDirectory) {
        *self.outgoing.lock() = Some(Arc::new(outgoing));
        let _ = self.available.signal(0, signals::USER_0);
    }

    /// 撤下 `instance`（仍是当前实例时）；重启前与发现已终止的转发者都会调用，重复调用无害。
    fn retire(&self, instance: &Arc<ServiceDirectory>) {
        let mut outgoing = self.outgoing.lock();
        if outgoing
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, instance))
        {
            let _ = self.available.signal(signals::USER_0, 0);
            *outgoing = None;
        }
    }

    /// 把连接转发给当前实例；实例已终止时等待重启完成再转发。
    fn forward(&self, protocol: &[u8], server: Channel) -> Result<(), Status> {
        loop {
            let current = self.outgoing.lock().clone();
            if let Some(instance) = current {
                match instance.channel().wait_one(signals::PEER_CLOSED, 0) {
                    Err(native_abi::ERR_TIMED_OUT) => return instance.forward(protocol, server),
                    _ => self.retire(&instance),
                }
            }
            self.available
                .wait_one(signals::USER_0, native_abi::DEADLINE_INFINITE)?;
        }
    }
}

/// 协议的提供者。
#[derive(Clone, Copy)]
enum Provider {
    Netstack,
    Fshost,
    Netcfg,
    Dhcp,
    AudioCore,
}

// OWNER: 各提供者的唯一实例状态；路由目录线程与监督任务共享。fshost 不重启（终止即关机）。
static NETSTACK: Once<Supervised> = Once::new();
static FSHOST: Once<Supervised> = Once::new();
static NETCFG: Once<Supervised> = Once::new();
static DHCP: Once<Supervised> = Once::new();
// OWNER: audio-core 的实例状态；linuxd 的音频连接经它转发给当前实例。
static AUDIO_CORE: Once<Supervised> = Once::new();

impl Provider {
    fn instance(self) -> &'static Supervised {
        let slot = match self {
            Self::Netstack => &NETSTACK,
            Self::Fshost => &FSHOST,
            Self::Netcfg => &NETCFG,
            Self::Dhcp => &DHCP,
            Self::AudioCore => &AUDIO_CORE,
        };
        slot.call_once(Supervised::new)
    }
}

/// 协议 → 提供者（对应 Fuchsia 组件的 `expose`）。
const ROUTES: &[(&[u8], Provider)] = &[
    (service_directory::NETWORK, Provider::Netstack),
    (service_directory::NETWORK_INSTALLER, Provider::Netstack),
    (service_directory::BLOCKS, Provider::Fshost),
    (service_directory::EXT4, Provider::Fshost),
    (service_directory::STORAGE_ADMIN, Provider::Fshost),
    (service_directory::NETWORK_SETTINGS, Provider::Netcfg),
    (service_directory::DHCP, Provider::Dhcp),
    (service_directory::AUDIO, Provider::AudioCore),
];

/// 各消费者可连接的协议（对应 Fuchsia 组件的 `use`）；路由目录拒绝未声明的协议。
const LINUXD_USES: &[&[u8]] = &[
    service_directory::NETWORK,
    service_directory::BLOCKS,
    service_directory::EXT4,
    service_directory::NETWORK_SETTINGS,
    service_directory::AUDIO,
];
const NETCFG_USES: &[&[u8]] = &[
    service_directory::NETWORK,
    service_directory::NETWORK_INSTALLER,
    service_directory::DHCP,
];
const DHCP_USES: &[&[u8]] = &[service_directory::NETWORK];

/// 一个受监督原生服务的启动描述。
struct ServiceSpec {
    name: &'static [u8],
    label: &'static str,
    image: &'static [u8],
    stack_size: usize,
}

const NETSTACK_SPEC: ServiceSpec = ServiceSpec {
    name: b"netstack",
    label: "netstack",
    image: netstack::IMAGE,
    stack_size: NETSTACK_STACK_SIZE,
};
const AUDIO_SPEC: ServiceSpec = ServiceSpec {
    name: b"audio-core",
    label: "audio-core",
    image: audio_core::IMAGE,
    stack_size: AUDIO_STACK_SIZE,
};
const NETCFG_SPEC: ServiceSpec = ServiceSpec {
    name: b"netcfg",
    label: "netcfg",
    image: netcfg::IMAGE,
    stack_size: NETCFG_STACK_SIZE,
};
const DHCP_SPEC: ServiceSpec = ServiceSpec {
    name: b"dhcp-client",
    label: "dhcp-client",
    image: dhcp_client::IMAGE,
    stack_size: DHCP_STACK_SIZE,
};

fn main(mut startup: Startup) -> i64 {
    let job = Job::from_handle(startup.take(startup::root::JOB));
    let resource =
        ROOT_RESOURCE.call_once(|| Resource::from_handle(startup.take(startup::root::RESOURCE)));
    let bootfs_vmo = Vmo::from_handle(startup.take(startup::root::BOOTFS));
    let device_tree = startup.take(startup::root::DEVICE_TREE);
    native_runtime::set_fatal_scope(
        job.duplicate(rights::SAME)
            .expect("root job duplicate")
            .into_handle(),
    );

    let exceptions =
        native_runtime::task::create_exception_channel(&job).expect("root exception channel");
    let bootfs = BOOTFS
        .call_once(|| native_runtime::bootfs::Bootfs::map(&bootfs_vmo).expect("bootfs mapping"));
    let (directory, directory_server) = Channel::create().expect("device directory channel");
    let directory = DirectoryClient::new(directory);
    let devices = start_driver_manager(&job, resource, bootfs, device_tree, directory_server);
    // fshost 为每块盘启动 ext4 实例（Fuchsia fshost 启动文件系统）：只交给它实例的 Job 与映像。
    let filesystems = Job::create(&job).expect("filesystem job");
    let ext4_image = bootfs_copy(bootfs, ext4::IMAGE);
    let (storage, storage_server) = Channel::create().expect("fshost outgoing channel");
    let storage_host = start_service(
        &job,
        bootfs,
        fshost::IMAGE,
        b"fshost",
        vec![
            storage_server.into_handle(),
            directory
                .scope(Class::Block.bit())
                .expect("fshost device scope")
                .into_channel()
                .into_handle(),
            filesystems.into_handle(),
            ext4_image.into_handle(),
        ],
        FSHOST_STACK_SIZE,
    );
    Provider::Fshost
        .instance()
        .install(ServiceDirectory::new(storage));
    let devices_root = Arc::new(directory);
    let linux_devices = devices_root.clone();
    let netcfg_devices = devices_root.clone();
    let command_line: &'static [u8] =
        alloc::boxed::Box::leak(core::mem::take(&mut startup.arguments).into_boxed_slice());
    let executor = Executor::create().expect("root executor");
    let netstack_job = job.duplicate(rights::SAME).expect("root job duplicate");
    executor.spawn(async move {
        supervise(
            executor,
            &netstack_job,
            bootfs,
            &NETSTACK_SPEC,
            Provider::Netstack,
            alloc::vec::Vec::new,
        )
        .await;
    });
    let dhcp_job = job.duplicate(rights::SAME).expect("root job duplicate");
    executor.spawn(async move {
        supervise(
            executor,
            &dhcp_job,
            bootfs,
            &DHCP_SPEC,
            Provider::Dhcp,
            || vec![services(DHCP_USES).into_handle()],
        )
        .await;
    });
    let audio_job = job.duplicate(rights::SAME).expect("root job duplicate");
    let audio_devices = devices_root.clone();
    executor.spawn(async move {
        supervise(
            executor,
            &audio_job,
            bootfs,
            &AUDIO_SPEC,
            Provider::AudioCore,
            || {
                vec![
                    audio_devices
                        .scope(Class::Pcm.bit())
                        .expect("audio-core device scope")
                        .into_channel()
                        .into_handle(),
                ]
            },
        )
        .await;
    });
    let netcfg_job = job.duplicate(rights::SAME).expect("root job duplicate");
    executor.spawn(async move {
        supervise(
            executor,
            &netcfg_job,
            bootfs,
            &NETCFG_SPEC,
            Provider::Netcfg,
            || {
                vec![
                    services(NETCFG_USES).into_handle(),
                    netcfg_devices
                        .scope(Class::Network.bit())
                        .expect("netcfg device scope")
                        .into_channel()
                        .into_handle(),
                    private_storage(b"netcfg").into_handle(),
                    bootfs_copy(bootfs, netcfg::DEFAULT_CONFIG).into_handle(),
                ]
            },
        )
        .await;
    });
    executor.spawn(async move {
        loop {
            if executor
                .on_signals(&exceptions, signals::READABLE | signals::PEER_CLOSED)
                .await
                .is_err()
            {
                return;
            }
            while let Ok(message) = exceptions.read() {
                report_exception(&message.bytes);
                // 丢弃异常令牌：交给内核的默认处理（终止出错进程）。
                drop(message.handles);
            }
        }
    });
    executor.spawn(async move {
        let _ = executor.on_signals(&devices, signals::TERMINATED).await;
        native_runtime::task::exit_process(power_off(&devices, "driver-manager"));
    });
    // 内存压力等级变化（Fuchsia memory_monitor）：内核已在缺页入口淘汰干净的文件页，仍不足时进入压力。
    let pressure = native_runtime::system::event(
        ROOT_RESOURCE.get().expect("root resource installed"),
        native_abi::system::EVENT_MEMORY_PRESSURE,
    )
    .expect("memory pressure event");
    executor.spawn(async move {
        loop {
            let _ = executor
                .on_signals(&pressure, native_abi::system::MEMORY_PRESSURE)
                .await;
            warn!("memory pressure: free memory below the low watermark after page eviction");
            let _ = executor
                .on_signals(&pressure, native_abi::system::MEMORY_NORMAL)
                .await;
            info!("memory pressure relieved");
        }
    });
    executor.spawn(async move {
        let _ = executor
            .on_signals(&storage_host, signals::TERMINATED)
            .await;
        native_runtime::task::exit_process(power_off(&storage_host, "fshost"));
    });
    let linux_root = job.duplicate(rights::SAME).expect("root job duplicate");
    executor.block_on(async move {
        supervise_linux(
            executor,
            &linux_root,
            resource,
            bootfs,
            &linux_devices,
            command_line,
        )
        .await
    })
}

/// 服务路由（对应 Fuchsia component_manager 的能力路由）：把消费者的连接转发给提供该协议的当前实例。
fn route(uses: &[&[u8]], protocol: &[u8], server: Channel) -> Result<(), Status> {
    if !uses.contains(&protocol) {
        return Err(native_abi::ERR_ACCESS_DENIED);
    }
    let (_, provider) = ROUTES
        .iter()
        .find(|(name, _)| *name == protocol)
        .ok_or(native_abi::ERR_NOT_FOUND)?;
    provider.instance().forward(protocol, server)
}

/// 为一个消费者实例服务新的路由目录（只含它声明使用的协议），返回交给它的一端。
fn services(uses: &'static [&'static [u8]]) -> Channel {
    let (client, server) = Channel::create().expect("service directory channel");
    service_directory::serve_directory(
        server,
        b"root-services",
        Arc::new(move |protocol: &[u8], channel: Channel| route(uses, protocol, channel)),
    )
    .expect("service directory");
    client
}

/// 经 fshost 为服务 `name` 派生私有数据目录（Fuchsia `storage` 能力）。
fn private_storage(name: &[u8]) -> Channel {
    let (admin, server) = Channel::create().expect("storage admin channel");
    Provider::Fshost
        .instance()
        .forward(service_directory::STORAGE_ADMIN, server)
        .expect("storage admin connection");
    filesystem_protocol::StorageAdmin::new(admin)
        .open(name)
        .expect("private storage")
}

/// 把 bootfs 中的文件复制为 VMO 交给服务：只读配置（Fuchsia `/config/data`），或自行启动子进程的服务
/// （driver-manager、fshost）所需的映像。
fn bootfs_copy(bootfs: &native_runtime::bootfs::Bootfs, path: &[u8]) -> Vmo {
    let data = bootfs
        .find(path)
        .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(path).unwrap_or("")))
        .data;
    let vmo = Vmo::create(data.len() as u64, 0).expect("bootfs copy vmo");
    vmo.write(data, 0).expect("bootfs copy write");
    vmo
}

/// 运行并监督 linuxd（Linux 容器）：主成员终止后终止整个 linuxd Job，等全部 Linux 进程结束、
/// 独占设备会话与网络域随之回收，再以新的设备目录范围与路由目录启动新实例；崩溃循环时关机。
async fn supervise_linux(
    executor: &'static Executor,
    root: &Job,
    resource: &Resource,
    bootfs: &'static native_runtime::bootfs::Bootfs,
    devices: &DirectoryClient,
    command_line: &[u8],
) -> i64 {
    let mut crashes = CrashLoop::new();
    loop {
        let (linux, job) = start_linuxd(
            root,
            resource,
            bootfs,
            devices
                .scope(
                    ALL_CLASSES & !Class::Network.bit() & !Class::Block.bit() & !Class::Pcm.bit(),
                )
                .expect("linuxd device scope")
                .into_channel(),
            services(LINUXD_USES),
            command_line,
        );
        let _ = executor.on_signals(&linux, signals::TERMINATED).await;
        let _ = native_runtime::task::kill(&job);
        let _ = executor.on_signals(&job, signals::TERMINATED).await;
        let status = linux.basic().map_or(-1, |basic| basic.return_code);
        if crashes.record() {
            error!("linuxd terminated {CRASH_LIMIT} times within 10 s (last status {status})");
            return power_off(&linux, "linuxd");
        }
        error!("linuxd terminated with status {status}; restarting");
    }
}

/// 运行并监督一个可重启的原生服务：每个实例拿到新的 outgoing（第一个角色句柄）与 `handles` 组装的其余
/// 角色句柄；终止后撤下实例并重启，崩溃循环时关机。
async fn supervise(
    executor: &'static Executor,
    job: &Job,
    bootfs: &'static native_runtime::bootfs::Bootfs,
    spec: &ServiceSpec,
    provider: Provider,
    mut handles: impl FnMut() -> alloc::vec::Vec<Handle>,
) {
    let supervised = provider.instance();
    let mut crashes = CrashLoop::new();
    loop {
        let (outgoing, outgoing_server) = Channel::create().expect("outgoing channel");
        let mut role = vec![outgoing_server.into_handle()];
        role.extend(handles());
        let process = start_service(job, bootfs, spec.image, spec.name, role, spec.stack_size);
        supervised.install(ServiceDirectory::new(outgoing));
        let _ = executor.on_signals(&process, signals::TERMINATED).await;
        if let Some(instance) = supervised.outgoing.lock().clone() {
            supervised.retire(&instance);
        }
        let status = process.basic().map_or(-1, |basic| basic.return_code);
        if crashes.record() {
            error!(
                "{} terminated {CRASH_LIMIT} times within 10 s (last status {status})",
                spec.label
            );
            native_runtime::task::exit_process(power_off(&process, spec.label));
        }
        error!("{} terminated with status {status}; restarting", spec.label);
    }
}

/// 系统服务终止后无法继续：记录退出状态并关机；关机请求被拒绝时返回根服务的退出码。
fn power_off(process: &Process, name: &str) -> i64 {
    let status = process.basic().map_or(-1, |basic| basic.return_code);
    error!("{name} terminated with status {status}; powering off");
    let resource = ROOT_RESOURCE.get().expect("root resource installed");
    let _ = native_runtime::system::power(resource, native_abi::system::POWER_OFF);
    -1
}

// OWNER: 根 resource；关机路径从任一服务的终止观察者使用它。
static ROOT_RESOURCE: native_runtime::sync::Once<Resource> = native_runtime::sync::Once::new();

fn report_exception(bytes: &[u8]) {
    let mut report = ExceptionReport::default();
    let size = core::mem::size_of::<ExceptionReport>();
    if bytes.len() < size {
        error!("malformed exception report ({} bytes)", bytes.len());
        return;
    }
    native_runtime::bytes_of_mut(&mut report).copy_from_slice(&bytes[..size]);
    error!(
        "unhandled exception in thread {}: kind {} access {} address {:#x} syndrome {:#x} pc {:#x}",
        report.thread_koid,
        report.kind,
        report.access,
        report.address,
        report.syndrome,
        program_counter(&report.state),
    );
}

#[cfg(target_arch = "aarch64")]
fn program_counter(state: &native_abi::register::RegisterState) -> u64 {
    state.pc
}

#[cfg(target_arch = "riscv64")]
fn program_counter(state: &native_abi::register::RegisterState) -> u64 {
    state.pc
}

/// 启动 linuxd：共享进程承载映像，首个成员进程运行首个线程。
fn start_linuxd(
    root: &Job,
    resource: &Resource,
    bootfs: &native_runtime::bootfs::Bootfs,
    directory: Channel,
    services: Channel,
    command_line: &[u8],
) -> (Process, Job) {
    let image = bootfs
        .find(linuxd::IMAGE)
        .unwrap_or_else(|| {
            panic!(
                "bootfs lacks {}",
                core::str::from_utf8(linuxd::IMAGE).unwrap_or("")
            )
        })
        .data;
    let job = Job::create(root).expect("linuxd job");
    let (shared, shared_vmar) = Process::create(&job, b"linuxd-image", native_abi::process::SHARED)
        .expect("linuxd shared process");
    let (process, _private) = shared.create_shared(b"linuxd").expect("linuxd process");
    let runtime_vmar: Vmar = shared_vmar
        .duplicate(rights::SAME)
        .expect("linuxd vmar duplicate");
    // linuxd 只需要系统操作，不交付设备 MMIO/IRQ/IOMMU 的根 authority。
    let system_resource = Resource::create(
        resource,
        native_abi::resource::SYSTEM,
        native_abi::resource::system::DEBUGLOG,
        native_abi::resource::system::COUNT,
    )
    .expect("linuxd system resource");
    // 观察者只拿到可枚举、可查询的根 Job：新增服务自动可见，且不能借此终止或修改任何任务。
    let observer = root
        .duplicate(rights::INSPECT | rights::ENUMERATE | rights::TRANSFER)
        .expect("root job inspect capability");
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &shared_vmar,
        runtime_vmar,
        name: b"linuxd",
        image,
        arguments: command_line,
        handles: vec![
            shared.into_handle(),
            job.duplicate(rights::SAME)
                .expect("job duplicate")
                .into_handle(),
            system_resource.into_handle(),
            directory.into_handle(),
            services.into_handle(),
            observer.into_handle(),
        ],
        stack_size: LINUXD_STACK_SIZE,
    })
    .expect("linuxd launch");
    info!("linuxd started");
    (process, job)
}

/// 启动 driver-manager：持有根 resource、DTB、设备目录写端，以及启动驱动宿主所需的 Job 与映像。
fn start_driver_manager(
    root: &Job,
    resource: &Resource,
    bootfs: &native_runtime::bootfs::Bootfs,
    device_tree: Handle,
    directory: Channel,
) -> Process {
    let find = |path: &[u8]| {
        bootfs
            .find(path)
            .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(path).unwrap_or("")))
            .data
    };
    let image = find(driver_manager::IMAGE);
    let host_vmo = bootfs_copy(bootfs, driver_host::IMAGE);
    let job = Job::create(root).expect("driver-manager job");
    // 驱动宿主运行在 driver-manager 的 Job 之下：driver-manager 终止时全部宿主随之终止。
    let drivers = Job::create(&job).expect("driver host job");
    let (process, vmar) =
        Process::create(&job, b"driver-manager", 0).expect("driver-manager process");
    let runtime_vmar: Vmar = vmar
        .duplicate(rights::SAME)
        .expect("driver-manager vmar duplicate");
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &vmar,
        runtime_vmar,
        name: b"driver-manager",
        image,
        arguments: &[],
        handles: vec![
            resource
                .duplicate(rights::SAME)
                .expect("resource duplicate")
                .into_handle(),
            device_tree,
            directory.into_handle(),
            drivers.into_handle(),
            host_vmo.into_handle(),
        ],
        stack_size: DRIVER_MANAGER_STACK_SIZE,
    })
    .expect("driver-manager launch");
    info!("driver-manager started");
    process
}

/// 在独立 Job 中启动普通进程形态的系统服务（ext4、netstack），交付按其启动约定排列的角色句柄。
fn start_service(
    root: &Job,
    bootfs: &native_runtime::bootfs::Bootfs,
    image: &[u8],
    name: &[u8],
    handles: alloc::vec::Vec<Handle>,
    stack_size: usize,
) -> Process {
    let label = core::str::from_utf8(name).unwrap_or("");
    let image = bootfs
        .find(image)
        .unwrap_or_else(|| panic!("bootfs lacks {}", core::str::from_utf8(image).unwrap_or("")))
        .data;
    let job = Job::create(root).unwrap_or_else(|status| panic!("{label} job: {status}"));
    let (process, vmar) =
        Process::create(&job, name, 0).unwrap_or_else(|status| panic!("{label} process: {status}"));
    let runtime_vmar: Vmar = vmar
        .duplicate(rights::SAME)
        .unwrap_or_else(|status| panic!("{label} vmar duplicate: {status}"));
    native_runtime::launch::launch(native_runtime::launch::Launch {
        process: &process,
        image_vmar: &vmar,
        runtime_vmar,
        name,
        image,
        arguments: &[],
        handles,
        stack_size,
    })
    .unwrap_or_else(|status| panic!("{label} launch: {status}"));
    info!("{label} started");
    process
}
