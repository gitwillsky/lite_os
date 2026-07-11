use core::sync::atomic::{AtomicU64, Ordering};

use riscv::register;
use spin::Mutex;

use crate::{
    arch::{dtb, sbi},
    config,
    drivers::goldfish_rtc::GoldfishRTCDevice,
};

static TICK_INTERVAL_VALUE: AtomicU64 = AtomicU64::new(0);

const USEC_PER_SEC: u64 = 1000_000;
const NSEC_PER_SEC: u64 = 1000_000_000;

// 系统启动时的时间偏移，从 Goldfish RTC 获取真实时间
static BOOT_TIME_UNIX_SECONDS: AtomicU64 = AtomicU64::new(0);

// 全局 RTC 设备实例
static RTC_DEVICE: Mutex<Option<GoldfishRTCDevice>> = Mutex::new(None);

// 初始化 RTC 设备
fn init_rtc_device() -> Option<GoldfishRTCDevice> {
    let board_info = dtb::board_info();
    debug!("Checking for RTC device...");

    if let Some(rtc_info) = board_info.rtc_device {
        debug!(
            "Found RTC device at base address: {:#x}, size: {:#x}",
            rtc_info.base_addr, rtc_info.size
        );

        // 检查地址是否合理
        if rtc_info.base_addr == 0 {
            warn!("Invalid RTC base address: 0x0");
            return None;
        }

        // 使用 MmioBus 创建 RTC 设备
        match GoldfishRTCDevice::new(rtc_info) {
            Ok(rtc) => {
                debug!("Successfully initialized Goldfish RTC device");
                Some(rtc)
            }
            Err(err) => {
                warn!("Failed to initialize Goldfish RTC: {:?}", err);
                None
            }
        }
    } else {
        warn!("Goldfish RTC device not found in device tree");
        None
    }
}

// 读取 Goldfish RTC 获取真实的 Unix 时间戳（纳秒）
fn read_goldfish_rtc_ns() -> Option<u64> {
    let rtc_guard = RTC_DEVICE.lock();
    if let Some(rtc) = rtc_guard.as_ref() {
        match rtc.read_time_ns() {
            Ok(time_ns) => {
                debug!("Successfully read RTC time: {} ns", time_ns);
                Some(time_ns)
            }
            Err(err) => {
                warn!("Failed to read RTC time: {:?}", err);
                None
            }
        }
    } else {
        debug!("RTC device not initialized");
        None
    }
}

// 获取真实的 Unix 时间戳（秒）
pub fn get_unix_timestamp() -> u64 {
    let boot_time = BOOT_TIME_UNIX_SECONDS.load(Ordering::Acquire);
    if boot_time == 0 {
        // 如果还没有初始化，直接从 RTC 读取当前时间
        if let Some(rtc_ns) = read_goldfish_rtc_ns() {
            rtc_ns / NSEC_PER_SEC
        } else {
            // RTC 不可用，返回基于系统运行时间的估计值
            warn!("RTC not available, using boot time estimate");
            1704067200 + (get_time_ns() / NSEC_PER_SEC) // 2024-01-01 + uptime
        }
    } else {
        // 使用启动时间偏移 + 系统运行时间
        boot_time + (get_time_ns() / NSEC_PER_SEC)
    }
}

pub fn get_time_us() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = dtb::board_info().time_base_freq;
    // 使用128位运算避免溢出
    ((current_mtime as u128 * USEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

pub fn get_time_ns() -> u64 {
    let current_mtime = register::time::read64();
    let time_base_freq = dtb::board_info().time_base_freq;
    // 使用128位运算避免溢出
    ((current_mtime as u128 * NSEC_PER_SEC as u128) / time_base_freq as u128) as u64
}

#[inline(always)]
pub fn set_next_timer_interrupt() {
    let current_mtime = register::time::read64();
    // 避免在 debug 构建下触发算术溢出 panic：采用 wrapping 加法
    let interval = TICK_INTERVAL_VALUE.load(Ordering::Acquire);
    assert!(
        interval != 0,
        "timer interval used before per-hart initialization"
    );
    let next_mtime = current_mtime.wrapping_add(interval);

    sbi::set_timer(next_mtime).expect("SBI TIME set_timer failed");
}

pub fn enable_timer_interrupt() {
    let time_base_freq = dtb::board_info().time_base_freq;

    // 1. DTB 的 timebase-frequency 是平台契约，零值不能被静默改写为伪造频率。
    assert!(
        time_base_freq != 0,
        "DTB timebase-frequency must be non-zero"
    );
    let ticks_per_sec = config::TICKS_PER_SEC as u64;
    assert!(ticks_per_sec != 0, "TICKS_PER_SEC must be non-zero");
    let interval = time_base_freq / ticks_per_sec;
    assert!(interval != 0, "timer tick rate exceeds timebase frequency");

    // 2. Release 发布 interval，set_next_timer_interrupt 的 Acquire 保证不会读到未初始化值。
    TICK_INTERVAL_VALUE.store(interval, Ordering::Release);
    unsafe {
        // 3. 每个 hart 独立启用 STIE，并在打开全局 SIE 前写入首个 deadline。
        register::sie::set_stimer();
    }

    set_next_timer_interrupt();
}

pub fn init_rtc() {
    // 初始化 RTC 设备
    if let Some(rtc) = init_rtc_device() {
        *RTC_DEVICE.lock() = Some(rtc);
        debug!("RTC device initialized successfully");
    } else {
        warn!("Failed to initialize RTC device");
    }

    // 从 Goldfish RTC 获取真实的启动时间
    if let Some(current_unix_ns) = read_goldfish_rtc_ns() {
        // RTC 给出“现在”，而 BOOT_TIME 保存 epoch offset；若直接保存现在并在读取时
        // 再加 uptime，会把启动前已经流逝的 uptime 重复计算一次。
        let boot_time =
            (current_unix_ns / NSEC_PER_SEC).saturating_sub(get_time_ns() / NSEC_PER_SEC);
        BOOT_TIME_UNIX_SECONDS.store(boot_time, Ordering::Release);
        debug!(
            "Boot time set to Unix timestamp: {} (from Goldfish RTC)",
            boot_time
        );
    } else {
        warn!("Goldfish RTC not available, using default boot time");
        BOOT_TIME_UNIX_SECONDS.store(1704067200, Ordering::Release);
    }
    debug!("timer initialized with real-time clock");
}
