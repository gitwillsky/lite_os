use alloc::sync::Arc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use alloc::format;
use alloc::vec;

use crate::drivers::hal::{
    Device, DeviceType, DeviceState, DeviceError, 
    Bus, InterruptVector, InterruptHandler,
    bus::{MmioBus, BusError},
    resource::{Resource, ResourceManager},
    GenericDevice,
    device::DeviceDriver,
};
use crate::board::RTCDevice;

// Goldfish RTC 寄存器偏移
const RTC_TIME_LOW: usize = 0x00;   // 纳秒时间低32位
const RTC_TIME_HIGH: usize = 0x04;  // 纳秒时间高32位
const RTC_ALARM_LOW: usize = 0x08;  // 闹钟时间低32位
const RTC_ALARM_HIGH: usize = 0x0c; // 闹钟时间高32位

/// Goldfish RTC 设备驱动 - 现在实现HAL Device trait
pub struct GoldfishRTCDevice {
    inner: GenericDevice,
    last_time: spin::Mutex<u64>, // 缓存上次读取的时间
}

impl GoldfishRTCDevice {
    /// 创建新的 Goldfish RTC 设备
    pub fn new(rtc_info: RTCDevice) -> Result<Self, DeviceError> {
        let bus = Arc::new(MmioBus::new(rtc_info.base_addr, rtc_info.size)
            .map_err(DeviceError::from)?);

        // 创建资源列表
        let memory_resource = Resource::Memory(crate::drivers::hal::resource::MemoryRange {
            start: rtc_info.base_addr,
            size: rtc_info.size,
            cached: false,      // Uncached for MMIO
            writable: true,     // RTC is writable
            executable: false,  // Not executable
        });

        let inner = GenericDevice::new(
            DeviceType::Console, // RTC可以归类为Console类型设备
            0x1234, // Goldfish RTC vendor ID
            0x5678, // Goldfish RTC device ID 
            "Goldfish RTC".to_string(),
            "Goldfish RTC Driver".to_string(),
            bus,
        ).with_resources(vec![memory_resource]);

        Ok(Self {
            inner,
            last_time: spin::Mutex::new(0),
        })
    }

    /// 读取当前的 Unix 时间戳（纳秒）
    pub fn read_time_ns(&self) -> Result<u64, DeviceError> {
        let bus = self.inner.bus();
        
        // 读取低32位和高32位
        let low = bus.read_u32(RTC_TIME_LOW).map_err(DeviceError::from)?;
        let high = bus.read_u32(RTC_TIME_HIGH).map_err(DeviceError::from)?;

        // 组合成64位纳秒时间戳
        let time_ns = ((high as u64) << 32) | (low as u64);
        
        // 缓存时间
        *self.last_time.lock() = time_ns;
        
        Ok(time_ns)
    }

    /// 读取当前的 Unix 时间戳（秒）
    pub fn read_time_sec(&self) -> Result<u64, DeviceError> {
        let time_ns = self.read_time_ns()?;
        Ok(time_ns / 1_000_000_000)
    }

    /// 读取当前的 Unix 时间戳（微秒）
    pub fn read_time_us(&self) -> Result<u64, DeviceError> {
        let time_ns = self.read_time_ns()?;
        Ok(time_ns / 1_000)
    }

    /// 读取当前的 Unix 时间戳（毫秒）
    pub fn read_time_ms(&self) -> Result<u64, DeviceError> {
        let time_ns = self.read_time_ns()?;
        Ok(time_ns / 1_000_000)
    }

    /// 设置闹钟时间（纳秒）
    pub fn set_alarm_ns(&self, alarm_time: u64) -> Result<(), DeviceError> {
        let bus = self.inner.bus();
        
        let low = (alarm_time & 0xFFFFFFFF) as u32;
        let high = (alarm_time >> 32) as u32;

        bus.write_u32(RTC_ALARM_LOW, low).map_err(DeviceError::from)?;
        bus.write_u32(RTC_ALARM_HIGH, high).map_err(DeviceError::from)?;

        debug!("[GoldfishRTC] Alarm set to {} ns", alarm_time);
        Ok(())
    }

    /// 设置闹钟时间（秒）
    pub fn set_alarm_sec(&self, alarm_time: u64) -> Result<(), DeviceError> {
        self.set_alarm_ns(alarm_time * 1_000_000_000)
    }

    /// 获取缓存的时间（避免频繁硬件访问）
    pub fn cached_time_ns(&self) -> u64 {
        *self.last_time.lock()
    }

    /// 获取设备基地址（用于调试）
    pub fn base_address(&self) -> usize {
        self.inner.bus().base_address()
    }

    /// 检查设备是否可访问
    pub fn is_accessible(&self) -> bool {
        self.inner.bus().is_accessible()
    }

    /// 自检设备功能
    pub fn self_test(&self) -> Result<bool, DeviceError> {
        // 读取时间两次，确保时间在递增
        let time1 = self.read_time_ns()?;
        
        // 短暂延迟
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
        
        let time2 = self.read_time_ns()?;
        
        // 时间应该递增（允许一些误差）
        let result = time2 >= time1;
        
        if result {
            info!("[GoldfishRTC] Self-test passed - time progression: {} -> {} ns", time1, time2);
        } else {
            warn!("[GoldfishRTC] Self-test failed - time regression: {} -> {} ns", time1, time2);
        }
        
        Ok(result)
    }
}

impl Device for GoldfishRTCDevice {
    fn device_type(&self) -> DeviceType {
        self.inner.device_type()
    }

    fn device_id(&self) -> u32 {
        self.inner.device_id()
    }

    fn vendor_id(&self) -> u32 {
        self.inner.vendor_id()
    }

    fn device_name(&self) -> String {
        self.inner.device_name()
    }

    fn driver_name(&self) -> String {
        self.inner.driver_name()
    }

    fn state(&self) -> DeviceState {
        self.inner.state()
    }

    fn probe(&mut self) -> Result<bool, DeviceError> {
        debug!("[GoldfishRTC] Probing device");
        
        // 检查设备是否可访问
        if !self.is_accessible() {
            return Ok(false);
        }
        
        // 尝试读取时间验证设备功能
        match self.read_time_ns() {
            Ok(time) => {
                debug!("[GoldfishRTC] Device probe successful, current time: {} ns", time);
                Ok(true)
            }
            Err(_) => {
                debug!("[GoldfishRTC] Device probe failed");
                Ok(false)
            }
        }
    }

    fn initialize(&mut self) -> Result<(), DeviceError> {
        info!("[GoldfishRTC] Initializing device at {:#x}", self.base_address());
        
        // 执行基础初始化
        self.inner.initialize()?;
        
        // 执行设备自检
        if self.self_test()? {
            info!("[GoldfishRTC] Device initialized and self-test passed");
            Ok(())
        } else {
            error!("[GoldfishRTC] Device initialization failed - self-test failed");
            Err(DeviceError::InitializationFailed)
        }
    }

    fn reset(&mut self) -> Result<(), DeviceError> {
        debug!("[GoldfishRTC] Resetting device");
        self.inner.reset()
    }

    fn shutdown(&mut self) -> Result<(), DeviceError> {
        info!("[GoldfishRTC] Shutting down device");
        self.inner.shutdown()
    }

    fn remove(&mut self) -> Result<(), DeviceError> {
        info!("[GoldfishRTC] Removing device");
        self.inner.remove()
    }

    fn suspend(&mut self) -> Result<(), DeviceError> {
        debug!("[GoldfishRTC] Suspending device");
        self.inner.suspend()
    }

    fn resume(&mut self) -> Result<(), DeviceError> {
        debug!("[GoldfishRTC] Resuming device");
        self.inner.resume()
    }

    fn bus(&self) -> Arc<dyn Bus> {
        self.inner.bus()
    }

    fn resources(&self) -> Vec<Resource> {
        self.inner.resources()
    }

    fn request_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        self.inner.request_resources(resource_manager)
    }

    fn release_resources(&mut self, resource_manager: &mut dyn ResourceManager) -> Result<(), DeviceError> {
        self.inner.release_resources(resource_manager)
    }

    fn supports_interrupt(&self) -> bool {
        false // RTC通常不使用中断
    }

    fn interrupt_vectors(&self) -> Vec<InterruptVector> {
        Vec::new()
    }

    fn set_interrupt_handler(&mut self, vector: InterruptVector, handler: Arc<dyn InterruptHandler>) -> Result<(), DeviceError> {
        self.inner.set_interrupt_handler(vector, handler)
    }

    fn supports_power_management(&self) -> bool {
        true
    }

    fn power_manager(&self) -> Option<Arc<dyn crate::drivers::hal::power::PowerManagement>> {
        self.inner.power_manager()
    }

    fn supports_hotplug(&self) -> bool {
        false // RTC设备通常不支持热插拔
    }

    fn get_property(&self, name: &str) -> Option<String> {
        match name {
            "base_address" => Some(format!("{:#x}", self.base_address())),
            "accessible" => Some(self.is_accessible().to_string()),
            "cached_time_ns" => Some(self.cached_time_ns().to_string()),
            _ => self.inner.get_property(name),
        }
    }

    fn set_property(&mut self, name: &str, value: &str) -> Result<(), DeviceError> {
        match name {
            "alarm_ns" => {
                if let Ok(alarm_time) = value.parse::<u64>() {
                    self.set_alarm_ns(alarm_time)
                } else {
                    Err(DeviceError::ConfigurationError)
                }
            }
            "alarm_sec" => {
                if let Ok(alarm_time) = value.parse::<u64>() {
                    self.set_alarm_sec(alarm_time)
                } else {
                    Err(DeviceError::ConfigurationError)
                }
            }
            _ => self.inner.set_property(name, value),
        }
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

impl core::fmt::Debug for GoldfishRTCDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("GoldfishRTCDevice")
            .field("device_name", &self.device_name())
            .field("base_addr", &format_args!("{:#x}", self.base_address()))
            .field("state", &self.state())
            .field("accessible", &self.is_accessible())
            .field("cached_time_ns", &self.cached_time_ns())
            .finish()
    }
}

/// RTC驱动程序
pub struct GoldfishRTCDriver {
    name: &'static str,
    version: &'static str,
}

impl GoldfishRTCDriver {
    pub fn new() -> Self {
        Self {
            name: "Goldfish RTC Driver",
            version: "1.0.0",
        }
    }
}

impl DeviceDriver for GoldfishRTCDriver {
    fn name(&self) -> &str {
        self.name
    }

    fn version(&self) -> &str {
        self.version
    }

    fn compatible_devices(&self) -> Vec<(u32, u32)> {
        vec![(0x1234, 0x5678)] // Goldfish RTC
    }

    fn probe(&self, device: &mut dyn Device) -> Result<bool, DeviceError> {
        // 检查设备类型和厂商ID
        Ok(device.device_type() == DeviceType::Console && 
           device.vendor_id() == 0x1234 && 
           device.device_id() == 0x5678)
    }

    fn bind(&self, device: &mut dyn Device) -> Result<(), DeviceError> {
        info!("[GoldfishRTCDriver] Binding to RTC device: {}", device.device_name());
        device.initialize()?;
        Ok(())
    }

    fn unbind(&self, device: &mut dyn Device) -> Result<(), DeviceError> {
        info!("[GoldfishRTCDriver] Unbinding from RTC device: {}", device.device_name());
        device.shutdown()?;
        Ok(())
    }

    fn supports_hotplug(&self) -> bool {
        false
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

    #[test]
    fn test_alarm_time_conversion() {
        // 测试秒到纳秒的转换
        let sec = 1_609_459_200u64; // 2021-01-01 00:00:00 UTC
        let ns = sec * 1_000_000_000;
        assert_eq!(ns, 1_609_459_200_000_000_000);
    }
}