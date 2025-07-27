use core::sync::atomic::{AtomicU64, Ordering};

use spin::Mutex;

use crate::{
    board,
    drivers::GoldfishRTC,
    timer::{get_time_ns, get_time_us},
};

// Goldfish RTC 寄存器偏移
const RTC_TIME_LOW: usize = 0x00; // 纳秒时间低32位
const RTC_TIME_HIGH: usize = 0x04; // 纳秒时间高32位
const MSEC_PER_SEC: u64 = 1000;
const USEC_PER_SEC: u64 = 1000_000;
const NSEC_PER_SEC: u64 = 1000_000_000;

// Unix 纪元时间常量 (1970-01-01 00:00:00 UTC)
const UNIX_EPOCH_SECONDS: u64 = 0;

// 系统启动时的时间偏移，从 Goldfish RTC 获取真实时间
static BOOT_TIME_UNIX_SECONDS: AtomicU64 = AtomicU64::new(0);

// 全局 RTC 设备实例
static RTC_DEVICE: Mutex<Option<GoldfishRTC>> = Mutex::new(None);

// 初始化 RTC 设备
pub fn init_rtc_device() {
    let board_info = board::board_info();
    debug!("Checking for RTC device...");

    if let Some(rtc_info) = board_info.rtc_device {
        debug!(
            "Found RTC device at base address: {:#x}, size: {:#x}",
            rtc_info.base_addr, rtc_info.size
        );

        // 检查地址是否合理
        if rtc_info.base_addr == 0 {
            panic!("Invalid RTC base address: 0x0");
        }

        // 使用 MmioBus 创建 RTC 设备
        match GoldfishRTC::new(rtc_info) {
            Ok(rtc) => {
                debug!("Successfully initialized Goldfish RTC device");
                *RTC_DEVICE.lock() = Some(rtc);
            }
            Err(err) => {
                panic!("Failed to initialize Goldfish RTC: {:?}", err);
            }
        }
    } else {
        panic!("Goldfish RTC device not found in device tree");
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
    let boot_time = BOOT_TIME_UNIX_SECONDS.load(Ordering::Relaxed);
    if boot_time == 0 {
        // 如果还没有初始化，直接从 RTC 读取当前时间
        if let Some(rtc_ns) = read_goldfish_rtc_ns() {
            rtc_ns / NSEC_PER_SEC
        } else {
            panic!("RTC not available");
        }
    } else {
        // 使用启动时间偏移 + 系统运行时间
        boot_time + (get_time_ns() / NSEC_PER_SEC)
    }
}

// 获取真实的 Unix 时间戳（微秒）
pub fn get_unix_timestamp_us() -> u64 {
    let boot_time = BOOT_TIME_UNIX_SECONDS.load(Ordering::Relaxed);
    if boot_time == 0 {
        if let Some(rtc_ns) = read_goldfish_rtc_ns() {
            rtc_ns / 1000
        } else {
            panic!("RTC not available");
        }
    } else {
        boot_time * USEC_PER_SEC + get_time_us()
    }
}
