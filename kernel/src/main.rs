#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::memory::KERNEL_SPACE;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;

mod arch;
mod audio;
mod cmdline;
mod config;
mod cpu;
mod entry;
#[macro_use]
mod platform;
#[macro_use]
mod log;

mod drivers;
mod drm;
mod fallible_tree;
mod fs;
mod lang_item;

mod id;
mod input;
mod ipc;
mod memory;
mod random;
mod socket;
mod sync;
mod syscall;
mod system;
mod task;
mod timer;
mod trap;
mod virtio_port;

/// 标记全局内核设施已完成初始化。
///
/// 次级 CPU 不能仅等待内核页表，因为页表会在文件系统、驱动和首个用户任务
/// 就绪前发布；缺少此屏障会让次级 CPU 提前进入调度器并访问未初始化的全局状态。
// OWNER: boot CPU publishes completion of global initialization to secondary CPUs.
static INIT_READY: AtomicBool = AtomicBool::new(false);

fn kernel_main(context: entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());

    log::init();
    memory::init_allocator();
    platform::initialize(context.platform());
    platform::verify_firmware();
    cpu::initialize(platform::hardware_cpu_ids(), context.hardware_cpu());
    task::initialize_interrupt_state();
    info!(
        "logical CPU topology initialized: count={}, boot={:?}",
        cpu::count(),
        cpu::boot_id()
    );
    memory::init();
    let parameters = cmdline::parse(platform::kernel_command_line())
        .unwrap_or_else(|error| panic!("invalid kernel command line: {error:?}"));
    if let Some(level) = parameters.log_level {
        log::apply_console_loglevel(level);
    }
    info!(
        "kernel command line: {}",
        core::str::from_utf8(platform::kernel_command_line()).unwrap_or("<non-utf8>")
    );
    timer::init_rtc();
    let vfs = fs::init_vfs();
    // 启动顺序由证明 token 约束：每一步只接受前置步骤返回的 token，顺序错误无法编译。
    let scheduler = task::initialize(vfs);
    platform::initialize_devices();
    let mut disk_index = 0;
    while let Some(disk) = drivers::block_device(disk_index) {
        fs::publish_block_device(disk).expect("block device publication failed");
        disk_index += 1;
    }
    // ALSA、DRM 与 SPICE port 领域当前各只绑定首个已发现 adapter（card0/pcmC0D0p/唯一 named
    // port）；其余同类 adapter 已注册但不发布节点。
    if let Some(output) = drivers::pcm_output(0) {
        audio::init(output).expect("ALSA PCM initialization failed");
    }
    if let Some(display) = drivers::display_device(0) {
        drm::device::init(display).expect("primary DRM initialization failed");
    }
    input::init().expect("evdev input initialization failed");
    if let Some(port) = drivers::port_device(0) {
        virtio_port::init(port).expect("VirtIO port initialization failed");
    }
    let console = fs::init_tty(select_console(parameters.console.as_deref()))
        .expect("TTY initialization failed");
    socket::init();
    let root = mount_filesystems(scheduler, &parameters);
    task::spawn_init(
        scheduler,
        root,
        console,
        arch::trap::user_entry(),
        trap::trap_return,
        task::InitProgram {
            path: parameters.init.as_deref(),
            arguments: &parameters.init_arguments,
            environment: &parameters.init_environment,
        },
    );
    // Release 发布页表、设备、文件系统和首个任务；secondary 在进入任何共享子系统前消费它。
    INIT_READY.store(true, Ordering::Release);
    for target in cpu::possible().iter() {
        if target == cpu::boot_id() {
            continue;
        }
        let hardware = cpu::hardware_id(target);
        platform::start_cpu(hardware, arch::secondary_entry(), context.platform()).unwrap_or_else(
            |error| panic!("firmware failed to start CPU {:?}: {}", hardware, error),
        );
    }

    enter_scheduler()
}

fn mount_filesystems(
    scheduler: task::SchedulerReady,
    parameters: &cmdline::KernelParameters,
) -> fs::RootMounted {
    let environment = fs::MountEnvironment {
        threads: task::kernel_thread_support(scheduler),
        proc_source: Arc::try_new(task::KernelProcSource).expect("proc source allocation failed"),
        cpu_count: cpu::count(),
    };
    // 没有 `root=` 时以首块盘为根（Linux 由构建期 ROOT_DEV 给出缺省）。
    let default_root;
    let root = match &parameters.root {
        Some(root) => root.as_slice(),
        None => {
            let disk = drivers::block_device(0)
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
        if parameters.read_only_root {
            fs::MountFlags::from_bits(u64::from(fs::MountFlags::READ_ONLY))
        } else {
            fs::MountFlags::default()
        },
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
fn select_console(name: Option<&[u8]>) -> Arc<dyn drivers::console::ConsoleDevice> {
    let mut index = 0;
    while let Some(device) = drivers::console_device(index) {
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

fn kernel_secondary_main(context: entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());
    // Acquire 消费 boot CPU 在 INIT_READY 之前完成的全部全局初始化写入。
    while !INIT_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    platform::validate_boot_info(context.platform());
    KERNEL_SPACE.wait().lock().active();

    enter_scheduler()
}

fn init_local_arch(hardware_cpu: cpu::HardwareCpuId) {
    // 每个 CPU 都必须建立 architecture-local execution state；缺失会使该 CPU 无法运行用户上下文。
    arch::cpu::initialize_local_execution();
    let executing_hardware_id = cpu::executing_hardware_id();
    assert_eq!(
        hardware_cpu, executing_hardware_id,
        "firmware and architecture entry CPU identities disagree"
    );

    trap::init();
}

fn enter_scheduler() -> ! {
    timer::enable_timer_interrupt();
    // SAFETY: local trap state and platform interrupt controllers are initialized before the
    // architecture enables scheduler interrupt delivery for this CPU.
    unsafe { arch::interrupt::enable_scheduler_interrupts() };
    cpu::mark_online();
    if cpu::current_id() == cpu::boot_id() {
        // boot CPU 等待所有 platform target 完成本地初始化；缺失该屏障会把“start 已接受”误当成 online。
        while cpu::online() != cpu::possible() {
            core::hint::spin_loop();
        }
        info!(
            "all platform CPUs online: count={}, mask={:#x}",
            cpu::count(),
            cpu::online().native_word()
        );
    }
    // 每个 CPU 在发布 online 后只同步自己的共享 kernel translations；尚未 online 的 CPU
    // 不可作为 remote-fence target，已 online CPU 已在各自 activation 路径完成本地 fence。
    arch::mmu::flush_local();

    task::run_tasks();
}
