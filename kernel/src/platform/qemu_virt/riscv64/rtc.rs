const RTC_TIME_LOW: usize = 0x00;
const RTC_TIME_HIGH: usize = 0x04;

/// Goldfish RTC 初始化错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RtcError {
    InvalidRange,
}

/// 从 Goldfish RTC MMIO 读取 realtime 纳秒值。
pub(crate) struct GoldfishRTCDevice {
    bus: crate::hal::MmioBus,
}

impl GoldfishRTCDevice {
    /// 根据 DTB 描述创建 RTC 实例。
    ///
    /// # Parameters
    ///
    /// - `base_addr`: 已由 platform discovery 验证来源的 MMIO 基址。
    /// - `size`: MMIO 区间长度。
    ///
    /// # Returns
    ///
    /// 区间覆盖时间寄存器时返回 RTC 实例。
    ///
    /// # Errors
    ///
    /// 基址为零、区间不足 8 字节或地址溢出时返回 `InvalidRange`。
    pub(crate) fn new(base_addr: usize, size: usize) -> Result<Self, RtcError> {
        if size < RTC_TIME_HIGH + core::mem::size_of::<u32>() {
            return Err(RtcError::InvalidRange);
        }
        let bus = crate::hal::MmioBus::new(base_addr, size).map_err(|_| RtcError::InvalidRange)?;
        Ok(Self { bus })
    }

    /// 读取 Unix epoch realtime 纳秒值。
    ///
    /// # Returns
    ///
    /// Goldfish RTC 高低 32 位寄存器组成的纳秒值。
    pub(crate) fn read_time_ns(&self) -> Result<u64, RtcError> {
        // 低字读取会锁存高字，先低后高。
        let low = self
            .bus
            .read_u32(RTC_TIME_LOW)
            .map_err(|_| RtcError::InvalidRange)?;
        let high = self
            .bus
            .read_u32(RTC_TIME_HIGH)
            .map_err(|_| RtcError::InvalidRange)?;
        Ok(((high as u64) << 32) | low as u64)
    }
}
