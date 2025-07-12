use alloc::sync::Arc;
use spin::Mutex;

use super::{
    block::{BlockDevice, BlockError, BLOCK_SIZE},
    virtio_mmio::*,
    virtio_queue::*,
};

// VirtIO Block 设备特性
pub const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
pub const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
pub const VIRTIO_BLK_F_GEOMETRY: u32 = 4;
pub const VIRTIO_BLK_F_RO: u32 = 5;
pub const VIRTIO_BLK_F_BLK_SIZE: u32 = 6;
pub const VIRTIO_BLK_F_FLUSH: u32 = 9;
pub const VIRTIO_BLK_F_TOPOLOGY: u32 = 10;
pub const VIRTIO_BLK_F_CONFIG_WCE: u32 = 11;

// VirtIO Block 请求类型
pub const VIRTIO_BLK_T_IN: u32 = 0;
pub const VIRTIO_BLK_T_OUT: u32 = 1;
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;

// VirtIO Block 状态
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
    mmio: VirtIOMMIO,
    queue: Mutex<VirtQueue>,
    capacity: u64,
}

impl VirtIOBlockDevice {
    pub fn new(base_addr: usize) -> Option<Arc<Self>> {
        let mmio = VirtIOMMIO::new(base_addr);

        // 探测设备
        if !mmio.probe() {
            debug!("VirtIO block device probe failed");
            return None;
        }

        if mmio.device_id() != VIRTIO_ID_BLOCK {
            debug!("VirtIO device ID mismatch: expected {}, got {}", VIRTIO_ID_BLOCK, mmio.device_id());
            return None;
        }

        // 重置设备
        mmio.set_status(0);

        // 设置ACKNOWLEDGE标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE);

        // 设置DRIVER标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER);

        // 读取设备特性
        let device_features = mmio.device_features();

        // 设置驱动程序特性 (基础功能)
        mmio.set_driver_features(0);

        // 设置FEATURES_OK标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK);

        // 验证FEATURES_OK
        if mmio.get_status() & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            error!("VirtIO block device does not accept features");
            return None;
        }

        // 设置页面大小
        mmio.set_guest_page_size(4096);

        // 设置队列
        mmio.select_queue(0);
        let queue_size = mmio.queue_max_size();

        let queue = VirtQueue::new(queue_size as u16, 0)?;

        mmio.set_queue_size(queue_size);
        mmio.set_queue_align(4096);

        let queue_pfn = queue.physical_address().as_usize() >> 12;

        // 设置队列PFN
        mmio.set_queue_pfn(queue_pfn as u32);

        // 读回验证
        let readback_pfn = mmio.read_reg(VIRTIO_MMIO_QUEUE_PFN);

        if readback_pfn != queue_pfn as u32 {
            warn!("Queue PFN mismatch: set {:#x}, readback {:#x}", queue_pfn, readback_pfn);
        }

        // 设置队列就绪标志 - 这是关键的修复
        mmio.write_reg(VIRTIO_MMIO_QUEUE_READY, 1);

        // 读取容量
        let capacity = unsafe {
            core::ptr::read_volatile((base_addr + VIRTIO_MMIO_CONFIG) as *const u64)
        };
        // print device capacity in MB
        info!("VirtIO block device capacity: {} MB", capacity * 512 / 1024 / 1024);

        // 设置DRIVER_OK标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK | VIRTIO_CONFIG_S_DRIVER_OK);

        Some(Arc::new(VirtIOBlockDevice {
            mmio,
            queue: Mutex::new(queue),
            capacity,
        }))
    }

    fn perform_io(&self, is_write: bool, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError> {
        debug!("VirtIO block {} operation: block {} (sector {})",
               if is_write { "write" } else { "read" },
               block_id,
               block_id * (BLOCK_SIZE / 512));

        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        if block_id >= self.capacity as usize {
            return Err(BlockError::InvalidBlock);
        }


        let mut queue = self.queue.lock();

        // 准备请求头 - sector字段应该是512字节扇区号，不是4096字节块号
        let sectors_per_block = BLOCK_SIZE / 512; // 4096 / 512 = 8
        let sector_id = block_id * sectors_per_block;

        let req = VirtIOBlkReq {
            type_: if is_write { VIRTIO_BLK_T_OUT } else { VIRTIO_BLK_T_IN },
            reserved: 0,
            sector: sector_id as u64,
        };


        let req_bytes = unsafe {
            core::slice::from_raw_parts(&req as *const _ as *const u8, core::mem::size_of::<VirtIOBlkReq>())
        };

        let mut status = [0u8; 1];

        // 添加到队列
        let desc_idx = if is_write {
            let mut status_slice: &mut [u8] = &mut status;
            let mut outputs = [status_slice];
            queue.add_buffer(&[req_bytes, buf], &mut outputs)
        } else {
            let mut status_slice: &mut [u8] = &mut status;
            let mut outputs = [buf, status_slice];
            queue.add_buffer(&[req_bytes], &mut outputs)
        };

        // 如果添加失败，返回错误
        let desc_idx = desc_idx.ok_or(BlockError::DeviceError)?;


        // 将描述符添加到available ring
        queue.add_to_avail(desc_idx);


        // 通知设备
        self.mmio.notify_queue(0);

        // 等待完成 - 先检查中断状态，如果有中断则处理
        const MAX_ATTEMPTS: usize = 1000;
        let mut attempts = MAX_ATTEMPTS;

        loop {
            // 检查设备中断状态
            let int_status = self.mmio.read_reg(VIRTIO_MMIO_INTERRUPT_STATUS);
            if int_status & 0x1 != 0 {
                // 发现used buffer中断，确认中断并处理
                self.mmio.write_reg(VIRTIO_MMIO_INTERRUPT_ACK, 0x1);
                break;
            }

            // 直接检查used ring索引是否更新
            let used_idx = unsafe {
                core::ptr::read_volatile(&(*queue.used).idx as *const core::sync::atomic::AtomicU16 as *const u16)
            };

            if used_idx != queue.last_used_idx {
                break;
            }

            attempts -= 1;
            if attempts == 0 {
                error!("VirtIO block I/O operation timed out after {} attempts", MAX_ATTEMPTS);
                // 检查设备状态
                let device_status = self.mmio.read_reg(VIRTIO_MMIO_STATUS);
                let int_status = self.mmio.read_reg(VIRTIO_MMIO_INTERRUPT_STATUS);
                debug!("Device status: {:#x}, Interrupt status: {:#x}", device_status, int_status);

                return Err(BlockError::IoError);
            }

            // 稍微增加延迟
            for _ in 0..1000 {
                core::hint::spin_loop();
            }
        }

        // 现在获取结果
        if let Some((id, _len)) = queue.get_used() {
            if id != desc_idx {
                error!("VirtIO block descriptor ID mismatch: expected {}, got {}", desc_idx, id);
            }
        }

        // 检查状态
        match status[0] {
            VIRTIO_BLK_S_OK => Ok(()),
            VIRTIO_BLK_S_IOERR => Err(BlockError::IoError),
            VIRTIO_BLK_S_UNSUPP => Err(BlockError::DeviceError),
            _ => Err(BlockError::DeviceError),
        }
    }
}

impl BlockDevice for VirtIOBlockDevice {
    fn read_block(&self, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError> {
        self.perform_io(false, block_id, buf)
    }

    fn write_block(&self, block_id: usize, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        let mut write_buf = [0u8; BLOCK_SIZE];
        write_buf.copy_from_slice(buf);
        self.perform_io(true, block_id, &mut write_buf)
    }

    fn num_blocks(&self) -> usize {
        self.capacity as usize
    }

    fn block_size(&self) -> usize {
        BLOCK_SIZE
    }
}