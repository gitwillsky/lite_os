#![no_std]
#![no_main]
#![feature(alloc_error_handler)]
#![feature(allocator_ext)]
#![deny(unsafe_op_in_unsafe_fn)]

//! LiteOS 微内核：只提供通用机制（线程、地址空间、内核对象、IPC、restricted mode、驱动原语）。
//! 本文件是唯一的装配点，只决定初始化顺序与 fail-stop 策略。

use core::sync::atomic::{AtomicBool, Ordering};

extern crate alloc;

mod arch;
mod config;
#[macro_use]
mod platform;
#[macro_use]
mod log;
mod boot;
mod bootstrap;
mod crash;
mod execution;
mod id;
mod iommu;
mod lang_item;
mod memory;
mod object;
mod random;
mod serial;
mod syscall;
mod thread;
mod vm;

/// 标记全局内核设施已完成初始化。
///
/// 次级 CPU 不能仅等待内核页表，因为页表会在根服务启动前发布；缺少此屏障会让次级 CPU
/// 提前进入调度器并访问未初始化的全局状态。
// OWNER: boot CPU 向次级 CPU 发布全局初始化完成。
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
    execution::barrier::initialize();
    crash::initialize();
    info!(
        "logical CPU topology initialized: count={}, boot={:?}",
        execution::cpu::count(),
        execution::cpu::boot_id()
    );
    // 崩溃记录区、initrd 与 DTB 都必须在 frame allocator 发布前排除：前者跨重启保留，后两者在根服务
    // 复制出 bootfs 与 DTB 之前不得被覆盖（固件可能把它们放在内核之后的可分配 RAM 中）。
    let allocator_end = crash::reserve_region(platform::physical_memory_end().into());
    let device_tree = platform::dtb_range();
    memory::init(
        allocator_end,
        [
            platform::initrd_range().map(|range| range.start.into()..range.end.into()),
            Some(device_tree.start.into()..device_tree.end.into()),
        ],
    );
    vm::initialize_kernel_space();
    // CPU 数与内核堆都已就绪：此后单页分配与释放先走每 CPU 缓存，不再每次争用全局 allocator 锁。
    memory::frame_allocator::enable_cpu_caches(execution::cpu::count());
    vm::initialize_zero_page();
    crash::recover();
    let parameters = boot::cmdline::parse(platform::kernel_command_line())
        .unwrap_or_else(|error| panic!("invalid kernel command line: {error:?}"));
    if let Some(level) = parameters.log_level {
        log::apply_console_loglevel(level);
    }
    crash::configure(parameters.panic_seconds);
    if parameters.stack_probe {
        vm::enable_kernel_stack_probe();
    }
    info!(
        "kernel command line: {}",
        core::str::from_utf8(platform::kernel_command_line()).unwrap_or("<non-utf8>")
    );
    execution::timer::init_rtc();
    random::initialize();
    thread::initialize();
    thread::install_user_entry(thread::UserEntryPoints {
        trap_handler: arch::trap::user_entry(),
        trap_return: execution::trap::trap_return,
    });
    platform::initialize_devices();
    log::initialize_readers();
    memory::frame_allocator::initialize_pressure_notifications(object::notify_memory_pressure);
    bootstrap::start_component_manager(platform::kernel_command_line());
    thread::Thread::create_kernel(vm::background_reclaim)
        .expect("background memory reclaimer creation failed")
        .start();

    // Release 发布页表、设备与根服务；次级 CPU 在进入任何共享子系统前消费它。
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

fn kernel_secondary_main(context: execution::entry::BootContext) -> ! {
    init_local_arch(context.hardware_cpu());
    // Acquire 消费 boot CPU 在 INIT_READY 之前完成的全部全局初始化写入。
    while !INIT_READY.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
    platform::validate_boot_info(context.platform());
    vm::KERNEL_SPACE.wait().lock().activate();

    enter_scheduler()
}

fn init_local_arch(hardware_cpu: execution::cpu::HardwareCpuId) {
    // 每个 CPU 都必须建立架构本地执行状态；缺失会使该 CPU 无法运行用户上下文。
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
    // SAFETY: 本地 trap 状态与平台中断控制器在架构开放调度中断前已初始化。
    unsafe { arch::interrupt::enable_scheduler_interrupts() };
    execution::cpu::mark_online();
    if execution::cpu::current_id() == execution::cpu::boot_id() {
        // boot CPU 等待全部平台 CPU 完成本地初始化；缺失该屏障会把“start 已接受”误当成 online。
        while execution::cpu::online() != execution::cpu::possible() {
            core::hint::spin_loop();
        }
        info!(
            "all platform CPUs online: count={}, mask={:#x}",
            execution::cpu::count(),
            execution::cpu::online().native_word()
        );
    }
    // 每个 CPU 在发布 online 后只同步自己的内核翻译；尚未 online 的 CPU 不是远端 fence 目标。
    arch::mmu::flush_local();

    thread::run();
}
