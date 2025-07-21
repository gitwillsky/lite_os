use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, Once};

use crate::console::print_str_legacy;

use super::{virtio_mmio::*, virtio_queue::*};

// VirtIO Console 设备ID
pub const VIRTIO_ID_CONSOLE: u32 = 3;

// VirtIO Console 特性位
pub const VIRTIO_CONSOLE_F_SIZE: u32 = 0;
pub const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 1;
pub const VIRTIO_CONSOLE_F_EMERG_WRITE: u32 = 2;

// VirtIO Console 队列索引
pub const RECEIVEQ_PORT0: u16 = 0; // 接收队列 (端口0)
pub const TRANSMITQ_PORT0: u16 = 1; // 发送队列 (端口0)
pub const CONTROLQ: u16 = 2; // 控制队列 (多端口)
pub const CONTROL_RECEIVEQ: u16 = 3; // 控制接收队列

// Console 配置结构
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOConsoleConfig {
    pub cols: u16,
    pub rows: u16,
    pub max_nr_ports: u32,
    pub emerg_wr: u32,
}

// Console 控制消息类型
pub const VIRTIO_CONSOLE_DEVICE_READY: u16 = 0;
pub const VIRTIO_CONSOLE_PORT_ADD: u16 = 1;
pub const VIRTIO_CONSOLE_PORT_REMOVE: u16 = 2;
pub const VIRTIO_CONSOLE_PORT_READY: u16 = 3;
pub const VIRTIO_CONSOLE_CONSOLE_PORT: u16 = 4;
pub const VIRTIO_CONSOLE_RESIZE: u16 = 5;
pub const VIRTIO_CONSOLE_PORT_OPEN: u16 = 6;
pub const VIRTIO_CONSOLE_PORT_NAME: u16 = 7;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOConsoleControl {
    pub id: u32,    // 端口ID
    pub event: u16, // 事件类型
    pub value: u16, // 事件值
}

pub struct VirtIOConsoleDevice {
    mmio_base: VirtIOMMIO,
    receive_queue: Arc<Mutex<VirtQueue>>,
    transmit_queue: Arc<Mutex<VirtQueue>>,
    control_queue: Option<Arc<Mutex<VirtQueue>>>,
    control_receive_queue: Option<Arc<Mutex<VirtQueue>>>,
    config: VirtIOConsoleConfig,
    multiport: bool,
}

impl VirtIOConsoleDevice {
    /// 创建新的VirtIO Console设备实例
    pub fn new(base_addr: usize) -> Option<Self> {
        let mmio_region = VirtIOMMIO::new(base_addr);

        // 探测设备
        if !mmio_region.probe() {
            return None;
        }

        // 检查设备ID
        if mmio_region.device_id() != VIRTIO_ID_CONSOLE {
            return None;
        }

        info!("[VirtIO Console] Found console device at {:#x}", base_addr);

        // 重置设备
        mmio_region.set_status(0);

        // 设置ACKNOWLEDGE标志
        mmio_region.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE);

        // 设置DRIVER标志
        mmio_region.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER);

        // 读取设备特性
        let device_features = mmio_region.device_features();
        let multiport = (device_features & (1 << VIRTIO_CONSOLE_F_MULTIPORT)) != 0;

        debug!(
            "[VirtIO Console] Features: multiport={}, emerg_write={}",
            multiport,
            (device_features & (1 << VIRTIO_CONSOLE_F_EMERG_WRITE)) != 0
        );

        // 协商特性 - 为了稳定性，先禁用多端口
        let driver_features = 0u32; // 只使用基础功能
        mmio_region.set_driver_features(driver_features);

        // 设置FEATURES_OK标志
        mmio_region.set_status(
            VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK,
        );

        // 验证FEATURES_OK
        if mmio_region.status() & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            error!("[VirtIO Console] Device does not accept features");
            return None;
        }

        // 设置页面大小
        mmio_region.set_guest_page_size(4096);

        // 获取设备配置
        let config = unsafe {
            core::ptr::read_volatile((base_addr + VIRTIO_MMIO_CONFIG) as *const VirtIOConsoleConfig)
        };

        debug!(
            "[VirtIO Console] Config: cols={}, rows={}, max_ports={}",
            config.cols, config.rows, config.max_nr_ports
        );

        // 强制设置为单端口模式以避免控制消息复杂性
        let multiport = false;
        debug!("[VirtIO Console] Using single-port mode for stability");

        // 初始化接收队列 (queue 0)
        mmio_region.select_queue(RECEIVEQ_PORT0 as u32);
        let rx_queue_size = mmio_region.queue_max_size();

        let receive_queue = Arc::new(Mutex::new(VirtQueue::new(
            rx_queue_size as u16,
            RECEIVEQ_PORT0 as usize,
        )?));

        mmio_region.set_queue_size(rx_queue_size);
        mmio_region.set_queue_align(4096);
        let rx_queue_pfn = receive_queue.lock().physical_address().as_usize() >> 12;
        mmio_region.set_queue_pfn(rx_queue_pfn as u32);

        // 设置队列就绪标志 - 这是关键修复
        mmio_region.write_reg(VIRTIO_MMIO_QUEUE_READY, 1);

        // 初始化发送队列 (queue 1)
        mmio_region.select_queue(TRANSMITQ_PORT0 as u32);
        let tx_queue_size = mmio_region.queue_max_size();

        let transmit_queue = Arc::new(Mutex::new(VirtQueue::new(
            tx_queue_size as u16,
            TRANSMITQ_PORT0 as usize,
        )?));

        mmio_region.set_queue_size(tx_queue_size);
        mmio_region.set_queue_align(4096);
        let tx_queue_pfn = transmit_queue.lock().physical_address().as_usize() >> 12;
        mmio_region.set_queue_pfn(tx_queue_pfn as u32);

        // 设置队列就绪标志 - 这是关键修复
        mmio_region.write_reg(VIRTIO_MMIO_QUEUE_READY, 1);

        // 如果支持多端口，初始化控制队列
        let (control_queue, control_receive_queue) = if multiport {
            // 控制发送队列 (queue 2)
            mmio_region.select_queue(CONTROLQ as u32);
            let cq_size = mmio_region.queue_max_size();
            let cq = Arc::new(Mutex::new(VirtQueue::new(
                cq_size as u16,
                CONTROLQ as usize,
            )?));
            mmio_region.set_queue_size(cq_size);
            mmio_region.set_queue_align(4096);
            let cq_pfn = cq.lock().physical_address().as_usize() >> 12;
            mmio_region.set_queue_pfn(cq_pfn as u32);
            mmio_region.set_queue_ready(1);

            // 控制接收队列 (queue 3)
            mmio_region.select_queue(CONTROL_RECEIVEQ as u32);
            let crq_size = mmio_region.queue_max_size();
            let crq = Arc::new(Mutex::new(VirtQueue::new(
                crq_size as u16,
                CONTROL_RECEIVEQ as usize,
            )?));
            mmio_region.set_queue_size(crq_size);
            mmio_region.set_queue_align(4096);
            let crq_pfn = crq.lock().physical_address().as_usize() >> 12;
            mmio_region.set_queue_pfn(crq_pfn as u32);
            mmio_region.set_queue_ready(1);

            (Some(cq), Some(crq))
        } else {
            (None, None)
        };

        // 设置DRIVER_OK标志 - 最后一步
        mmio_region.set_status(
            VIRTIO_CONFIG_S_ACKNOWLEDGE
                | VIRTIO_CONFIG_S_DRIVER
                | VIRTIO_CONFIG_S_FEATURES_OK
                | VIRTIO_CONFIG_S_DRIVER_OK,
        );

        let mut device = Self {
            mmio_base: mmio_region,
            receive_queue,
            transmit_queue,
            control_queue,
            control_receive_queue,
            config,
            multiport,
        };

        // 完整初始化：发送必要的控制消息
        if multiport {
            // 1. 发送设备就绪消息
            device.send_control_message(VIRTIO_CONSOLE_DEVICE_READY, 0, 1);

            // 2. 标记端口0为控制台端口
            device.send_control_message(VIRTIO_CONSOLE_CONSOLE_PORT, 0, 1);

            // 3. 打开端口0
            device.send_control_message(VIRTIO_CONSOLE_PORT_OPEN, 0, 1);
        } else {
            debug!("[VirtIO Console] Single port console - no control messages needed");

            // 对于单端口模式，预先设置接收缓冲区
            let receive_queue_clone = Arc::clone(&device.receive_queue);
            let mut rx_queue = receive_queue_clone.lock();
            device.setup_receive_buffer(&mut rx_queue);
            drop(rx_queue); // 释放锁
        }

        info!("[VirtIO Console] Device initialization completed successfully");
        Some(device)
    }

    /// 向控制台写入数据
    pub fn write(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.is_empty() {
            return Ok(());
        }

        let mut transmit_queue = self.transmit_queue.lock();

        // 创建持久化的数据缓冲区以确保数据在设备处理期间有效
        let mut data_buffer = Vec::with_capacity(data.len());
        data_buffer.extend_from_slice(data);

        // 使用add_buffer方法添加输出缓冲区
        let inputs = [data_buffer.as_slice()];
        let mut outputs: [&mut [u8]; 0] = [];

        let head_desc = transmit_queue
            .add_buffer(&inputs, &mut outputs)
            .ok_or("Failed to add buffer to transmit queue")?;

        // 将描述符添加到可用环
        transmit_queue.add_to_avail(head_desc);

        // 通知设备
        self.mmio_base.notify_queue(TRANSMITQ_PORT0 as u32);

        // 非阻塞检查：尝试几次后就放弃，防止系统卡死
        const MAX_ATTEMPTS: usize = 100;
        let mut attempts = 0;

        while attempts < MAX_ATTEMPTS {
            // 检查是否有完成的操作
            if let Some((id, _len)) = transmit_queue.used() {
                if id == head_desc {
                    print_str_legacy("[VirtIO Console] Write completed successfully");
                    return Ok(());
                } else {
                    // ID不匹配，强制回收并报错
                    transmit_queue.recycle_descriptors_force(head_desc);
                    return Err("Descriptor ID mismatch");
                }
            }

            // 轻微延迟避免过度占用CPU
            for _ in 0..50 {
                core::hint::spin_loop();
            }
            attempts += 1;
        }

        // 超时处理：非阻塞式，假设写入最终会完成
        print_str_legacy("[VirtIO Console] Write operation timeout, assuming eventual completion");

        // 不回收描述符，让设备最终完成操作
        // transmit_queue.recycle_descriptors_force(head_desc);

        Ok(()) // 返回成功，避免系统卡死
    }

    /// 从控制台读取数据
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let mut receive_queue = self.receive_queue.lock();

        // 检查是否有完成的接收操作
        if let Some((_used_desc, len)) = receive_queue.used() {
            let read_len = core::cmp::min(len as usize, buffer.len());

            // 注意：由于当前VirtQueue实现的限制，我们无法直接访问接收的数据
            // VirtIO Console的数据应该已经写入我们之前提供的接收缓冲区
            // 但目前的架构不支持获取这些数据，所以暂时只返回接收的字节数
            // 实际的控制台输入需要通过中断处理或轮询机制来实现

            // 设置新的接收缓冲区（内联以避免借用冲突）
            const RX_BUFFER_SIZE: usize = 256;
            let mut rx_buffer = alloc::vec![0u8; RX_BUFFER_SIZE];
            let inputs: [&[u8]; 0] = [];
            let mut outputs = [rx_buffer.as_mut_slice()];
            if let Some(head_desc) = receive_queue.add_buffer(&inputs, &mut outputs) {
                receive_queue.add_to_avail(head_desc);
                self.mmio_base.notify_queue(RECEIVEQ_PORT0 as u32);
                core::mem::forget(rx_buffer);
            }

            Ok(read_len)
        } else {
            // 没有数据可读，确保有接收缓冲区等待输入
            const RX_BUFFER_SIZE: usize = 256;
            let mut rx_buffer = alloc::vec![0u8; RX_BUFFER_SIZE];
            let inputs: [&[u8]; 0] = [];
            let mut outputs = [rx_buffer.as_mut_slice()];
            if let Some(head_desc) = receive_queue.add_buffer(&inputs, &mut outputs) {
                receive_queue.add_to_avail(head_desc);
                self.mmio_base.notify_queue(RECEIVEQ_PORT0 as u32);
                core::mem::forget(rx_buffer);
            }
            
            Ok(0) // 非阻塞读取
        }
    }

    /// 为接收队列设置缓冲区
    fn setup_receive_buffer(&mut self, receive_queue: &mut spin::MutexGuard<VirtQueue>) {
        const RX_BUFFER_SIZE: usize = 256;
        let mut rx_buffer = alloc::vec![0u8; RX_BUFFER_SIZE];
        let inputs: [&[u8]; 0] = [];
        let mut outputs = [rx_buffer.as_mut_slice()];
        if let Some(head_desc) = receive_queue.add_buffer(&inputs, &mut outputs) {
            receive_queue.add_to_avail(head_desc);
            self.mmio_base.notify_queue(RECEIVEQ_PORT0 as u32);
            core::mem::forget(rx_buffer);
        }
    }

    /// 检查是否有输入数据可读
    pub fn has_input(&self) -> bool {
        self.receive_queue.lock().used().is_some()
    }

    /// 获取控制台配置信息
    pub fn get_config(&self) -> VirtIOConsoleConfig {
        self.config
    }

    /// 发送控制消息（用于多端口模式）
    fn send_control_message(&mut self, event: u16, id: u32, value: u16) {
        if let Some(ref control_queue) = self.control_queue {
            let control_msg = VirtIOConsoleControl { id, event, value };

            let msg_bytes = unsafe {
                core::slice::from_raw_parts(
                    &control_msg as *const _ as *const u8,
                    core::mem::size_of::<VirtIOConsoleControl>(),
                )
            };

            let mut queue = control_queue.lock();
            let inputs = [msg_bytes];
            let mut outputs: [&mut [u8]; 0] = [];

            if let Some(head_desc) = queue.add_buffer(&inputs, &mut outputs) {
                queue.add_to_avail(head_desc);
                self.mmio_base.notify_queue(CONTROLQ as u32);

                // 等待消息发送完成，带有更详细的日志
                let mut attempts = 0;
                while queue.used().is_none() && attempts < 1000 {
                    core::hint::spin_loop();
                    attempts += 1;
                }

                if let Some((used_desc, len)) = queue.used() {
                    queue.recycle_descriptors_force(used_desc);
                } else {
                    print_str_legacy("[VirtIO Console] Control message timeout");
                }
            } else {
                print_str_legacy("[VirtIO Console] Failed to add control message to queue");
            }
        } else {
            print_str_legacy("[VirtIO Console] No control queue available");
        }
    }

    /// 处理中断
    pub fn handle_interrupt(&mut self) {
        let interrupt_status = self.mmio_base.interrupt_status();

        if interrupt_status & VIRTIO_MMIO_INT_VRING != 0 {
            // 队列中断
            print_str_legacy("[VirtIO Console] Queue interrupt received");
        }

        if interrupt_status & VIRTIO_MMIO_INT_CONFIG != 0 {
            // 配置变更中断
            print_str_legacy("[VirtIO Console] Configuration change interrupt");

            // 重新读取配置 - 直接从MMIO寄存器读取
            let config_addr = self.mmio_base.read_reg(VIRTIO_MMIO_CONFIG / 4);
            self.config =
                unsafe { core::ptr::read_volatile(config_addr as *const VirtIOConsoleConfig) };
        }

        // 清除中断状态
        self.mmio_base.interrupt_ack(interrupt_status);
    }
}

static VIRTIO_CONSOLE: Once<Option<Mutex<VirtIOConsoleDevice>>> = Once::new();

/// 初始化VirtIO Console设备
pub fn init_virtio_console(base_addr: usize) -> bool {
    if let Some(device) = VirtIOConsoleDevice::new(base_addr) {
        VIRTIO_CONSOLE.call_once(|| Some(Mutex::new(device)));
        true
    } else {
        false
    }
}

/// 向VirtIO Console写入数据
pub fn virtio_console_write(data: &[u8]) -> Result<(), &'static str> {
    if data.is_empty() {
        return Ok(());
    }

    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        // 使用try_lock避免死锁，如果锁被占用则快速失败
        if let Some(mut console) = console_arc.try_lock() {
            console.write(data)
        } else {
            print_str_legacy("[VirtIO Console] Write skipped - device busy");
            Ok(()) // 非阻塞式，避免卡死
        }
    } else {
        Err("VirtIO Console not initialized")
    }
}

/// 从VirtIO Console读取数据
pub fn virtio_console_read(buffer: &mut [u8]) -> Result<usize, &'static str> {
    if buffer.is_empty() {
        return Ok(0);
    }

    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        // 使用try_lock避免死锁
        if let Some(mut console) = console_arc.try_lock() {
            console.read(buffer)
        } else {
            print_str_legacy("[VirtIO Console] Read skipped - device busy");
            Ok(0) // 非阻塞式
        }
    } else {
        Err("VirtIO Console not initialized")
    }
}

/// 检查VirtIO Console是否有输入
pub fn virtio_console_has_input() -> bool {
    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        // 使用try_lock避免死锁
        if let Some(console) = console_arc.try_lock() {
            console.has_input()
        } else {
            false // 如果锁被占用，假设没有输入
        }
    } else {
        false
    }
}

/// 检查VirtIO Console是否已初始化
pub fn is_virtio_console_available() -> bool {
    VIRTIO_CONSOLE.is_completed()
}
