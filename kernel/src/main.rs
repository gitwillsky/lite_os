#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::memory::KERNEL_SPACE;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;

// 目录即模块路径；设备类接口与总线驱动分别位于 drivers/device 与 drivers/<bus>。
// main 仅装配，子领域的依赖方向见 docs/architecture-contract.md。
mod arch;
mod config;
#[macro_use]
mod platform;
#[macro_use]
mod log;
mod boot;
mod devices;
mod execution;
mod net;

mod crash;
mod drivers;
mod fallible_tree;
mod file;
mod fs;
mod lang_item;

mod id;
mod io_uring;
mod ipc;
mod memory;
mod namespace;
mod random;
mod syscall;
mod system;

/// 内核编入的启动画面位图（`LBS1`）。
///
/// 文件系统挂载之前 GPU 就要显示它，所以无法从 rootfs 读取；具体文件由构建环境变量
/// `LITEOS_BOOT_SCENE` 选择（默认值在根目录 `.cargo/config.toml`），缺失时编译失败。
// OWNER: 内核镜像的只读数据；仅 boot CPU 在 `boot::boot_screen::BootScreen::start` 中读取，没有可变状态。
static BOOT_SCENE: &[u8] = include_bytes!(env!("LITEOS_BOOT_SCENE"));

/// 标记全局内核设施已完成初始化。
///
/// 次级 CPU 不能仅等待内核页表，因为页表会在文件系统、驱动和首个用户任务
/// 就绪前发布；缺少此屏障会让次级 CPU 提前进入调度器并访问未初始化的全局状态。
// OWNER: boot CPU publishes completion of global initialization to secondary CPUs.
static INIT_READY: AtomicBool = AtomicBool::new(false);

fn kernel_main(context: execution::entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());

    log::init();
    memory::init_allocator();
    platform::initialize(context.platform());
    platform::verify_firmware();
    execution::cpu::initialize(platform::hardware_cpu_ids(), context.hardware_cpu());
    arch::mmu::initialize_asid_allocator(execution::cpu::count());
    execution::deferred::initialize(execution::cpu::count());
    execution::task::initialize_interrupt_state();
    crash::initialize();
    info!(
        "logical CPU topology initialized: count={}, boot={:?}",
        execution::cpu::count(),
        execution::cpu::boot_id()
    );
    // boot 装配保留区策略；memory 不反向依赖 crash 领域。
    let allocator_end = crash::reserve_region(platform::physical_memory_end().into());
    memory::init(allocator_end);
    crash::recover();
    // 显示设备提前到这里建立：GPU 初始化只需要内存与设备窗口，开机画面因此在其余设备、文件系统与 scheduler
    // 之前就上屏；GPU 中断仍等 interrupt controller 就绪后在 `bind_platform_devices` 补注册。
    let display = drivers::virtio::bind_display();
    if let Some(display) = &display {
        crash::register_display(display.panic_framebuffer());
    }
    let mut boot_screen = boot::boot_screen::BootScreen::start(
        display
            .as_ref()
            .map(drivers::virtio::EarlyDisplay::framebuffer),
        BOOT_SCENE,
    );
    let parameters = boot::cmdline::parse(platform::kernel_command_line())
        .unwrap_or_else(|error| panic!("invalid kernel command line: {error:?}"));
    if let Some(level) = parameters.log_level {
        log::apply_console_loglevel(level);
    }
    crash::configure(parameters.panic_seconds);
    info!(
        "kernel command line: {}",
        core::str::from_utf8(platform::kernel_command_line()).unwrap_or("<non-utf8>")
    );
    execution::timer::init_rtc();
    let namespace_limit = namespace::initial_namespace_limit(
        memory::frame_statistics()
            .capacity_pages
            .saturating_mul(memory::PAGE_SIZE),
        memory::KERNEL_STACK_SIZE,
    );
    namespace::initialize(
        namespace_limit,
        fs::inotify_watch_limit(
            memory::frame_statistics()
                .capacity_pages
                .saturating_mul(memory::PAGE_SIZE),
        ),
    )
    .expect("initial user namespace allocation failed");
    let identity = system::identity();
    namespace::initialize_uts(identity[1].as_bytes(), identity[5].as_bytes())
        .expect("initial UTS namespace allocation failed");
    namespace::initialize_pid(execution::cpu::count())
        .expect("initial PID namespace allocation failed");
    namespace::initialize_time().expect("initial time namespace allocation failed");
    ipc::initialize_namespaces().expect("initial IPC namespace allocation failed");
    net::initialize_net().expect("initial NET namespace allocation failed");
    boot_screen.advance(boot::boot_screen::Milestone::Namespaces);
    let vfs = fs::init_vfs();
    // 启动顺序由证明 token 约束：每一步只接受前置步骤返回的 token，顺序错误无法编译。
    let scheduler = execution::task::initialize(vfs);
    boot_screen.advance(boot::boot_screen::Milestone::Scheduler);
    platform::initialize_devices();
    drivers::pci::initialize(platform::pci_host()).expect("PCI root bus initialization failed");
    drivers::virtio::bind_platform_devices(display.as_ref());
    let mut disk_index = 0;
    while let Some(disk) = devices::block::device(disk_index) {
        fs::publish_block_device(disk).expect("block device publication failed");
        disk_index += 1;
    }
    boot_screen.advance(boot::boot_screen::Milestone::Devices);
    // ALSA 发布全部 PCM outputs；DRM 与 SPICE port 分别选择首个显示与唯一 named port。
    devices::audio::init(|| execution::task::current_task().map(|thread| thread.tid_identity()))
        .expect("ALSA discovery initialization failed");
    // 最后一次内核绘制：DRM 初始化会绑定 completion work，此后 boot scanout 的同步刷新被拒绝，
    // 进度条由 compositor 从 `KERNEL_PROGRESS_PERMILLE` 接着画。
    boot_screen.advance(boot::boot_screen::Milestone::Audio);
    if let Some(display) = devices::model::display_device(0) {
        devices::drm::device::init(display).expect("primary DRM initialization failed");
    }
    devices::input::init().expect("evdev input initialization failed");
    if let Some(port) = devices::model::port_device(0) {
        devices::virtio_port::init(port).expect("VirtIO port initialization failed");
    }
    let console = devices::tty::init(select_console(parameters.console.as_deref()))
        .expect("TTY initialization failed");
    net::init();
    let root = mount_filesystems(scheduler, &parameters);
    for host in drivers::usb::initialize().expect("USB host initialization failed") {
        let body =
            alloc::boxed::Box::try_new(move || host.run()).expect("USB worker allocation failed");
        execution::task::spawn_kernel_thread("usb-enumerate", body)
            .expect("USB worker initialization failed");
    }
    let mut reclaim_wait = execution::task::KernelDeadlineWait::new()
        .expect("background reclaim deadline reservation failed");
    let reclaim_body = alloc::boxed::Box::try_new(move || {
        memory::run_background_reclaim(execution::timer::get_time_ns, |deadline| {
            reclaim_wait.sleep_until(deadline);
        })
    })
    .expect("background reclaim body allocation failed");
    execution::task::spawn_kernel_thread("memory-reclaim", reclaim_body)
        .expect("background reclaim thread initialization failed");
    let mut unix_gc_wait = execution::task::KernelDeadlineWait::new()
        .expect("UNIX collector deadline reservation failed");
    let unix_gc_gate = execution::sync::TaskMutexWaitPreparation::prepare()
        .expect("UNIX collector gate reservation failed");
    let unix_gc_body = alloc::boxed::Box::try_new(move || {
        net::run_unix_rights_collector(
            execution::timer::get_time_ns,
            |deadline| {
                unix_gc_wait.sleep_until(deadline);
            },
            unix_gc_gate,
        );
    })
    .expect("UNIX collector body allocation failed");
    execution::task::spawn_kernel_thread("unix-rights-gc", unix_gc_body)
        .expect("UNIX collector thread initialization failed");
    syscall::start_io_uring_worker().expect("io_uring worker initialization failed");
    execution::task::spawn_init(
        scheduler,
        root,
        console,
        arch::trap::user_entry(),
        execution::trap::trap_return,
        execution::task::InitProgram {
            path: parameters.init.as_deref(),
            arguments: &parameters.init_arguments,
            environment: &parameters.init_environment,
        },
    );
    // Release 发布页表、设备、文件系统和首个任务；secondary 在进入任何共享子系统前消费它。
    INIT_READY.store(true, Ordering::Release);
    for target in execution::cpu::possible().iter() {
        if target == execution::cpu::boot_id() {
            continue;
        }
        let hardware = execution::cpu::hardware_id(target);
        platform::start_cpu(hardware, arch::secondary_entry(), context.platform()).unwrap_or_else(
            |error| panic!("firmware failed to start CPU {:?}: {}", hardware, error),
        );
    }

    enter_scheduler()
}

fn mount_filesystems(
    scheduler: execution::task::SchedulerReady,
    parameters: &boot::cmdline::KernelParameters,
) -> fs::RootMounted {
    let environment = fs::MountEnvironment {
        threads: execution::task::kernel_thread_support(scheduler),
        proc_source: Arc::try_new(execution::task::KernelProcSource)
            .expect("proc source allocation failed"),
        cpu_count: execution::cpu::count(),
        net_links: Arc::try_new(execution::task::KernelProcSource)
            .expect("net link source allocation failed"),
        requester: execution::task::current_requester,
    };
    // 没有 `root=` 时以首块盘为根（Linux 由构建期 ROOT_DEV 给出缺省）。
    let default_root;
    let root = match &parameters.root {
        Some(root) => root.as_slice(),
        None => {
            let disk = devices::block::device(0)
                .expect("boot requires a block device for the root filesystem");
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
/// 名称不匹配任何已注册 console 时 fail-stop：每个进程都持有一个 terminal，不能像 Linux 那样
/// 在没有 `/dev/console` 的情况下运行 init。
fn select_console(name: Option<&[u8]>) -> Arc<dyn crate::devices::console::ConsoleDevice> {
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

fn kernel_secondary_main(context: execution::entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());
    // Acquire 消费 boot CPU 在 INIT_READY 之前完成的全部全局初始化写入。
    while !INIT_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    platform::validate_boot_info(context.platform());
    KERNEL_SPACE.wait().lock().active();

    enter_scheduler()
}

fn init_local_arch(hardware_cpu: execution::cpu::HardwareCpuId) {
    // 每个 CPU 都必须建立 architecture-local execution state；缺失会使该 CPU 无法运行用户上下文。
    arch::cpu::initialize_local_execution();
    let executing_hardware_id = execution::cpu::executing_hardware_id();
    assert_eq!(
        hardware_cpu, executing_hardware_id,
        "firmware and architecture entry CPU identities disagree"
    );

    execution::trap::init();
}

fn enter_scheduler() -> ! {
    execution::timer::enable_timer_interrupt();
    // SAFETY: local trap state and platform interrupt controllers are initialized before the
    // architecture enables scheduler interrupt delivery for this CPU.
    unsafe { arch::interrupt::enable_scheduler_interrupts() };
    execution::cpu::mark_online();
    if execution::cpu::current_id() == execution::cpu::boot_id() {
        // boot CPU 等待所有 platform target 完成本地初始化；缺失该屏障会把“start 已接受”误当成 online。
        while execution::cpu::online() != execution::cpu::possible() {
            core::hint::spin_loop();
        }
        info!(
            "all platform CPUs online: count={}, mask={:#x}",
            execution::cpu::count(),
            execution::cpu::online().native_word()
        );
    }
    // 每个 CPU 在发布 online 后只同步自己的共享 kernel translations；尚未 online 的 CPU
    // 不可作为 remote-fence target，已 online CPU 已在各自 activation 路径完成本地 fence。
    arch::mmu::flush_local();

    execution::task::run_tasks();
}
