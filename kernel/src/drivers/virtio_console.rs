use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::boxed::Box;
use spin::{Mutex, Once};

use super::{
    hal::{VirtIODevice, Device, DeviceType, DeviceState, DeviceError, InterruptHandler},
    virtio_queue::*,
};

pub const VIRTIO_CONSOLE_F_SIZE: u32 = 0;
pub const VIRTIO_CONSOLE_F_MULTIPORT: u32 = 1;
pub const VIRTIO_CONSOLE_F_EMERG_WRITE: u32 = 2;

pub const RECEIVEQ_PORT0: u16 = 0;
pub const TRANSMITQ_PORT0: u16 = 1;
pub const CONTROLQ: u16 = 2;
pub const CONTROL_RECEIVEQ: u16 = 3;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOConsoleConfig {
    pub cols: u16,
    pub rows: u16,
    pub max_nr_ports: u32,
    pub emerg_wr: u32,
}

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
    pub id: u32,
    pub event: u16,
    pub value: u16,
}

pub struct VirtIOConsoleDevice {
    device: VirtIODevice,
    receive_queue: Arc<Mutex<VirtQueue>>,
    transmit_queue: Arc<Mutex<VirtQueue>>,
    control_queue: Option<Arc<Mutex<VirtQueue>>>,
    control_receive_queue: Option<Arc<Mutex<VirtQueue>>>,
    config: VirtIOConsoleConfig,
    multiport: bool,
}

impl VirtIOConsoleDevice {
    pub fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut virtio_device = VirtIODevice::new(base_addr, 0x1000).ok()?;

        if virtio_device.device_type() != DeviceType::Console {
            return None;
        }

        info!("[VirtIO Console] Found console device at {:#x}", base_addr);

        virtio_device.initialize().ok()?;

        let device_features = virtio_device.device_features().ok()?;
        let multiport = (device_features & (1 << VIRTIO_CONSOLE_F_MULTIPORT)) != 0;

        let mut driver_features = 0u32;
        if (device_features & (1 << VIRTIO_CONSOLE_F_EMERG_WRITE)) != 0 {
            driver_features |= 1 << VIRTIO_CONSOLE_F_EMERG_WRITE;
            debug!("[VirtIO Console] Enabling emergency write feature");
        }

        virtio_device.set_driver_features(driver_features).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device.set_status(status | super::hal::virtio::VIRTIO_CONFIG_S_FEATURES_OK).ok()?;

        if virtio_device.get_status().ok()? & super::hal::virtio::VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        virtio_device.set_guest_page_size(4096).ok()?;

        let config = VirtIOConsoleConfig {
            cols: virtio_device.read_config_u16(0).unwrap_or(80),
            rows: virtio_device.read_config_u16(2).unwrap_or(25),
            max_nr_ports: virtio_device.read_config_u32(4).unwrap_or(1),
            emerg_wr: virtio_device.read_config_u32(8).unwrap_or(0),
        };

        debug!(
            "[VirtIO Console] Config: cols={}, rows={}, max_ports={}",
            config.cols, config.rows, config.max_nr_ports
        );

        let multiport = false;
        debug!("[VirtIO Console] Using single-port mode for stability");

        virtio_device.select_queue(RECEIVEQ_PORT0 as u32).ok()?;
        let rx_queue_size = virtio_device.queue_max_size().ok()?;
        debug!("[VirtIO Console] RX queue max size: {}", rx_queue_size);

        if rx_queue_size == 0 {
            error!("[VirtIO Console] Invalid RX queue size");
            return None;
        }

        let receive_queue = Arc::new(Mutex::new(VirtQueue::new(
            rx_queue_size as u16,
            RECEIVEQ_PORT0 as usize,
        )?));

        virtio_device.set_queue_size(rx_queue_size).ok()?;
        virtio_device.set_queue_align(4096).ok()?;
        let rx_queue_pfn = receive_queue.lock().physical_address().as_usize() >> 12;
        virtio_device.set_queue_pfn(rx_queue_pfn as u32).ok()?;

        virtio_device.select_queue(TRANSMITQ_PORT0 as u32).ok()?;
        let tx_queue_size = virtio_device.queue_max_size().ok()?;
        debug!("[VirtIO Console] TX queue max size: {}", tx_queue_size);

        let transmit_queue = if tx_queue_size == 0 {
            warn!("[VirtIO Console] Queue 1 not available, using shared queue");
            Arc::clone(&receive_queue)
        } else {
            let tq = Arc::new(Mutex::new(VirtQueue::new(
                tx_queue_size as u16,
                TRANSMITQ_PORT0 as usize,
            )?));

            virtio_device.set_queue_size(tx_queue_size).ok()?;
            virtio_device.set_queue_align(4096).ok()?;
            let tx_queue_pfn = tq.lock().physical_address().as_usize() >> 12;
            virtio_device.set_queue_pfn(tx_queue_pfn as u32).ok()?;

            tq
        };

        let (control_queue, control_receive_queue) = (None, None);

        virtio_device.select_queue(RECEIVEQ_PORT0 as u32).ok()?;
        virtio_device.set_queue_ready(1).ok()?;

        virtio_device.select_queue(TRANSMITQ_PORT0 as u32).ok()?;
        virtio_device.set_queue_ready(1).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device.set_status(status | super::hal::virtio::VIRTIO_CONFIG_S_DRIVER_OK).ok()?;

        let mut device = Self {
            device: virtio_device,
            receive_queue,
            transmit_queue,
            control_queue,
            control_receive_queue,
            config,
            multiport,
        };

        if !multiport {
            let receive_queue_clone = Arc::clone(&device.receive_queue);
            let mut rx_queue = receive_queue_clone.lock();
            device.setup_receive_buffers(&mut rx_queue);
        }

        info!("[VirtIO Console] Device initialization completed successfully");
        Some(Arc::new(device))
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), &'static str> {
        if data.is_empty() {
            return Ok(());
        }

        debug!("[VirtIO Console] Writing {} bytes", data.len());

        if self.supports_emergency_write() {
            return self.emergency_write(data);
        }

        let mut transmit_queue = self.transmit_queue.lock();

        if transmit_queue.num_free == 0 {
            error!("[VirtIO Console] Transmit queue full");
            return Err("Transmit queue full");
        }

        let buffer_len = core::cmp::min(data.len(), 1024);
        let mut temp_buffer = alloc::vec![0u8; buffer_len];
        temp_buffer.copy_from_slice(&data[..buffer_len]);

        let inputs = [temp_buffer.as_slice()];
        let mut outputs: [&mut [u8]; 0] = [];

        let head_desc = transmit_queue
            .add_buffer(&inputs, &mut outputs)
            .ok_or("Failed to add buffer to transmit queue")?;

        transmit_queue.add_to_avail(head_desc);

        self.device.notify_queue(TRANSMITQ_PORT0 as u32).map_err(|_| "Notify failed")?;

        const MAX_WAIT_CYCLES: usize = 1000;
        let mut cycles = 0;

        loop {
            if let Some((id, _len)) = transmit_queue.used() {
                if id == head_desc {
                    debug!("[VirtIO Console] Write completed after {} cycles", cycles);
                    return Ok(());
                } else {
                    transmit_queue.recycle_descriptors_force(id);
                }
            }

            for _ in 0..10 {
                core::hint::spin_loop();
            }

            cycles += 1;
            if cycles >= MAX_WAIT_CYCLES {
                debug!("[VirtIO Console] Write timeout, but data submitted");
                return Ok(());
            }
        }
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, &'static str> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let mut receive_queue = self.receive_queue.lock();

        if let Some((used_desc, len)) = receive_queue.used() {
            let read_len = core::cmp::min(len as usize, buffer.len());
            debug!("[VirtIO Console] Received {} bytes, descriptor {}", len, used_desc);

            if receive_queue.num_free == receive_queue.size {
                self.setup_receive_buffers(&mut receive_queue);
            }

            Ok(read_len)
        } else {
            if receive_queue.num_free == receive_queue.size {
                self.setup_receive_buffers(&mut receive_queue);
            }
            Ok(0)
        }
    }

    fn setup_receive_buffer(&self, receive_queue: &mut spin::MutexGuard<VirtQueue>) {
        const RX_BUFFER_SIZE: usize = 256;
        let mut rx_buffer = alloc::vec![0u8; RX_BUFFER_SIZE];
        let inputs: [&[u8]; 0] = [];
        let mut outputs = [rx_buffer.as_mut_slice()];

        if let Some(head_desc) = receive_queue.add_buffer(&inputs, &mut outputs) {
            receive_queue.add_to_avail(head_desc);
            let _ = self.device.notify_queue(RECEIVEQ_PORT0 as u32);
            core::mem::forget(rx_buffer);
            debug!("[VirtIO Console] Set up receive buffer of {} bytes", RX_BUFFER_SIZE);
        } else {
            error!("[VirtIO Console] Failed to set up receive buffer");
        }
    }

    fn setup_receive_buffers(&self, receive_queue: &mut spin::MutexGuard<VirtQueue>) {
        for i in 0..4 {
            if receive_queue.num_free < 1 {
                debug!("[VirtIO Console] No more free descriptors for receive buffer {}", i);
                break;
            }
            self.setup_receive_buffer(receive_queue);
        }
    }

    pub fn has_input(&self) -> bool {
        self.receive_queue.lock().used().is_some()
    }

    fn supports_emergency_write(&self) -> bool {
        let device_features = self.device.device_features().unwrap_or(0);
        (device_features & (1 << VIRTIO_CONSOLE_F_EMERG_WRITE)) != 0
    }

    fn emergency_write(&self, data: &[u8]) -> Result<(), &'static str> {
        debug!("[VirtIO Console] Using emergency write for {} bytes", data.len());

        for &byte in data {
            if let Err(_) = self.device.write_config_u32(12, byte as u32) {
                return Err("Emergency write failed");
            }

            for _ in 0..10 {
                core::hint::spin_loop();
            }
        }

        debug!("[VirtIO Console] Emergency write completed");
        Ok(())
    }

    pub fn get_config(&self) -> VirtIOConsoleConfig {
        self.config
    }

    pub fn handle_interrupt(&mut self) {
        let interrupt_status = self.device.interrupt_status().unwrap_or(0);

        if interrupt_status & super::hal::virtio::VIRTIO_MMIO_INT_VRING != 0 {
            debug!("[VirtIO Console] Queue interrupt received");
        }

        if interrupt_status & super::hal::virtio::VIRTIO_MMIO_INT_CONFIG != 0 {
            debug!("[VirtIO Console] Configuration change interrupt");
        }

        let _ = self.device.interrupt_ack(interrupt_status);
    }
}

impl Device for VirtIOConsoleDevice {
    fn device_type(&self) -> DeviceType {
        self.device.device_type()
    }

    fn device_id(&self) -> u32 {
        self.device.device_id()
    }

    fn vendor_id(&self) -> u32 {
        self.device.vendor_id()
    }

    fn device_name(&self) -> alloc::string::String {
        self.device.device_name()
    }

    fn driver_name(&self) -> alloc::string::String {
        self.device.driver_name()
    }

    fn state(&self) -> DeviceState {
        self.device.state()
    }

    fn probe(&mut self) -> Result<bool, DeviceError> {
        self.device.probe()
    }

    fn initialize(&mut self) -> Result<(), DeviceError> {
        Err(DeviceError::InvalidState)
    }

    fn reset(&mut self) -> Result<(), DeviceError> {
        self.device.reset()
    }

    fn shutdown(&mut self) -> Result<(), DeviceError> {
        self.device.shutdown()
    }

    fn remove(&mut self) -> Result<(), DeviceError> {
        self.device.remove()
    }

    fn suspend(&mut self) -> Result<(), DeviceError> {
        self.device.suspend()
    }

    fn resume(&mut self) -> Result<(), DeviceError> {
        Err(DeviceError::NotSupported)
    }

    fn bus(&self) -> Arc<dyn super::hal::Bus> {
        self.device.bus()
    }

    fn resources(&self) -> alloc::vec::Vec<super::hal::resource::Resource> {
        self.device.resources()
    }

    fn request_resources(&mut self, resource_manager: &mut dyn super::hal::resource::ResourceManager) -> Result<(), DeviceError> {
        self.device.request_resources(resource_manager)
    }

    fn release_resources(&mut self, resource_manager: &mut dyn super::hal::resource::ResourceManager) -> Result<(), DeviceError> {
        self.device.release_resources(resource_manager)
    }

    fn supports_interrupt(&self) -> bool {
        true
    }

    fn set_interrupt_handler(&mut self, vector: super::hal::InterruptVector, handler: Arc<dyn InterruptHandler>) -> Result<(), DeviceError> {
        self.device.set_interrupt_handler(vector, handler)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

static VIRTIO_CONSOLE: Once<Option<Mutex<Arc<VirtIOConsoleDevice>>>> = Once::new();

pub fn init_virtio_console(base_addr: usize) -> bool {
    if let Some(device) = VirtIOConsoleDevice::new(base_addr) {
        VIRTIO_CONSOLE.call_once(|| Some(Mutex::new(device)));
        true
    } else {
        false
    }
}

pub fn virtio_console_write(data: &[u8]) -> Result<(), &'static str> {
    if data.is_empty() {
        return Ok(());
    }

    debug!("[VirtIO Console API] Write request for {} bytes", data.len());

    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        let console_wrapper = console_arc.lock();
        let console_ptr = Arc::as_ptr(&console_wrapper) as *mut VirtIOConsoleDevice;

        let result = unsafe { (*console_ptr).write(data) };
        match &result {
            Ok(()) => debug!("[VirtIO Console API] Write successful"),
            Err(e) => error!("[VirtIO Console API] Write failed: {}", e),
        }
        result
    } else {
        error!("[VirtIO Console API] Console not initialized");
        Err("VirtIO Console not initialized")
    }
}

pub fn virtio_console_read(buffer: &mut [u8]) -> Result<usize, &'static str> {
    if buffer.is_empty() {
        return Ok(0);
    }

    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        let console_wrapper = console_arc.lock();
        let console_ptr = Arc::as_ptr(&console_wrapper) as *mut VirtIOConsoleDevice;

        unsafe { (*console_ptr).read(buffer) }
    } else {
        Err("VirtIO Console not initialized")
    }
}

pub fn virtio_console_has_input() -> bool {
    let console_guard = VIRTIO_CONSOLE.wait();
    if let Some(console_arc) = console_guard.as_ref() {
        if let Some(console) = console_arc.try_lock() {
            console.has_input()
        } else {
            false
        }
    } else {
        false
    }
}

pub fn is_virtio_console_available() -> bool {
    VIRTIO_CONSOLE.is_completed()
}