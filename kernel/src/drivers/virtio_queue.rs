use crate::memory::{
    address::{PhysicalAddress, VirtualAddress},
    frame_allocator::{self, FrameTracker},
};
use crate::drivers::hal::memory::{DmaBuffer, MemoryError};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU16, Ordering, fence};

// VirtIO Ring 描述符标志
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

// VirtIO Used Ring 标志
pub const VIRTQ_USED_F_NO_NOTIFY: u16 = 1;

// VirtIO Available Ring 标志
pub const VIRTQ_AVAIL_F_NO_INTERRUPT: u16 = 1;

/// VirtIO队列错误类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtQueueError {
    InvalidSize,
    OutOfMemory,
    NoFreeDescriptors,
    InvalidDescriptor,
    QueueFull,
    BufferTooLarge,
    DmaError,
}

impl From<MemoryError> for VirtQueueError {
    fn from(_: MemoryError) -> Self {
        VirtQueueError::DmaError
    }
}

/// VirtIO队列统计信息
#[derive(Debug, Clone, Default)]
pub struct VirtQueueStats {
    pub total_requests: u64,
    pub completed_requests: u64,
    pub failed_requests: u64,
    pub bytes_transferred: u64,
    pub average_latency_ns: u64,
    pub queue_full_events: u64,
    pub descriptor_exhaustion: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtqAvail {
    pub flags: AtomicU16,
    pub idx: AtomicU16,
    pub ring: [u16; 0], // 实际大小由队列大小决定
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[repr(C)]
#[derive(Debug)]
pub struct VirtqUsed {
    pub flags: AtomicU16,
    pub idx: AtomicU16,
    pub ring: [VirtqUsedElem; 0], // 实际大小由队列大小决定
}

pub struct VirtQueue {
    pub size: u16,
    pub desc: *mut VirtqDesc,
    pub avail: *mut VirtqAvail,
    pub used: *mut VirtqUsed,
    pub free_head: u16,
    pub num_free: u16,
    pub last_used_idx: u16,
    pub avail_idx: u16,
    pub queue_token: usize,
    // Shadow descriptors that device can't access - inspired by virtio-drivers
    desc_shadow: Vec<VirtqDesc>,
    _frame_tracker: FrameTracker,
    // 统计信息和管理数据
    stats: VirtQueueStats,
    creation_time: u64, // 队列创建时间，用于调试
}

impl VirtQueue {
    pub fn new(size: u16, queue_token: usize) -> Option<Self> {
        if size == 0 || size & (size - 1) != 0 {
            error!("[VirtQueue] Invalid queue size: {} (must be power of 2)", size);
            return None; // 队列大小必须是2的幂
        }

        debug!("[VirtQueue] Creating queue with size={}, token={}", size, queue_token);

        // 计算需要的内存大小 - 严格按照VirtIO规范进行对齐
        let desc_size = size_of::<VirtqDesc>() * size as usize;

        // Available ring: flags(2) + idx(2) + ring[size](2*size) + used_event(2)
        let avail_size = 2 + 2 + 2 * size as usize + 2;
        let avail_offset = desc_size;

        // Used ring需要对齐到4字节边界
        let used_offset = (avail_offset + avail_size + 3) & !3;
        // Used ring: flags(2) + idx(2) + ring[size](8*size) + avail_event(2)
        let used_size = 2 + 2 + 8 * size as usize + 2;

        // 总大小对齐到页边界
        let total_size = (used_offset + used_size + 4095) & !4095;

        // 分配足够的连续页面
        let pages_needed = (total_size + 4095) / 4096;
        debug!("[VirtQueue] Allocating {} pages for queue", pages_needed);
        let frame_tracker = frame_allocator::alloc_contiguous(pages_needed)?;

        let va = VirtualAddress::from(frame_tracker.ppn.as_usize() * 4096);

        let desc = va.as_usize() as *mut VirtqDesc;
        let avail = (va.as_usize() + avail_offset) as *mut VirtqAvail;
        let used = (va.as_usize() + used_offset) as *mut VirtqUsed;

        // 初始化描述符链
        let mut desc_shadow = Vec::with_capacity(size as usize);
        unsafe {
            for i in 0..size {
                let shadow_desc = VirtqDesc {
                    next: if i == size - 1 { 0 } else { i + 1 },
                    flags: 0,
                    addr: 0,
                    len: 0,
                };
                desc_shadow.push(shadow_desc);

                // Initialize actual descriptors
                (*desc.add(i as usize)).next = shadow_desc.next;
                (*desc.add(i as usize)).flags = shadow_desc.flags;
                (*desc.add(i as usize)).addr = shadow_desc.addr;
                (*desc.add(i as usize)).len = shadow_desc.len;
            }

            // 初始化available ring
            (*avail).flags = AtomicU16::new(0);
            (*avail).idx = AtomicU16::new(0);

            // 初始化used ring
            (*used).flags = AtomicU16::new(0);
            (*used).idx = AtomicU16::new(0);
        }

        debug!("[VirtQueue] Successfully created queue: size={}, num_free={}", size, size);
        Some(VirtQueue {
            size,
            desc,
            avail,
            used,
            free_head: 0,
            num_free: size,
            last_used_idx: 0,
            avail_idx: 0,
            queue_token,
            desc_shadow,
            _frame_tracker: frame_tracker,
            stats: VirtQueueStats::default(),
            creation_time: 0, // 简化实现，后续可以对接RTC
        })
    }

    pub fn physical_address(&self) -> PhysicalAddress {
        // 简单地返回虚拟地址对应的物理地址
        let va = VirtualAddress::from(self.desc as usize);

        // 详细调试虚拟地址到物理地址的转换
        let vpn = va.floor();
        let kernel_space = crate::memory::KERNEL_SPACE.wait().lock();
        let pte = kernel_space
            .translate(vpn)
            .expect("Failed to translate virtual address to physical address");
        let pa = PhysicalAddress::from(pte.ppn()).as_usize() + va.page_offset();

        PhysicalAddress::from(pa)
    }

    // Write descriptor from shadow to actual - inspired by virtio-drivers
    fn write_desc(&mut self, index: u16) {
        let index = index as usize;
        unsafe {
            (*self.desc.add(index)) = self.desc_shadow[index];
        }
    }

    // Simple HAL-like buffer sharing following virtio-drivers pattern
    fn share_buffer(&self, buf: &[u8]) -> u64 {
        let va = VirtualAddress::from(buf.as_ptr() as usize);
        // 使用内核页表进行虚拟地址到物理地址的转换
        let vpn = va.floor();
        let kernel_space = crate::memory::KERNEL_SPACE.wait().lock();
        let pte = kernel_space
            .translate(vpn)
            .expect("Failed to translate virtual address to physical address");
        let pa = PhysicalAddress::from(pte.ppn()).as_usize() + va.page_offset();

        pa as u64
    }

    fn share_buffer_mut(&self, buf: &mut [u8]) -> u64 {
        let va = VirtualAddress::from(buf.as_ptr() as usize);
        // 使用内核页表进行虚拟地址到物理地址的转换
        let vpn = va.floor();
        let kernel_space = crate::memory::KERNEL_SPACE.wait().lock();
        let pte = kernel_space
            .translate(vpn)
            .expect("Failed to translate virtual address to physical address");
        let pa = PhysicalAddress::from(pte.ppn()).as_usize() + va.page_offset();

        pa as u64
    }

    pub fn add_buffer(&mut self, inputs: &[&[u8]], outputs: &mut [&mut [u8]]) -> Option<u16> {
        let total_needed = (inputs.len() + outputs.len()) as u16;
        if total_needed == 0 {
            return None;
        }

        if self.num_free < total_needed {
            error!("[VIRTIO_QUEUE] Not enough free descriptors: need {}, have {}",
                   total_needed, self.num_free);
            return None;
        }

        let head = self.free_head;
        let mut desc_idx = head;
        let outputs_len = outputs.len(); // Store length to avoid borrowing issues

        // 添加输入缓冲区 - update shadow first, then write to actual
        for (i, input) in inputs.iter().enumerate() {
            let addr = self.share_buffer(input);
            let len = input.len() as u32;
            let flags = if i == inputs.len() - 1 && outputs_len == 0 {
                0
            } else {
                VIRTQ_DESC_F_NEXT
            };

            let desc = &mut self.desc_shadow[desc_idx as usize];
            desc.addr = addr;
            desc.len = len;
            desc.flags = flags;

            let next_idx = desc.next;
            self.write_desc(desc_idx); // Write to actual descriptor

            if i < inputs.len() - 1 || outputs_len > 0 {
                desc_idx = next_idx;
            }
        }

        // 添加输出缓冲区 - update shadow first, then write to actual
        for (i, output) in outputs.iter_mut().enumerate() {
            let addr = self.share_buffer_mut(output);
            let len = output.len() as u32;
            let flags = VIRTQ_DESC_F_WRITE
                | if i == outputs_len - 1 {
                    0
                } else {
                    VIRTQ_DESC_F_NEXT
                };

            let desc = &mut self.desc_shadow[desc_idx as usize];
            desc.addr = addr;
            desc.len = len;
            desc.flags = flags;

            let next_idx = desc.next;
            self.write_desc(desc_idx); // Write to actual descriptor

            if i < outputs_len - 1 {
                desc_idx = next_idx;
            }
        }

        // 更新free_head
        self.free_head = self.desc_shadow[desc_idx as usize].next;
        self.num_free -= (inputs.len() + outputs_len) as u16;

        Some(head)
    }

    pub fn add_to_avail(&mut self, desc_idx: u16) {
        // Update available ring following virtio-drivers pattern
        let avail_slot = self.avail_idx & (self.size - 1);

        unsafe {
            let ring_ptr = (self.avail as *mut u16).add(2 + avail_slot as usize);
            *ring_ptr = desc_idx;
        }

        // Write barrier: ensure descriptor table updates are visible before avail ring update
        fence(Ordering::SeqCst);

        // Increment avail index - this makes the descriptor available to device
        self.avail_idx = self.avail_idx.wrapping_add(1);
        unsafe {
            (*self.avail).idx.store(self.avail_idx, Ordering::Release);
        }
    }

    pub fn used(&mut self) -> Option<(u16, u32)> {
        unsafe {
            let used_idx = (*self.used).idx.load(Ordering::Acquire);

            if self.last_used_idx == used_idx {
                return None;
            }

            let ring_slot = self.last_used_idx & (self.size - 1);

            // 正确计算used ring元素的位置
            // VirtqUsed结构: flags(2字节) + idx(2字节) + ring[]
            // 但是ring是VirtqUsedElem数组，每个元素8字节
            let ring_base = (self.used as *const u8).add(4) as *const VirtqUsedElem;
            let used_elem = *ring_base.add(ring_slot as usize);

            // Validate descriptor ID before processing
            if used_elem.id >= self.size as u32 {
                error!(
                    "VirtIO queue: invalid descriptor ID {} (queue size: {})",
                    used_elem.id, self.size
                );
                // Skip this element and continue
                self.last_used_idx = self.last_used_idx.wrapping_add(1);
                return None;
            }

            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            // 释放描述符
            self.recycle_descriptors(used_elem.id as u16);

            Some((used_elem.id as u16, used_elem.len))
        }
    }

    fn recycle_descriptors(&mut self, head: u16) {
        let mut desc_idx = head;

        // Validate descriptor index bounds
        if desc_idx >= self.size {
            error!(
                "VirtIO queue: invalid descriptor index {} (queue size: {})",
                desc_idx, self.size
            );
            return;
        }

        loop {
            let desc = &mut self.desc_shadow[desc_idx as usize];
            let next = desc.next;

            // Clear the descriptor in shadow
            desc.addr = 0;
            desc.len = 0;
            let has_next = desc.flags & VIRTQ_DESC_F_NEXT != 0;
            desc.flags = 0;

            // Add to free list
            desc.next = self.free_head;
            self.free_head = desc_idx;
            self.num_free += 1;

            // Write updated descriptor to actual
            self.write_desc(desc_idx);

            if !has_next {
                break;
            }

            // Validate next descriptor index bounds
            if next >= self.size {
                error!(
                    "VirtIO queue: invalid next descriptor index {} (queue size: {})",
                    next, self.size
                );
                break;
            }

            desc_idx = next;
        }
    }

    // 强制回收描述符，用于超时等异常情况
    pub fn recycle_descriptors_force(&mut self, head: u16) {
        // debug!("[VIRTIO_QUEUE] Force recycling descriptors starting from {}", head);
        self.recycle_descriptors(head);
        // debug!("[VIRTIO_QUEUE] After force recycle: {} free descriptors", self.num_free);
    }

    /// 获取队列统计信息
    pub fn get_stats(&self) -> &VirtQueueStats {
        &self.stats
    }

    /// 重置队列统计信息
    pub fn reset_stats(&mut self) {
        self.stats = VirtQueueStats::default();
    }

    /// 获取队列健康状态
    pub fn health_check(&self) -> Result<(), VirtQueueError> {
        // 检查基本队列状态
        if self.num_free > self.size {
            error!("[VirtQueue] Invalid state: num_free ({}) > size ({})", 
                   self.num_free, self.size);
            return Err(VirtQueueError::InvalidDescriptor);
        }

        // 检查可用描述符数量是否合理
        if self.num_free == 0 && self.stats.total_requests > 0 {
            warn!("[VirtQueue] No free descriptors available, may indicate resource leak");
        }

        // 检查完成率
        let completion_rate = if self.stats.total_requests > 0 {
            (self.stats.completed_requests * 100) / self.stats.total_requests
        } else {
            100
        };

        if completion_rate < 95 && self.stats.total_requests > 10 {
            warn!("[VirtQueue] Low completion rate: {}%", completion_rate);
        }

        Ok(())
    }

    /// 更新统计信息 - 请求开始
    pub fn record_request_start(&mut self, bytes: usize) {
        self.stats.total_requests += 1;
        self.stats.bytes_transferred += bytes as u64;
        
        if self.num_free == 0 {
            self.stats.queue_full_events += 1;
        }
    }

    /// 更新统计信息 - 请求完成
    pub fn record_request_completion(&mut self, success: bool) {
        if success {
            self.stats.completed_requests += 1;
        } else {
            self.stats.failed_requests += 1;
        }
    }

    /// 获取队列使用率百分比
    pub fn utilization_percent(&self) -> u16 {
        ((self.size - self.num_free) * 100) / self.size
    }

    /// 获取队列年龄（自创建以来的时间）
    pub fn age_ms(&self) -> u64 {
        // 简化实现，返回固定值
        // 在实际系统中应该计算当前时间与 creation_time 的差值
        1000
    }

    /// 获取队列信息用于调试
    pub fn debug_info(&self) -> alloc::string::String {
        use alloc::format;
        format!("VirtQueue[token={}]: size={}, free={}, utilization={}%, requests={}/{}, errors={}",
                self.queue_token, 
                self.size, 
                self.num_free,
                self.utilization_percent(),
                self.stats.completed_requests,
                self.stats.total_requests,
                self.stats.failed_requests)
    }

    /// 检查队列是否接近满载
    pub fn is_nearly_full(&self) -> bool {
        self.utilization_percent() > 80
    }

    /// 检查队列是否为空
    pub fn is_empty(&self) -> bool {
        self.num_free == self.size
    }

    /// 检查队列是否完全满载
    pub fn is_full(&self) -> bool {
        self.num_free == 0
    }

    /// 获取可用描述符数量占比
    pub fn free_descriptor_ratio(&self) -> f32 {
        (self.num_free as f32) / (self.size as f32)
    }
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}
