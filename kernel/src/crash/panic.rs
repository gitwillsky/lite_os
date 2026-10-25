//! panic 的唯一 owner：停住其余 CPU、保存现场、显示蓝屏，然后按策略停机或重启。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    fmt::{self, Write},
    panic::PanicInfo,
    sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering},
};

use spin::Once;

use super::{
    backtrace::{self, MAX_FRAMES},
    bluescreen::{self, Report},
    record::Recorder,
};
use crate::{
    cpu::{self, CpuSet},
    drivers::PanicFramebuffer,
    platform, timer,
};

/// 等待其余 CPU 响应停止 IPI 的上限。IRQ 屏蔽的 CPU 收不到 IPI，不能无限等待。
const STOP_TIMEOUT_US: u64 = 200_000;
/// 记录进 dump 的 kmsg 条数（环容量即上限）。
const KMSG_TAIL_RECORDS: usize = 128;
/// 没有 CPU 持有 panic 所有权。
const NO_OWNER: usize = usize::MAX;

/// 其余 CPU 在响应停止 IPI 时留下的执行位置。
struct CpuSnapshot {
    acked: AtomicBool,
    depth: AtomicUsize,
    frames: [AtomicUsize; MAX_FRAMES],
}

// OWNER: 本模块独占 panic 所有权：第一个 CAS 成功的 CPU 保存现场，其余（含本 CPU 的嵌套 panic）
// 只能停机。缺少它，两个 CPU 同时 panic 会交错写记录区与串口，嵌套 panic 会无限递归。
static OWNER: AtomicUsize = AtomicUsize::new(NO_OWNER);
// OWNER: panic 所有者向其余 CPU 发布的停止请求；IPI handler 读到它就采集自身位置并永久停机。
// 缺少它，IPI 只是普通唤醒，其余 CPU 会在全局不变量失效后继续运行。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
// OWNER: 每颗 logical CPU 一份停止快照，在 `initialize` 按已发布 topology 分配一次；panic 路径不能
// 分配，缺失时只能在转储里写“无快照”。
static SNAPSHOTS: Once<Box<[CpuSnapshot]>> = Once::new();
// OWNER: Linux `panic=` 语义：0 停在蓝屏，正数为秒数后重启，负数立即重启。由 boot CPU 在命令行解析后
// 写入一次；默认 0，缺失时坏掉的机器会在无人值守时反复重启而不留下可读现场。
static REBOOT_SECONDS: AtomicI32 = AtomicI32::new(0);
// OWNER: boot CPU 在显示设备装配后登记的 panic 帧缓冲；未登记时只输出串口。
static DISPLAY: Once<Arc<dyn PanicFramebuffer>> = Once::new();

/// 按 logical topology 分配停止快照；必须在 `cpu::initialize` 之后调用。
///
/// # Panics
///
/// 重复初始化或分配失败时 fail-stop。
pub(crate) fn initialize() {
    let mut snapshots = Vec::new();
    snapshots
        .try_reserve_exact(cpu::count())
        .expect("crash snapshot allocation failed");
    snapshots.extend((0..cpu::count()).map(|_| CpuSnapshot {
        acked: AtomicBool::new(false),
        depth: AtomicUsize::new(0),
        frames: core::array::from_fn(|_| AtomicUsize::new(0)),
    }));
    let mut installed = false;
    SNAPSHOTS.call_once(|| {
        installed = true;
        snapshots.into_boxed_slice()
    });
    assert!(installed, "crash snapshots initialized twice");
}

/// 应用 `panic=` 内核参数。
///
/// # Parameters
///
/// - `seconds`: Linux `panic=` 的值；`None` 保持默认（停在蓝屏）。
pub(crate) fn configure(seconds: Option<i32>) {
    if let Some(seconds) = seconds {
        REBOOT_SECONDS.store(seconds, Ordering::Relaxed);
    }
}

/// 登记 panic 时使用的帧缓冲；之后任何 panic 都会在其上绘制蓝屏。
pub(crate) fn register_display(display: Arc<dyn PanicFramebuffer>) {
    DISPLAY.call_once(|| display);
}

/// 核间中断处理中检查并响应停止请求。
///
/// 没有请求时立即返回；有请求时采集本 CPU 的回溯、发布 ack 并永久停机，不再返回。
#[inline(never)]
pub(crate) fn stop_if_requested() {
    if !STOP_REQUESTED.load(Ordering::Acquire) {
        return;
    }
    crate::arch::interrupt::disable_for_fail_stop();
    if let Some(snapshots) = SNAPSHOTS.get() {
        let slot = &snapshots[cpu::current_id().index()];
        let mut frames = [0usize; MAX_FRAMES];
        let depth = backtrace::capture(crate::arch::backtrace::frame_pointer(), &mut frames);
        for (slot, frame) in slot.frames.iter().zip(frames).take(depth) {
            slot.store(frame, Ordering::Relaxed);
        }
        slot.depth.store(depth, Ordering::Relaxed);
        slot.acked.store(true, Ordering::Release);
    }
    halt_forever();
}

fn halt_forever() -> ! {
    loop {
        crate::arch::interrupt::wait();
    }
}

/// 向其余在线 CPU 发停止 IPI 并有界等待 ack。
///
/// # Returns
///
/// 被请求停止的 CPU 集合；其中没有 ack 的是 IRQ 屏蔽或已卡死的 CPU。
fn stop_other_cpus() -> CpuSet {
    let (Some(snapshots), true) = (SNAPSHOTS.get(), cpu::is_initialized()) else {
        return CpuSet::default();
    };
    let mut targets = cpu::online();
    targets.remove(cpu::current_id());
    if targets.is_empty() {
        return targets;
    }
    STOP_REQUESTED.store(true, Ordering::Release);
    let _ = platform::send_ipi(targets);
    let deadline = timer::get_time_us().saturating_add(STOP_TIMEOUT_US);
    while timer::get_time_us() < deadline
        && !targets
            .iter()
            .all(|target| snapshots[target.index()].acked.load(Ordering::Acquire))
    {
        core::hint::spin_loop();
    }
    targets
}

/// 把 `frames` 逐行写成 `  #n 0x…`。
fn write_frames(out: &mut dyn fmt::Write, frames: impl Iterator<Item = usize>) {
    for (index, address) in frames.enumerate() {
        let _ = writeln!(out, "  #{index:<2} {address:#018x}");
    }
}

/// 同时写串口与记录区的输出端；串口永远可用，记录区缺失（panic 早于 memory::init）时只有串口。
struct Sink {
    recorder: Option<Recorder>,
}

impl fmt::Write for Sink {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        platform::console::panic_print_fmt(format_args!("{text}"));
        if let Some(recorder) = &mut self.recorder {
            let _ = recorder.write_str(text);
        }
        Ok(())
    }
}

impl Sink {
    fn commit(&mut self) {
        if let Some(recorder) = &mut self.recorder {
            recorder.commit();
        }
    }
}

/// kernel panic 的入口，由 `#[panic_handler]` 调用。
///
/// 1. 关本地中断并争夺 panic 所有权；失败者（其他 CPU 或本 CPU 嵌套 panic）直接停机。
/// 2. 停止其余 CPU，使 kmsg 环、记录区与帧缓冲不再被并发修改。
/// 3. 依次写入 panic 原因 + 回溯、各 CPU 位置、kmsg 尾部；每段写完即提交，后续步骤再 fault
///    也不会丢掉前面的现场。
/// 4. 在显示设备上绘制蓝屏。
/// 5. 按 `panic=` 策略停在蓝屏或重启；重启走 PSCI/SBI warm-reset 路径，RAM 中的记录随之保留。
pub(crate) fn handle(info: &PanicInfo) -> ! {
    crate::arch::interrupt::disable_for_fail_stop();
    let me = if cpu::is_initialized() {
        cpu::current_id().index()
    } else {
        0
    };
    if OWNER
        .compare_exchange(NO_OWNER, me, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        platform::console::panic_println_fmt(format_args!(
            "KERNEL PANIC: nested panic on CPU {me}"
        ));
        halt_forever();
    }

    let stopped = stop_other_cpus();
    let uptime_us = if cpu::is_initialized() {
        timer::get_time_us()
    } else {
        0
    };
    let mut frames = [0usize; MAX_FRAMES];
    let depth = backtrace::capture(crate::arch::backtrace::frame_pointer(), &mut frames);

    // SAFETY: 上面的 CAS 使本 CPU 成为唯一 panic 所有者。
    let mut sink = Sink {
        recorder: unsafe { Recorder::begin() },
    };
    let _ = writeln!(sink, "KERNEL PANIC: {}", info.message());
    if let Some(location) = info.location() {
        let _ = writeln!(
            sink,
            "  at {}:{}:{}",
            location.file(),
            location.line(),
            location.column()
        );
    }
    let _ = writeln!(
        sink,
        "  CPU {me}, uptime {}.{:06} s, LiteOS {}",
        uptime_us / 1_000_000,
        uptime_us % 1_000_000,
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(sink, "backtrace (resolve with addr2line -e kernel):");
    write_frames(&mut sink, frames[..depth].iter().copied());
    sink.commit();

    if let Some(snapshots) = SNAPSHOTS.get() {
        for target in stopped.iter() {
            let slot = &snapshots[target.index()];
            if slot.acked.load(Ordering::Acquire) {
                let _ = writeln!(sink, "CPU {} stopped, backtrace:", target.index());
                let depth = slot.depth.load(Ordering::Relaxed);
                write_frames(
                    &mut sink,
                    slot.frames[..depth]
                        .iter()
                        .map(|frame| frame.load(Ordering::Relaxed)),
                );
            } else {
                let _ = writeln!(
                    sink,
                    "CPU {} did not respond (interrupts masked or hung)",
                    target.index()
                );
            }
        }
    }
    sink.commit();

    let _ = writeln!(sink, "kernel log tail:");
    // SAFETY: 本 CPU 是 panic 所有者，其余 CPU 已停止或确认无响应。
    unsafe { crate::log::write_tail_for_fail_stop(&mut sink, KMSG_TAIL_RECORDS) };
    sink.commit();

    if let Some(display) = DISPLAY.get() {
        bluescreen::show(
            &**display,
            &Report {
                info,
                cpu: me,
                uptime_seconds: uptime_us / 1_000_000,
                frames: &frames[..depth],
                record_saved: sink.recorder.is_some(),
            },
        );
    }

    let seconds = REBOOT_SECONDS.load(Ordering::Relaxed);
    if seconds != 0 {
        if seconds > 0 && cpu::is_initialized() {
            let deadline = timer::get_time_us().saturating_add(seconds as u64 * 1_000_000);
            while timer::get_time_us() < deadline {
                core::hint::spin_loop();
            }
        }
        let _ = platform::reset_system(
            platform::ResetKind::ColdReboot,
            platform::ResetReason::SystemFailure,
        );
    }
    halt_forever()
}
