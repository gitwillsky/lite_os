use alloc::sync::Arc;
use spin::Mutex;

use super::{
    InterruptError, InterruptHandler, InterruptVector, VIRTIO_CONFIG_S_DRIVER_OK,
    VIRTIO_CONFIG_S_FEATURES_OK, VIRTIO_F_VERSION_1, VIRTIO_MMIO_INT_CONFIG, VIRTIO_MMIO_INT_VRING,
    VirtIODevice,
    block::{BLOCK_SIZE, BlockDevice, BlockError},
    virtio_queue::*,
};

const VIRTIO_BLK_T_IN: u32 = 0;
const VIRTIO_BLK_T_OUT: u32 = 1;
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_F_FLUSH: u64 = 1 << 9;

pub(super) const VIRTIO_BLK_S_OK: u8 = 0;
pub(super) const VIRTIO_BLK_S_IOERR: u8 = 1;
pub(super) const VIRTIO_BLK_S_UNSUPP: u8 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct VirtIOBlkReq {
    type_: u32,
    reserved: u32,
    sector: u64,
}

pub(super) struct VirtIOBlockDevice {
    device: VirtIODevice,
    queue: Mutex<VirtQueue>,
    capacity: u64,
    supports_flush: bool,
}

impl VirtIOBlockDevice {
    pub(super) fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mut virtio_device = VirtIODevice::new(base_addr, 0x1000).ok()?;

        if virtio_device.device_id() != 2 {
            return None;
        }

        virtio_device.initialize().ok()?;

        let device_features = virtio_device.device_features().ok()?;
        if device_features & VIRTIO_F_VERSION_1 == 0 {
            return None;
        }
        let driver_features = VIRTIO_F_VERSION_1 | device_features & VIRTIO_BLK_F_FLUSH;
        virtio_device.set_driver_features(driver_features).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device
            .set_status(status | VIRTIO_CONFIG_S_FEATURES_OK)
            .ok()?;

        if virtio_device.get_status().ok()? & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            return None;
        }

        let queue_size = virtio_device.queue_max_size(0).ok()?;
        let queue = VirtQueue::new(queue_size)?;
        virtio_device
            .configure_queue(0, queue_size, queue.addresses())
            .ok()?;

        let capacity = virtio_device.read_config_u64(0).ok()?;

        let status = virtio_device.get_status().ok()?;
        virtio_device
            .set_status(status | VIRTIO_CONFIG_S_DRIVER_OK)
            .ok()?;

        info!(
            "VirtIO block device capacity: {} MB",
            capacity * 512 / 1024 / 1024
        );

        Arc::try_new(Self {
            device: virtio_device,
            queue: Mutex::new(queue),
            capacity,
            supports_flush: driver_features & VIRTIO_BLK_F_FLUSH != 0,
        })
        .ok()
    }

    fn validate_block(&self, block_id: usize, len: usize) -> Result<(), BlockError> {
        if len != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }
        if block_id >= (self.capacity * 512 / BLOCK_SIZE as u64) as usize {
            return Err(BlockError::InvalidBlock);
        }
        Ok(())
    }

    fn complete_request(
        &self,
        queue: &mut VirtQueue,
        desc_idx: u16,
        status: &[u8; 1],
    ) -> Result<(), BlockError> {
        loop {
            if let Ok(int_status) = self.device.interrupt_status()
                && int_status & 0x1 != 0
            {
                let _ = self.device.interrupt_ack(0x1);
            }
            loop {
                match queue.used() {
                    Ok(Some((id, _))) if id == desc_idx => return Self::decode_status(status[0]),
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(()) => panic!("VirtIO device returned a corrupt used-ring chain"),
                }
            }
            for _ in 0..200 {
                core::hint::spin_loop();
            }
        }
    }

    fn decode_status(status: u8) -> Result<(), BlockError> {
        match status {
            VIRTIO_BLK_S_OK => Ok(()),
            VIRTIO_BLK_S_IOERR => Err(BlockError::IoError),
            VIRTIO_BLK_S_UNSUPP => Err(BlockError::DeviceError),
            _ => Err(BlockError::DeviceError),
        }
    }

    fn read(&self, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError> {
        self.validate_block(block_id, buf.len())?;

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

        // 设备完成前不能返回：描述符仍引用当前栈上的 request/status/buffer，
        // 没有 reset + DMA quiesce 协议时超时返回会让设备晚到写入已复用的栈内存。
        self.complete_request(&mut queue, desc_idx, &status)
    }

    fn write(&self, block_id: usize, buf: &[u8]) -> Result<(), BlockError> {
        self.validate_block(block_id, buf.len())?;
        let mut queue = self.queue.lock();
        let req = VirtIOBlkReq {
            type_: VIRTIO_BLK_T_OUT,
            reserved: 0,
            sector: (block_id * (BLOCK_SIZE / 512)) as u64,
        };
        // SAFETY: request is `repr(C)` and remains alive until synchronous queue completion;
        // the immutable byte view covers exactly the request object.
        let req_bytes = unsafe {
            core::slice::from_raw_parts(
                &req as *const _ as *const u8,
                core::mem::size_of::<VirtIOBlkReq>(),
            )
        };
        let mut status = [0u8; 1];
        let mut outputs: [&mut [u8]; 1] = [&mut status];
        let desc_idx = queue
            .add_buffer(&[req_bytes, buf], &mut outputs)
            .ok_or(BlockError::DeviceError)?;
        queue.add_to_avail(desc_idx);
        self.device
            .notify_queue(0)
            .unwrap_or_else(|_| panic!("VirtIO queue notify failed after publishing write"));
        self.complete_request(&mut queue, desc_idx, &status)
    }

    fn flush_device(&self) -> Result<(), BlockError> {
        if !self.supports_flush {
            return Ok(());
        }
        let mut queue = self.queue.lock();
        let req = VirtIOBlkReq {
            type_: VIRTIO_BLK_T_FLUSH,
            reserved: 0,
            sector: 0,
        };
        // SAFETY: request is `repr(C)` and remains alive until synchronous queue completion;
        // the immutable byte view covers exactly the request object.
        let req_bytes = unsafe {
            core::slice::from_raw_parts(
                &req as *const _ as *const u8,
                core::mem::size_of::<VirtIOBlkReq>(),
            )
        };
        let mut status = [0u8; 1];
        let mut outputs: [&mut [u8]; 1] = [&mut status];
        let desc_idx = queue
            .add_buffer(&[req_bytes], &mut outputs)
            .ok_or(BlockError::DeviceError)?;
        queue.add_to_avail(desc_idx);
        self.device
            .notify_queue(0)
            .unwrap_or_else(|_| panic!("VirtIO queue notify failed after publishing flush"));
        self.complete_request(&mut queue, desc_idx, &status)
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

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<usize, BlockError> {
        self.write(block_id, buf)?;
        Ok(buf.len())
    }

    fn flush(&self) -> Result<(), BlockError> {
        self.flush_device()
    }
}

struct VirtIOBlockIrqHandler {
    device: Arc<VirtIOBlockDevice>,
}

impl InterruptHandler for VirtIOBlockIrqHandler {
    fn handle_interrupt(&self, _vector: InterruptVector) -> Result<(), InterruptError> {
        // 仅做最小化的中断确认，避免与同步 I/O 路径上的队列锁竞争
        if let Ok(status) = self.device.device.interrupt_status() {
            // 确认 VRING 与 CONFIG 两类中断（如存在）
            let _ = self
                .device
                .device
                .interrupt_ack(status & (VIRTIO_MMIO_INT_VRING | VIRTIO_MMIO_INT_CONFIG));
        }
        Ok(())
    }
}

impl VirtIOBlockDevice {
    pub(super) fn irq_handler_for(self: &Arc<Self>) -> Arc<dyn InterruptHandler> {
        Arc::try_new(VirtIOBlockIrqHandler {
            device: self.clone(),
        })
        .expect("VirtIO block IRQ handler allocation failed")
    }
}
