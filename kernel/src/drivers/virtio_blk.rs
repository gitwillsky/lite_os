use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

use crate::memory::address::PhysicalAddress;

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
            println!("[VirtIOBlock] device probe failed");
            return None;
        }

        if mmio.device_id() != VIRTIO_ID_BLOCK {
            println!("[VirtIOBlock] device id not match: {}", mmio.device_id());
            return None;
        }

        println!("[VirtIOBlock] found VirtIO block device");

        // 重置设备
        mmio.set_status(0);

        // 设置ACKNOWLEDGE标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE);

        // 设置DRIVER标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER);

        // 读取设备特性
        let device_features = mmio.device_features();
        println!("[VirtIOBlock] device features: {:#x}", device_features);

        // 设置驱动程序特性 (基础功能)
        mmio.set_driver_features(0);

        // 设置FEATURES_OK标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK);

        // 验证FEATURES_OK
        if mmio.get_status() & VIRTIO_CONFIG_S_FEATURES_OK == 0 {
            println!("[VirtIOBlock] device not accept features");
            return None;
        }

        // 设置页面大小
        mmio.set_guest_page_size(4096);

        // 设置队列
        mmio.select_queue(0);
        let queue_size = mmio.queue_max_size();
        println!("[VirtIOBlock] queue size: {}", queue_size);

        let queue = VirtQueue::new(queue_size as u16, 0)?;
        mmio.set_queue_size(queue_size);
        mmio.set_queue_align(4096);

        let queue_pfn = queue.physical_address().as_usize() >> 12;
        mmio.set_queue_pfn(queue_pfn as u32);

        // 读取容量
        let capacity = unsafe {
            core::ptr::read_volatile((base_addr + VIRTIO_MMIO_CONFIG) as *const u64)
        };
        println!("[VirtIOBlock] device capacity: {} sectors", capacity);

        // 设置DRIVER_OK标志
        mmio.set_status(VIRTIO_CONFIG_S_ACKNOWLEDGE | VIRTIO_CONFIG_S_DRIVER | VIRTIO_CONFIG_S_FEATURES_OK | VIRTIO_CONFIG_S_DRIVER_OK);

        Some(Arc::new(VirtIOBlockDevice {
            mmio,
            queue: Mutex::new(queue),
            capacity,
        }))
    }

    fn perform_io(&self, is_write: bool, block_id: usize, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() != BLOCK_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        if block_id >= self.capacity as usize {
            return Err(BlockError::InvalidBlock);
        }

        let mut queue = self.queue.lock();

        // 准备请求头
        let req = VirtIOBlkReq {
            type_: if is_write { VIRTIO_BLK_T_OUT } else { VIRTIO_BLK_T_IN },
            reserved: 0,
            sector: block_id as u64,
        };

        let req_bytes = unsafe {
            core::slice::from_raw_parts(&req as *const _ as *const u8, core::mem::size_of::<VirtIOBlkReq>())
        };

        let mut status = [0u8; 1];

        // 添加到队列
        let desc_idx = if is_write {
            queue.add_buffer(&[req_bytes, buf], &[&mut status])
        } else {
            queue.add_buffer(&[req_bytes], &[buf, &mut status])
        };

        // 如果添加失败，返回错误
        let desc_idx = desc_idx.ok_or(BlockError::DeviceError)?;

        // 将描述符添加到available ring
        queue.add_to_avail(desc_idx);

        // 通知设备
        self.mmio.notify_queue(0);

        // 等待完成
        let mut timeout = 100000; // Add timeout
        loop {
            if let Some((id, _len)) = queue.get_used() {
                if id == desc_idx {
                    break;
                }
            }
            timeout -= 1;
            if timeout == 0 {
                println!("[VirtIOBlock] I/O operation timed out");
                return Err(BlockError::IoError);
            }
            // 简单的忙等待，实际实现中应该使用中断
            core::hint::spin_loop();
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