use alloc::sync::Arc;
use crate::drivers::hal::bus::{MmioBus, Bus, BusError};
use crate::board::RTCDevice;

// Goldfish RTC 寄存器偏移
const RTC_TIME_LOW: usize = 0x00;   // 纳秒时间低32位
const RTC_TIME_HIGH: usize = 0x04;  // 纳秒时间高32位
const RTC_ALARM_LOW: usize = 0x08;  // 闹钟时间低32位
const RTC_ALARM_HIGH: usize = 0x0c; // 闹钟时间高32位

/// Goldfish RTC 设备驱动
pub struct GoldfishRTC {
    bus: Arc<MmioBus>,
}

impl GoldfishRTC {
    /// 创建新的 Goldfish RTC 设备
    pub fn new(rtc_info: RTCDevice) -> Result<Self, BusError> {
        let bus = Arc::new(MmioBus::new(rtc_info.base_addr, rtc_info.size)?);
        
        Ok(Self { bus })
    }
    
    /// 读取当前的 Unix 时间戳（纳秒）
    pub fn read_time_ns(&self) -> Result<u64, BusError> {
        // 读取低32位和高32位
        let low = self.bus.read_u32(RTC_TIME_LOW)?;
        let high = self.bus.read_u32(RTC_TIME_HIGH)?;
        
        // 组合成64位纳秒时间戳
        Ok(((high as u64) << 32) | (low as u64))
    }
    
    /// 读取当前的 Unix 时间戳（秒）
    pub fn read_time_sec(&self) -> Result<u64, BusError> {
        let time_ns = self.read_time_ns()?;
        Ok(time_ns / 1_000_000_000)
    }
    
    /// 读取当前的 Unix 时间戳（微秒）
    pub fn read_time_us(&self) -> Result<u64, BusError> {
        let time_ns = self.read_time_ns()?;
        Ok(time_ns / 1_000)
    }
    
    /// 设置闹钟时间（纳秒）
    pub fn set_alarm_ns(&self, alarm_time: u64) -> Result<(), BusError> {
        let low = (alarm_time & 0xFFFFFFFF) as u32;
        let high = (alarm_time >> 32) as u32;
        
        self.bus.write_u32(RTC_ALARM_LOW, low)?;
        self.bus.write_u32(RTC_ALARM_HIGH, high)?;
        
        Ok(())
    }
    
    /// 获取设备基地址（用于调试）
    pub fn base_address(&self) -> usize {
        self.bus.base_address()
    }
    
    /// 检查设备是否可访问
    pub fn is_accessible(&self) -> bool {
        self.bus.is_accessible()
    }
}

impl core::fmt::Debug for GoldfishRTC {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoldfishRTC")
            .field("base_addr", &format_args!("{:#x}", self.bus.base_address()))
            .field("size", &self.bus.size())
            .field("accessible", &self.bus.is_accessible())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_rtc_time_conversion() {
        // 测试纳秒到秒的转换
        let ns = 1_609_459_200_000_000_000u64; // 2021-01-01 00:00:00 UTC in nanoseconds
        let sec = ns / 1_000_000_000;
        assert_eq!(sec, 1_609_459_200);
    }
}