use crate::arch::dtb::RTCDevice;

const RTC_TIME_LOW: usize = 0x00;
const RTC_TIME_HIGH: usize = 0x04;

/// Goldfish RTC 初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtcError {
    InvalidRange,
}

/// @description 从 Goldfish RTC MMIO 读取 realtime 纳秒值。
pub(crate) struct GoldfishRTCDevice {
    base_addr: usize,
}

impl GoldfishRTCDevice {
    /// 根据 DTB 描述创建 RTC 实例。
    ///
    /// # Parameters
    ///
    /// - `rtc`: DTB 中的 MMIO 基址和区间长度。
    ///
    /// # Returns
    ///
    /// 区间覆盖时间寄存器时返回 RTC 实例。
    ///
    /// # Errors
    ///
    /// 基址为零、区间不足 8 字节或地址溢出时返回 `InvalidRange`。
    pub(crate) fn new(rtc: RTCDevice) -> Result<Self, RtcError> {
        if rtc.base_addr == 0
            || rtc.size < RTC_TIME_HIGH + core::mem::size_of::<u32>()
            || rtc.base_addr.checked_add(rtc.size).is_none()
        {
            return Err(RtcError::InvalidRange);
        }
        Ok(Self {
            base_addr: rtc.base_addr,
        })
    }

    /// 读取 Unix epoch realtime 纳秒值。
    ///
    /// # Returns
    ///
    /// Goldfish RTC 高低 32 位寄存器组成的纳秒值。
    pub(crate) fn read_time_ns(&self) -> Result<u64, RtcError> {
        // SAFETY: `new` 已验证 DTB MMIO 区间覆盖两个 32 位寄存器；
        // 内核地址空间按设备页映射该区间，MMIO 读取必须使用 volatile。
        let low =
            unsafe { core::ptr::read_volatile((self.base_addr + RTC_TIME_LOW) as *const u32) };
        let high =
            unsafe { core::ptr::read_volatile((self.base_addr + RTC_TIME_HIGH) as *const u32) };
        Ok(((high as u64) << 32) | low as u64)
    }
}
