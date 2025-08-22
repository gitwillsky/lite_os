use alloc::boxed::Box;
use alloc::sync::Arc;
use spin::Mutex;

use super::{
    block::{BLOCK_SIZE, BlockDevice, BlockError},
    hal::{Device, DeviceError, DeviceState, DeviceType, MmioBus, VirtIODevice},
    virtio_queue::*,
};

pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
pub const VIRTIO_BLK_F_GEOMETRY: u32 = 4;
pub const VIRTIO_BLK_F_RO: u32 = 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 9;
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 10;
pub const VIRTIO_BLK_F_CONFIG_WCE: u32 = 11;

pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOBlkConfig {
    pub capacity: u64,
    pub size_max: u32,
    pub seg_max: u32,
    pub geometry: VirtIOBlkGeometry,
    pub blk_size: u32,
    pub topology: VirtIOBlkTopology,
    pub writeback: u8,
    pub unused0: [u8; 3],
    pub max_discard_sectors: u32,
    pub max_discard_seg: u32,
    pub discard_sector_alignment: u32,
    pub max_write_zeroes_sectors: u32,
    pub max_write_zeroes_seg: u32,
    pub write_zeroes_may_unmap: u8,
    pub unused1: [u8; 3],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOBlkGeometry {
    pub cylinders: u16,
    pub heads: u8,
    pub sectors: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOBlkTopology {
    pub physical_block_exp: u8,
    pub alignment_offset: u8,
    pub min_io_size: u16,
    pub opt_io_size: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtIOBlkReq {
    pub type_: u32,
    pub reserved: u32,
    pub sector: u64,
}

pub struct VirtIOBlockDevice {
    device: VirtIODevice,
    queue: Mutex<VirtQueue>,
    capacity: u64,
}

impl VirtIOBlockDevice {
    pub fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut virtio_device = VirtIODevice::new(base_addr, 0x1000).ok()?;

        if virtio_device.device_type() != DeviceType::Block {
            return None;
        }

        virtio_device.initialize().ok()?;

        virtio_device.set_driver_features(0).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device
            .set_status(status | super::hal::virtio::VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;

        if virtio_device.get_status().ok()? & super::hal::virtio::VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        virtio_device.set_guest_page_size(4096).ok()?;

        virtio_device.select_queue(0).ok()?;
        let queue_size = virtio_device.queue_max_size().ok()?;

        let queue = VirtQueue::new(queue_size as u16, 0)?;

        virtio_device.set_queue_size(queue_size).ok()?;
        virtio_device.set_queue_align(4096).ok()?;

        let queue_pfn = queue.physical_address().as_usize() >> 12;
        virtio_device.set_queue_pfn(queue_pfn as u32).ok()?;
        virtio_device.set_queue_ready(1).ok()?;

        let capacity = virtio_device.read_config_u64(0).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device
            .set_status(status | super::hal::virtio::VIRTIO_CONFIG_S_DRIVER_OK)
            .ok()?;

        info!(
            "VirtIO block device capacity: {} MB",
            capacity * 512 / 1024 / 1024
        );

        Some(Arc::new(Self {
            device: virtio_device,
            queue: Mutex::new(queue),
            capacity,
        }))
    }

    fn perform_io(
        &self,
        is_write: bool,
        block_id: usize,
        buf: &mut [u8],
    ) -> Result<(), BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        if block_id >= (self.capacity * 512 / BLOCK_SIZE as u64) as usize {
            debug!(
                "[VIRTIO_BLK] Block {} exceeds capacity {}",
                block_id,
                (self.capacity * 512 / BLOCK_SIZE as u64)
            );
            return Err(BlockError::InvalidBlock);
        }

        let mut queue = self.queue.lock();

        let sectors_per_block = BLOCK_SIZE / 512;
        let sector_id = block_id * sectors_per_block;

        let req = VirtIOBlkReq {
            type_: if is_write {
                VIRTIO_BLK_T_OUT
            } else {
                VIRTIO_BLK_T_IN
            },
            reserved: 0,
            sector: sector_id as u64,
        };

        let req_bytes = unsafe {
            core::slice::from_raw_parts(
                &req as *const _ as *const u8,
                core::mem::size_of::<VirtIOBlkReq>(),
            )
        };

        let mut status = [0u8; 1];

        let desc_idx = if is_write {
            let status_slice: &mut [u8] = &mut status;
            let mut outputs = [status_slice];
            queue.add_buffer(&[req_bytes, buf], &mut outputs)
        } else {
            let status_slice: &mut [u8] = &mut status;
            let mut outputs = [buf, status_slice];
            queue.add_buffer(&[req_bytes], &mut outputs)
        };

        let desc_idx = desc_idx.ok_or_else(|| {
            debug!(
                "[VIRTIO_BLK] Failed to add buffer to queue for block {}",
                block_id
            );
            BlockError::DeviceError
        })?;

        queue.add_to_avail(desc_idx);

        self.device
            .notify_queue(0)
            .map_err(|_| BlockError::DeviceError)?;

        const MAX_ATTEMPTS: usize = 100000; // allow more drain cycles on slow devices
        let mut attempts = MAX_ATTEMPTS;
        let mut completed = false;

        // Busy-wait until our descriptor chain is reported in used ring.
        // Drain and recycle any other completed requests in front of us to
        // preserve correct queue ordering.
        while !completed {
            // Check and acknowledge interrupt if present
            if let Ok(int_status) = self.device.interrupt_status() {
                if int_status & 0x1 != 0 {
                    let _ = self.device.interrupt_ack(0x1);
                }
            }

            // Drain used ring entries
            while let Some((id, _len)) = queue.used() {
                if id == desc_idx {
                    completed = true;
                    break;
                } else {
                    // Another request completed earlier. This is valid when
                    // multiple requests are in flight. We already recycled its
                    // descriptors in queue.used(), just continue draining.
                }
            }

            if completed {
                break;
            }

            // Backoff / timeout management
            attempts -= 1;
            if attempts == 0 {
                error!("VirtIO block I/O operation timed out after drain");
                // Do not force-recycle our descriptors; that corrupts the queue
                return Err(BlockError::IoError);
            }
            for _ in 0..200 {
                core::hint::spin_loop();
            }
        }

        match status[0] {
            VIRTIO_BLK_S_OK => Ok(()),
            VIRTIO_BLK_S_IOERR => {
                debug!(
                    "[VIRTIO_BLK] Device reported I/O error for block {}",
                    block_id
                );
                Err(BlockError::IoError)
            }
            VIRTIO_BLK_S_UNSUPP => {
                debug!(
                    "[VIRTIO_BLK] Device reported unsupported operation for block {}",
                    block_id
                );
                Err(BlockError::DeviceError)
            }
            _ => {
                debug!(
                    "[VIRTIO_BLK] Device reported unknown status {} for block {}",
                    status[0], block_id
                );
                Err(BlockError::DeviceError)
            }
        }
    }
}

impl BlockDevice for VirtIOBlockDevice {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<usize, BlockError> {
        self.perform_io(false, block_id, buf)?;
        Ok(buf.len())
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        let mut write_buf = [0u8; BLOCK_SIZE];
        write_buf.copy_from_slice(buf);
        self.perform_io(true, block_id, &mut write_buf)?;
        Ok(buf.len())
    }

    fn num_blocks(&self) -> usize {
        (self.capacity * 512 / BLOCK_SIZE as u64) as usize
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
}

impl Device for VirtIOBlockDevice {
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
        Ok(true) // VirtIOBlockDevice is already initialized
    }

    fn initialize(&mut self) -> Result<(), DeviceError> {
        Ok(()) // Already initialized in new()
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

    fn request_resources(
        &mut self,
        resource_manager: &mut dyn super::hal::resource::ResourceManager,
    ) -> Result<(), DeviceError> {
        self.device.request_resources(resource_manager)
    }

    fn release_resources(
        &mut self,
        resource_manager: &mut dyn super::hal::resource::ResourceManager,
    ) -> Result<(), DeviceError> {
        self.device.release_resources(resource_manager)
    }

    fn supports_interrupt(&self) -> bool {
        self.device.supports_interrupt()
    }

    fn set_interrupt_handler(
        &mut self,
        vector: super::hal::InterruptVector,
        handler: Arc<dyn super::hal::InterruptHandler>,
    ) -> Result<(), DeviceError> {
        self.device.set_interrupt_handler(vector, handler)
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }
}

// === 中断处理支持 ===

struct VirtIOBlockIrqHandler {
    device: Arc<VirtIOBlockDevice>,
}

impl super::hal::interrupt::InterruptHandler for VirtIOBlockIrqHandler {
    fn handle_interrupt(
        &self,
        _vector: super::hal::interrupt::InterruptVector,
    ) -> Result<(), super::hal::interrupt::InterruptError> {
        // 仅做最小化的中断确认，避免与同步 I/O 路径上的队列锁竞争
        if let Ok(status) = self.device.device.interrupt_status() {
            // 确认 VRING 与 CONFIG 两类中断（如存在）
            let _ = self.device.device.interrupt_ack(
                status
                    & (super::hal::virtio::VIRTIO_MMIO_INT_VRING
                        | super::hal::virtio::VIRTIO_MMIO_INT_CONFIG),
            );
        }
        Ok(())
    }

    fn can_handle(&self, _vector: super::hal::interrupt::InterruptVector) -> bool {
        true
    }

    fn name(&self) -> &str {
        "virtio-blk-irq"
    }
}

impl VirtIOBlockDevice {
    pub fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn super::hal::InterruptHandler> {
        Arc::new(VirtIOBlockIrqHandler {
            device: self.clone(),
        })
    }
}
