use alloc::sync::Arc;
use spin::Mutex;

use crate::drivers::hal::{interrupt, virtio::VirtIODevice};

use super::{
    block::{BLOCK_SIZE, BlockDevice, BlockError},
    virtio_queue::*,
};

const VIRTIO_BLK_T_IN: u32 = 0;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtIOBlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

pub struct VirtIOBlockDevice {
    device: VirtIODevice,
    queue: Mutex<VirtQueue>,
    capacity: u64,
}

impl VirtIOBlockDevice {
    pub fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut virtio_device = VirtIODevice::new(base_addr, 0x1000).ok()?;

        if virtio_device.device_id() != 2 {
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
        let queue_size_u16 = u16::try_from(queue_size).ok()?;
        let queue = VirtQueue::new(queue_size_u16)?;

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

    fn read(&self, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError> {
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
            type_: VIRTIO_BLK_T_IN,
            reserved: 0,
            sector: sector_id as u64,
        };

        // SAFETY: request 是 `repr(C)` 且在同步 I/O 完成前一直存活；
        // 只创建不可变字节视图，长度等于完整 request 对象。
        let req_bytes = unsafe {
            core::slice::from_raw_parts(
                &req as *const _ as *const u8,
                core::mem::size_of::<VirtIOBlkReq>(),
            )
        };

        let mut status = [0u8; 1];

        let status_slice: &mut [u8] = &mut status;
        let mut outputs = [buf, status_slice];
        let desc_idx = queue.add_buffer(&[req_bytes], &mut outputs);

        let desc_idx = desc_idx.ok_or_else(|| {
            debug!(
                "[VIRTIO_BLK] Failed to add buffer to queue for block {}",
                block_id
            );
            BlockError::DeviceError
        })?;

        queue.add_to_avail(desc_idx);

        if self.device.notify_queue(0).is_err() {
            panic!("VirtIO queue notify failed after publishing DMA descriptors");
        }

        let mut completed = false;

        // 设备完成前不能返回：描述符仍引用当前栈上的 request/status/buffer，
        // 没有 reset + DMA quiesce 协议时超时返回会让设备晚到写入已复用的栈内存。
        while !completed {
            // Check and acknowledge interrupt if present
            if let Ok(int_status) = self.device.interrupt_status() {
                if int_status & 0x1 != 0 {
                    let _ = self.device.interrupt_ack(0x1);
                }
            }

            // Drain used ring entries
            loop {
                match queue.used() {
                    Ok(Some((id, _len))) if id == desc_idx => {
                        completed = true;
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(()) => panic!("VirtIO device returned a corrupt used-ring chain"),
                }
            }

            if completed {
                break;
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
        self.read(block_id, buf)?;
        Ok(buf.len())
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
}

struct VirtIOBlockIrqHandler {
    device: Arc<VirtIOBlockDevice>,
}

impl interrupt::InterruptHandler for VirtIOBlockIrqHandler {
    fn handle_interrupt(
        &self,
        _vector: interrupt::InterruptVector,
    ) -> Result<(), interrupt::InterruptError> {
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
}

impl VirtIOBlockDevice {
    pub fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn interrupt::InterruptHandler> {
        Arc::new(VirtIOBlockIrqHandler {
            device: self.clone(),
        })
    }
}
