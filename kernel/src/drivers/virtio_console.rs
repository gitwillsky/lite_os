use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::{Mutex, Once};

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

        // 检查设备ID
        if mmio_region.device_id() != VIRTIO_ID_CONSOLE {
            return None;
        }

        info!("[VirtIO Console] Found console device at {:#x}", base_addr);

        // 获取设备配置
        let config = unsafe {
            core::ptr::read_volatile((base_addr + VIRTIO_MMIO_CONFIG) as *const VirtIOConsoleConfig)
        };

        debug!(
            "[VirtIO Console] Config: cols={}, rows={}, max_ports={}",
            config.cols, config.rows, config.max_nr_ports
        );

        // 检查多端口特性
        let device_features = mmio_region.device_features();
        let multiport = (device_features & (1 << VIRTIO_CONSOLE_F_MULTIPORT)) != 0;

        debug!(
            "[VirtIO Console] Features: multiport={}, emerg_write={}",
            multiport,
            (device_features & (1 << VIRTIO_CONSOLE_F_EMERG_WRITE)) != 0
        );

        // 协商特性 - 为了稳定性，先禁用多端口，只启用紧急写入
        let mut driver_features = 0u32;
        if (device_features & (1 << VIRTIO_CONSOLE_F_EMERG_WRITE)) != 0 {
            driver_features |= 1 << VIRTIO_CONSOLE_F_EMERG_WRITE;
            debug!("[VirtIO Console] Enabling emergency write feature");
        }
        mmio_region.set_driver_features(driver_features);

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
        mmio_region.set_queue_ready(1);

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
        mmio_region.set_queue_ready(1);

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

        // 设置页面大小
        mmio_region.set_guest_page_size(4096);

        // 设置状态为驱动OK
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
        }

        debug!("[VirtIO Console] Device initialization complete, returning device");
        Some(device)
    }

    /// 向控制台写入数据
    pub fn write(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.is_empty() {
            return Ok(());
        }

        let mut transmit_queue = self.transmit_queue.lock();

        // 使用add_buffer方法添加输出缓冲区
        let inputs = [data];
        let mut outputs: [&mut [u8]; 0] = [];

        let head_desc = transmit_queue
            .add_buffer(&inputs, &mut outputs)
            .ok_or("Failed to add buffer to transmit queue")?;

        // 将描述符添加到可用环
        transmit_queue.add_to_avail(head_desc);

        // 通知设备
        self.mmio_base.notify_queue(TRANSMITQ_PORT0 as u32);

        // 等待传输完成
        while transmit_queue.used().is_none() {
            core::hint::spin_loop();
        }

        // 处理已使用的描述符
        if let Some((used_desc, _len)) = transmit_queue.used() {
            transmit_queue.recycle_descriptors_force(used_desc);
        }

        Ok(())
    }

    /// 从控制台读取数据
    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let mut receive_queue = self.receive_queue.lock();

        // 检查是否有可用数据
        if let Some((used_desc, len)) = receive_queue.used() {
            let read_len = core::cmp::min(len as usize, buffer.len());
            // 数据已经在buffer中（因为描述符指向buffer）
            receive_queue.recycle_descriptors_force(used_desc);
            Ok(read_len)
        } else {
            // 没有数据可读，提供缓冲区给设备用于将来的输入
            let inputs: [&[u8]; 0] = [];
            let mut outputs = [buffer];

            if let Some(head_desc) = receive_queue.add_buffer(&inputs, &mut outputs) {
                receive_queue.add_to_avail(head_desc);
                self.mmio_base.notify_queue(RECEIVEQ_PORT0 as u32);
            }

            Ok(0) // 非阻塞读取
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
                    warn!(
                        "[VirtIO Console] Control message timeout after {} attempts",
                        attempts
                    );
                }
            } else {
                warn!("[VirtIO Console] Failed to add control message to queue");
            }
        } else {
            warn!(
                "[VirtIO Console] No control queue available for message: event={}",
                event
            );
        }
    }

    /// 处理中断
    pub fn handle_interrupt(&mut self) {
        let interrupt_status = self.mmio_base.interrupt_status();

        if interrupt_status & VIRTIO_MMIO_INT_VRING != 0 {
            // 队列中断
            debug!("[VirtIO Console] Queue interrupt received");
        }

        if interrupt_status & VIRTIO_MMIO_INT_CONFIG != 0 {
            // 配置变更中断
            debug!("[VirtIO Console] Configuration change interrupt");

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
    debug!("[VirtIO Console] Starting init_virtio_console at {:#x}", base_addr);
    if let Some(device) = VirtIOConsoleDevice::new(base_addr) {
        debug!("[VirtIO Console] Device created, setting up global instance");
        VIRTIO_CONSOLE.call_once(|| Some(Mutex::new(device)));
        info!("[VirtIO Console] Global console device initialized successfully");
        debug!("[VirtIO Console] init_virtio_console returning true");
        true
    } else {
        debug!("[VirtIO Console] Device creation failed");
        false
    }
}

/// 向VirtIO Console写入数据
pub fn virtio_console_write(data: &[u8]) -> Result<(), &'static str> {
    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        let mut console = console_arc.lock();
        console.write(data)
    } else {
        Err("VirtIO Console not initialized")
    }
}

/// 从VirtIO Console读取数据
pub fn virtio_console_read(buffer: &mut [u8]) -> Result<usize, &'static str> {
    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        let mut console = console_arc.lock();
        console.read(buffer)
    } else {
        Err("VirtIO Console not initialized")
    }
}

/// 检查VirtIO Console是否有输入
pub fn virtio_console_has_input() -> bool {
    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        let console = console_arc.lock();
        console.has_input()
    } else {
        false
    }
}

/// 检查VirtIO Console是否已初始化
pub fn is_virtio_console_available() -> bool {
    VIRTIO_CONSOLE.is_completed()
}
