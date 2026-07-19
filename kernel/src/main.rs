#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(allocator_api)]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::memory::KERNEL_SPACE;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;

mod arch;
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

/// 标记全局内核设施已完成初始化。
///
/// 次级 CPU 不能仅等待内核页表，因为页表会在文件系统、驱动和首个用户任务
/// 就绪前发布；缺少此屏障会让次级 CPU 提前进入调度器并访问未初始化的全局状态。
// OWNER: boot CPU publishes completion of global initialization to secondary CPUs.
static INIT_READY: AtomicBool = AtomicBool::new(false);

fn kernel_main(context: entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());

    log::init();
    log::disable_module("kernel::task::loader");
    memory::init_allocator();
    platform::initialize(context.platform());
    platform::verify_firmware();
    cpu::initialize(platform::hardware_cpu_ids(), context.hardware_cpu());
    task::initialize_interrupt_state();
    debug!(
        "logical CPU topology initialized: count={}, boot={:?}",
        cpu::count(),
        cpu::boot_id()
    );
    memory::init();
    timer::init_rtc();
    fs::init_vfs();
    platform::initialize_devices();
    if let Some(display) = drivers::primary_display() {
        let (completion_read, completion_write) = task::create_notification_endpoints()
            .expect("DRM completion notification allocation failed");
        drm::device::init(display, completion_read, completion_write)
            .expect("primary DRM initialization failed");
    }
    input::init(task::create_notification_endpoints).expect("evdev input initialization failed");
    fs::init_pty(
        task::create_pipe_endpoints,
        task::create_notification_endpoints,
        task::hangup_terminal,
        task::publish_terminal_input_signals,
    )
    .expect("Unix98 PTY initialization failed");
    socket::init();
    mount_root_filesystem();
    task::init(
        arch::trap::user_entry(),
        trap::trap_return,
        Arc::try_new(PlatformConsole).expect("platform console allocation failed"),
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

fn mount_root_filesystem() {
    let device =
        drivers::block::get_primary_block_device().expect("boot requires one primary block device");
    let filesystem = fs::Ext2FileSystem::new(device).expect("invalid ext2 root filesystem");
    fs::vfs()
        .mount_root(b"root", filesystem)
        .expect("root filesystem mounted more than once");
    info!("ext2 root filesystem mounted at /");
    fs::vfs()
        .mount_at(b"/dev", b"devfs", fs::DevFileSystem::instance())
        .expect("failed to mount devfs at /dev");
    info!("devfs mounted at /dev");
    fs::vfs()
        .mount_at(
            b"/dev/pts",
            b"devpts",
            fs::DevPtsFileSystem::new().expect("failed to allocate devpts"),
        )
        .expect("failed to mount devpts at /dev/pts");
    info!("devpts mounted at /dev/pts");
    fs::vfs()
        .mount_at(
            b"/proc",
            b"proc",
            fs::ProcFileSystem::new(
                Arc::try_new(task::KernelProcSource).expect("proc source allocation failed"),
            )
            .expect("failed to allocate procfs"),
        )
        .expect("failed to mount procfs at /proc");
    info!("procfs mounted at /proc");
    fs::vfs()
        .mount_at(
            b"/sys",
            b"sysfs",
            fs::SysFileSystem::new(cpu::count()).expect("failed to allocate sysfs"),
        )
        .expect("failed to mount sysfs at /sys");
    info!("sysfs mounted at /sys");
}

struct PlatformConsole;

impl fs::Console for PlatformConsole {
    fn read(&self, bytes: &mut [u8]) -> Result<usize, fs::FileSystemError> {
        Ok(drivers::read_console(bytes))
    }

    fn input_ready(&self) -> bool {
        drivers::console_input_ready()
    }

    fn discard_input(&self) -> usize {
        drivers::discard_console_input()
    }

    fn write(&self, bytes: &[u8]) -> Result<usize, fs::FileSystemError> {
        for byte in bytes {
            platform::debug_console_write(*byte).map_err(|_| fs::FileSystemError::IoError)?;
        }
        Ok(bytes.len())
    }
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
