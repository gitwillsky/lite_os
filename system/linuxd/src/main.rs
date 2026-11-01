#![no_std]
#![no_main]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! linuxd：Linux 兼容层服务（Starnix 模型）。
//!
//! 每个 Linux 进程是共享进程的一个成员，每个 Linux 线程是该成员中的一个 native 线程：线程在
//! restricted mode 运行 Linux 程序，在 normal mode（本服务的代码）处理它的 syscall 与异常。

extern crate alloc;
#[macro_use]
extern crate native_runtime;

mod arch;
mod cmdline;
mod cpu;
mod deferred;
mod devices;
mod file;
mod fs;
mod handles;
mod id;
mod io_uring;
mod ipc;
mod log;
mod memory;
mod namespace;
mod net;
mod platform;
mod random;
mod syscall;
mod system;
mod task;
mod time;
mod trap;

// 可睡眠同步原语在系统服务间共享；linuxd 经 `crate::sync` 使用它们，并安装 Linux 线程的等待目标。
use service_sync as sync;

#[allow(unused_imports, reason = "日志宏经 `crate::` 路径调用的地方需要根级名字")]
pub(crate) use native_runtime::{debug, error, info, warn};

use device_protocol::boot::Milestone;
use native_runtime::Startup;
use native_runtime::native_abi::SystemBasic;
use native_runtime::sync::Once;

native_runtime::entry!(main);

/// 内核报告的系统概况（启动后不变的部分）。
pub(crate) fn system_info() -> &'static SystemBasic {
    // OWNER: CPU 数、时钟粒度、调度时间片与启动时刻在启动后不变。
    static INFO: Once<SystemBasic> = Once::new();
    INFO.call_once(native_runtime::system::basic)
}

/// 装配顺序：
///
/// 1. 取回启动句柄与 command line；
/// 2. Linux 命名空间、VFS 与任务表，然后启动 deferred 线程（定时器、控制台输入与设备活动都经它分派）；
/// 3. 读取 devmgr 设备目录并连接全部设备，注册 console；
/// 4. 装配块设备、音频、DRM、输入与 tty，挂载根文件系统，启动后台线程；
/// 5. 发布 init。main 线程随后退出（只结束本线程），linuxd 由各服务线程与 Linux 线程继续运行。
fn main(mut startup: Startup) -> i64 {
    let directory = handles::install(&mut startup);
    native_runtime::set_fatal_scope(
        handles::job()
            .duplicate(native_runtime::native_abi::rights::SAME)
            .expect("linuxd job duplicate")
            .into_handle(),
    );
    info!("linuxd started on {} CPUs", cpu::count());
    if let Some(record) = system::previous_panic() {
        warn!("previous boot ended in a kernel panic:\n{}", core::str::from_utf8(record).unwrap_or("<non-utf8>"));
    }

    let parameters = cmdline::parse(platform::kernel_command_line())
        .unwrap_or_else(|error| panic!("invalid kernel command line: {error:?}"));
    info!(
        "kernel command line: {}",
        core::str::from_utf8(platform::kernel_command_line()).unwrap_or("<non-utf8>")
    );

    initialize_namespaces();
    let vfs = fs::init_vfs();
    let scheduler = task::initialize(vfs);
    start_deferred_worker();
    let devices = devices::directory::connect(directory);
    devices.advance(Milestone::Namespaces);
    devices.advance(Milestone::Scheduler);

    platform::register_console();
    let mut disk_index = 0;
    while let Some(disk) = devices::block::device(disk_index) {
        fs::publish_block_device(disk).expect("block device publication failed");
        disk_index += 1;
    }
    devices.advance(Milestone::Devices);
    devices::audio::init(|| task::current_task().map(|thread| thread.tid_identity()))
        .expect("ALSA discovery initialization failed");
    // 最后一次开机画面推进：DRM 初始化会绑定显示活动，此后 boot scanout 的同步刷新被拒绝，
    // 进度条由 compositor 从 `KERNEL_PROGRESS_PERMILLE` 接着画。
    devices.advance(Milestone::Audio);
    if let Some(display) = device_model::display_device(0) {
        devices::drm::device::init(display).expect("primary DRM initialization failed");
    }
    devices::input::init().expect("evdev input initialization failed");
    if let Some(port) = device_model::port_device(0) {
        devices::virtio_port::init(port).expect("VirtIO port initialization failed");
    }
    let console = devices::tty::init(select_console(parameters.console.as_deref()))
        .expect("TTY initialization failed");
    net::init();
    let root = mount_filesystems(scheduler, &parameters);
    start_background_threads();
    task::spawn_init(
        scheduler,
        root,
        console,
        task::InitProgram {
            path: parameters.init.as_deref(),
            arguments: &parameters.init_arguments,
            environment: &parameters.init_environment,
        },
    );
    // 装配完成：只结束主线程。从 main 返回会结束整个进程，连同已启动的 init 与全部服务线程。
    native_runtime::task::exit_thread()
}

/// 初始 user/UTS/PID/time/IPC/NET 命名空间。
///
/// # Panics
///
/// 任一初始命名空间分配失败：没有它们无法创建 init。
fn initialize_namespaces() {
    let memory_bytes = (system_info().total_pages as usize).saturating_mul(memory::PAGE_SIZE);
    namespace::initialize(
        namespace::initial_namespace_limit(memory_bytes, trap::CARRIER_STACK),
        fs::inotify_watch_limit(memory_bytes),
    )
    .expect("initial user namespace allocation failed");
    let identity = system::identity();
    namespace::initialize_uts(identity[1].as_bytes(), identity[5].as_bytes())
        .expect("initial UTS namespace allocation failed");
    namespace::initialize_pid(cpu::count()).expect("initial PID namespace allocation failed");
    namespace::initialize_time().expect("initial time namespace allocation failed");
    ipc::initialize_namespaces().expect("initial IPC namespace allocation failed");
    net::initialize_net().expect("initial NET namespace allocation failed");
}

/// 启动唯一 deferred 线程：它取代内核 tick，分派定时器、驱动完成、控制台与网络工作。
///
/// # Panics
///
/// 线程创建失败：没有它任何阻塞等待都不会被唤醒。
fn start_deferred_worker() {
    let worker = native_runtime::thread::spawn(b"deferred", || {
        deferred::run(task::dispatch_deferred_work, task::next_deferred_deadline)
    })
    .expect("deferred worker thread");
    drop(worker);
}

/// USB sysfs、AF_UNIX 回收与 io_uring 后台线程（USB 主控与枚举在 devmgr）。
///
/// # Panics
///
/// 任一线程或其等待资源分配失败。
fn start_background_threads() {
    devices::usb::initialize().expect("USB sysfs initialization failed");
    let mut unix_gc_wait =
        task::KernelDeadlineWait::new().expect("UNIX collector deadline reservation failed");
    let unix_gc_gate =
        sync::TaskMutexWaitPreparation::prepare().expect("UNIX collector gate reservation failed");
    let unix_gc_body = alloc::boxed::Box::try_new(move || {
        net::run_unix_rights_collector(
            time::get_time_ns,
            |deadline| {
                unix_gc_wait.sleep_until(deadline);
            },
            unix_gc_gate,
        );
    })
    .expect("UNIX collector body allocation failed");
    task::spawn_kernel_thread("unix-rights-gc", unix_gc_body)
        .expect("UNIX collector thread initialization failed");
    syscall::start_io_uring_worker().expect("io_uring worker initialization failed");
}

fn mount_filesystems(scheduler: task::SchedulerReady, parameters: &cmdline::KernelParameters) -> fs::RootMounted {
    let environment = fs::MountEnvironment {
        threads: task::kernel_thread_support(scheduler),
        proc_source: alloc::sync::Arc::try_new(task::KernelProcSource).expect("proc source allocation failed"),
        cpu_count: cpu::count(),
        net_links: alloc::sync::Arc::try_new(task::KernelProcSource).expect("net link source allocation failed"),
        requester: task::current_requester,
    };
    // 没有 `root=` 时以首块盘为根（Linux 由构建期 ROOT_DEV 给出缺省）。
    let default_root;
    let root = match &parameters.root {
        Some(root) => root.as_slice(),
        None => {
            let disk = devices::block::device(0).expect("boot requires a block device for the root filesystem");
            let mut path = alloc::vec::Vec::new();
            path.try_reserve_exact(b"/dev/".len() + disk.disk_name().len())
                .expect("root path allocation failed");
            path.extend_from_slice(b"/dev/");
            path.extend_from_slice(disk.disk_name());
            default_root = path;
            default_root.as_slice()
        }
    };
    let mounted = fs::mount_root(
        environment,
        root,
        parameters.root_filesystem_type.as_deref(),
        fs::MountFlags::from_bits(
            fs::MountFlags::default().bits()
                | if parameters.read_only_root {
                    u64::from(fs::MountFlags::READ_ONLY)
                } else {
                    0
                },
        ),
    )
    .unwrap_or_else(|error| {
        panic!(
            "VFS: unable to mount root fs on {}: {error:?}",
            core::str::from_utf8(root).unwrap_or("<non-utf8>")
        )
    });
    info!("root filesystem mounted at /, devtmpfs at /dev");
    mounted
}

/// 按 `console=` 名称选择 `/dev/console` 背后的设备；未指定时取首个。
///
/// # Panics
///
/// 名称不匹配任何已注册 console 时 fail-stop：每个进程都持有一个 terminal。
fn select_console(name: Option<&[u8]>) -> alloc::sync::Arc<dyn devices::console::ConsoleDevice> {
    let mut index = 0;
    while let Some(device) = devices::console::console_device(index) {
        if name.is_none_or(|name| device.name() == name) {
            return device;
        }
        index += 1;
    }
    panic!(
        "console={} names no registered console",
        core::str::from_utf8(name.unwrap_or(b"")).unwrap_or("<non-utf8>")
    )
}
