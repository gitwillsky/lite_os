use crate::memory::{FrameAllocationClass, FrameTracker, PhysicalAddress, alloc_contiguous};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU16, Ordering};

use super::hal::VirtQueueAddresses;

#[path = "virtio_queue/dma.rs"]
mod dma;
#[cfg_attr(test, allow(unused_imports))]
pub(super) use dma::{DeviceWriteBuffer, DmaBuffer, DmaSlice};
use dma::{DmaChainRequirement, descriptor_requirement};

/// Virtqueue descriptor chain 在 publication 前的可恢复构造错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum VirtQueueError {
    /// buffer 长度溢出或没有可发布的 segment。
    InvalidBuffer,
    /// 当前 free descriptor 不足。
    NoDescriptors,
}

/// used ring 已摘取、但尚未由 concrete adapter owner 验证的 descriptor completion。
///
/// token 不回收 descriptor；adapter 必须先验证 head/generation/slot identity，再把唯一 token
/// 交回 `VirtQueue::recycle_used`。验证失败时只能 reset/fail-stop，队列会保持 pending latch，
/// 从而阻止 duplicate/unknown completion 继续污染 free list。
#[must_use = "used completion must be owner-claimed then recycled, or terminate the queue"]
pub(super) struct UsedDescriptor {
    queue: u64,
    head: u16,
    length: u32,
}

impl UsedDescriptor {
    /// @description 返回 device 声明完成的 descriptor chain head。
    pub(super) fn head(&self) -> u16 {
        self.head
    }

    /// @description 返回 device 声明写入的 completion length。
    pub(super) fn length(&self) -> u32 {
        self.length
    }
}

// VirtIO Ring 描述符标志
pub(super) const VIRTQ_DESC_F_NEXT: u16 = 1;
pub(super) const VIRTQ_DESC_F_WRITE: u16 = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct VirtqDesc {
    pub(super) addr: u64,
    pub(super) len: u32,
    pub(super) flags: u16,
    pub(super) next: u16,
}

#[repr(C)]
#[derive(Debug)]
pub(super) struct VirtqAvail {
    pub(super) flags: AtomicU16,
    pub(super) idx: AtomicU16,
    pub(super) ring: [u16; 0], // 实际大小由队列大小决定
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub(super) struct VirtqUsedElem {
    pub(super) id: u32,
    pub(super) len: u32,
}

#[repr(C)]
#[derive(Debug)]
pub(super) struct VirtqUsed {
    pub(super) flags: AtomicU16,
    pub(super) idx: AtomicU16,
    pub(super) ring: [VirtqUsedElem; 0], // 实际大小由队列大小决定
}

pub(super) struct VirtQueue {
    pub(super) size: u16,
    pub(super) desc: *mut VirtqDesc,
    pub(super) avail: *mut VirtqAvail,
    pub(super) used: *mut VirtqUsed,
    pub(super) free_head: u16,
    pub(super) num_free: u16,
    pub(super) last_used_idx: u16,
    pub(super) avail_idx: u16,
    // OWNER: used ring 每次最多有一个已摘取但尚未由 adapter claim 的 completion。
    // 缺失该 latch 会让 caller 丢弃 invalid token 后继续消费队列并复用未验证 descriptor。
    pending_used: Option<u16>,
    // OWNER: ring/token/chain corruption 后永久关闭本 queue；reset 是唯一退出策略。
    failed: bool,
    // Shadow descriptors that device can't access - inspired by virtio-drivers
    desc_shadow: Vec<VirtqDesc>,
    _frame_tracker: FrameTracker,
    addresses: VirtQueueAddresses,
}

impl VirtQueue {
    pub(super) fn new(size: u16) -> Option<Self> {
        if size == 0 || size & (size - 1) != 0 {
            error!(
                "[VirtQueue] Invalid queue size: {} (must be power of 2)",
                size
            );
            return None; // 队列大小必须是2的幂
        }

        debug!("[VirtQueue] Creating queue with size={}", size);

        let mut desc_shadow = Vec::new();
        desc_shadow.try_reserve_exact(size as usize).ok()?;

        // 计算需要的内存大小 - 严格按照VirtIO规范进行对齐
        let desc_size = size_of::<VirtqDesc>() * size as usize;

        // Available ring: flags(2) + idx(2) + ring[size](2*size) + used_event(2)
        let avail_size = 2 + 2 + 2 * size as usize + 2;
        let avail_offset = desc_size;

        // Legacy 设备要求 used ring 按 queue_align 对齐；统一按 4096 对齐，兼容性更好
        let queue_align: usize = 4096;
        let used_offset = (avail_offset + avail_size + (queue_align - 1)) & !(queue_align - 1);
        // Used ring: flags(2) + idx(2) + ring[size](8*size) + avail_event(2)
        let used_size = 2 + 2 + 8 * size as usize + 2;

        // 总大小对齐到页边界
        let total_size = (used_offset + used_size + 4095) & !4095;

        // 分配足够的连续页面
        let pages_needed = total_size.div_ceil(4096);
        debug!("[VirtQueue] Allocating {} pages for queue", pages_needed);
        let frame_tracker = alloc_contiguous(pages_needed, FrameAllocationClass::KernelCritical)?;

        let base_pa = PhysicalAddress::from(frame_tracker.ppn.as_usize() * 4096);
        let base_va = base_pa.as_mut_ptr::<u8>() as usize;

        let desc = base_va as *mut VirtqDesc;
        let avail = (base_va + avail_offset) as *mut VirtqAvail;
        let used = (base_va + used_offset) as *mut VirtqUsed;

        // SAFETY: 连续帧数按 `total_size` 向上取整，desc/avail/used 的偏移均在该区间内；
        // frame tracker 在队列生命周期内保持页存活，初始化期间设备尚未获得 PFN，不存在并发 DMA。
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

        debug!(
            "[VirtQueue] Successfully created queue: size={}, num_free={}",
            size, size
        );
        Some(VirtQueue {
            size,
            desc,
            avail,
            used,
            free_head: 0,
            num_free: size,
            last_used_idx: 0,
            avail_idx: 0,
            pending_used: None,
            failed: false,
            desc_shadow,
            _frame_tracker: frame_tracker,
            addresses: VirtQueueAddresses {
                descriptor: base_pa.as_usize() as u64,
                driver: (base_pa.as_usize() + avail_offset) as u64,
                device: (base_pa.as_usize() + used_offset) as u64,
            },
        })
    }

    /// @description 返回 MMIO v2 queue publication 所需的三段物理地址。
    ///
    /// @return descriptor、available 与 used ring 的稳定物理基址。
    pub(super) fn addresses(&self) -> VirtQueueAddresses {
        self.addresses
    }

    /// @description 返回尚未被 driver-owned descriptor chain 占用的 entry 数量。
    /// @return 外层 queue lock 下稳定的 free-list 容量。
    pub(super) fn free_descriptor_count(&self) -> u16 {
        self.num_free
    }

    // Write descriptor from shadow to actual - inspired by virtio-drivers
    fn write_desc(&mut self, index: u16) {
        let index = index as usize;
        // SAFETY: 所有调用者的 index 来自长度为 `size` 的 free chain，
        // desc 指向 `_frame_tracker` 保持存活的描述符表。
        unsafe {
            (*self.desc.add(index)) = self.desc_shadow[index];
        }
    }

    fn write_segments(
        &mut self,
        buffer: &DmaSlice<'_>,
        descriptor: &mut u16,
        remaining: &mut usize,
    ) {
        buffer.for_each_segment(|physical, length, writable| {
            let current = *descriptor;
            let next = self.desc_shadow[current as usize].next;
            *remaining -= 1;
            let desc = &mut self.desc_shadow[current as usize];
            desc.addr = physical;
            desc.len = length as u32;
            desc.flags = if writable { VIRTQ_DESC_F_WRITE } else { 0 }
                | if *remaining != 0 {
                    VIRTQ_DESC_F_NEXT
                } else {
                    0
                };
            self.write_desc(current);
            if *remaining != 0 {
                *descriptor = next;
            }
        });
    }

    pub(super) fn add_dma(&mut self, buffers: &[DmaSlice<'_>]) -> Result<u16, VirtQueueError> {
        let total_count = match descriptor_requirement(buffers, usize::from(self.num_free)) {
            DmaChainRequirement::Required(count) => count,
            DmaChainRequirement::Empty => return Err(VirtQueueError::InvalidBuffer),
            DmaChainRequirement::ExceedsCapacity => {
                return Err(VirtQueueError::NoDescriptors);
            }
        };

        let total_needed = total_count as u16;
        let head = self.free_head;
        let mut desc_idx = head;
        let mut remaining = total_count;
        for buffer in buffers {
            self.write_segments(buffer, &mut desc_idx, &mut remaining);
        }
        assert_eq!(remaining, 0, "VirtIO segment count diverged from fill");
        self.free_head = self.desc_shadow[desc_idx as usize].next;
        self.num_free -= total_needed;

        Ok(head)
    }

    pub(super) fn add_to_avail(&mut self, desc_idx: u16) {
        // Update available ring following virtio-drivers pattern
        let avail_slot = self.avail_idx & (self.size - 1);

        // SAFETY: size 在创建时已验证为 2 的幂，因此 slot < size；
        // avail ring 指向已分配共享页，外层 Mutex 串行化 producer 写入。
        unsafe {
            let ring_ptr = (self.avail as *mut u16).add(2 + avail_slot as usize);
            *ring_ptr = desc_idx;
        }

        // Increment avail index - this makes the descriptor available to device
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: `avail` points into the queue pages retained by `_frame_tracker`; producer
        // access is serialized by `&mut self`.  The Release store is the VirtIO-required barrier:
        // it publishes the descriptor table and ring slot before exposing the new index.
        unsafe {
            (*self.avail).idx.store(self.avail_idx, Ordering::Release);
        }
    }

    /// @description 从 used ring 摘取一个尚未回收的 completion token。
    ///
    /// @return 无 completion 时为 `None`；成功 token 只暴露 head/length，不改变 free list。
    /// @errors ring identity 越界，或上一个 token 未合法回收时返回错误；caller 必须 reset。
    pub(super) fn used(&mut self) -> Result<Option<UsedDescriptor>, ()> {
        if self.failed || self.pending_used.is_some() {
            return Err(());
        }
        // SAFETY: used ring 完整位于 `_frame_tracker` 保持存活的共享页内；
        // Acquire 读 used.idx 后才读取对应 ring slot，slot 通过 power-of-two size 限制。
        unsafe {
            let used_idx = (*self.used).idx.load(Ordering::Acquire);

            if self.last_used_idx == used_idx {
                return Ok(None);
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
                self.last_used_idx = self.last_used_idx.wrapping_add(1);
                self.failed = true;
                return Err(());
            }

            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            let head = used_elem.id as u16;
            self.pending_used = Some(head);
            Ok(Some(UsedDescriptor {
                queue: self.addresses.descriptor,
                head,
                length: used_elem.len,
            }))
        }
    }

    /// @description 在 concrete adapter 已 exactly-once claim completion 后回收 descriptor。
    ///
    /// @param completion 当前 queue 的唯一 pending token。
    /// @return chain 完整回到 free list时成功。
    /// @errors token 不属于本 queue、不是当前 pending head 或 chain 损坏时返回错误；队列保持
    /// terminal pending 状态，caller 必须 reset，禁止重试或局部回收。
    pub(super) fn recycle_used(&mut self, completion: UsedDescriptor) -> Result<(), ()> {
        if completion.queue != self.addresses.descriptor
            || self.pending_used != Some(completion.head)
        {
            self.failed = true;
            return Err(());
        }
        if self.recycle_descriptors(completion.head).is_err() {
            self.failed = true;
            return Err(());
        }
        self.pending_used = None;
        Ok(())
    }

    /// @description 非破坏性检查 used ring 是否尚有未回收 completion。
    ///
    /// @return device 发布的 used index 领先当前 consumer 时返回 `true`。
    pub(super) fn has_used(&self) -> bool {
        // SAFETY: used ring 在 `_frame_tracker` 生命周期内有效；Acquire 与 device 的
        // used publication 配对，本方法不读取 ring payload。
        unsafe { (*self.used).idx.load(Ordering::Acquire) != self.last_used_idx }
    }

    fn recycle_descriptors(&mut self, head: u16) -> Result<(), ()> {
        let mut desc_idx = head;
        let mut recycled = 0u16;

        // Validate descriptor index bounds
        if desc_idx >= self.size {
            error!(
                "VirtIO queue: invalid descriptor index {} (queue size: {})",
                desc_idx, self.size
            );
            return Err(());
        }

        loop {
            if recycled >= self.size || self.num_free >= self.size {
                return Err(());
            }
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
            recycled += 1;

            // Write updated descriptor to actual
            self.write_desc(desc_idx);

            if !has_next {
                return Ok(());
            }

            // Validate next descriptor index bounds
            if next >= self.size {
                error!(
                    "VirtIO queue: invalid next descriptor index {} (queue size: {})",
                    next, self.size
                );
                return Err(());
            }

            desc_idx = next;
        }
    }

    /// @description 回收尚未发布到 available ring 的 descriptor chain。
    ///
    /// @param head `add_dma` 返回、但 adapter validation 拒绝发布的 chain head。
    /// @return chain 完整回到 free list 时成功；queue ownership 已损坏时返回错误。
    pub(super) fn retire_unpublished(&mut self, head: u16) -> Result<(), ()> {
        self.recycle_descriptors(head)
    }
}

// SAFETY: 队列指针都指向 `_frame_tracker` 独占且在对象销毁前有效的连续页；
// 所有可变队列操作都要求 `&mut self`，设备实例又使用 `Mutex<VirtQueue>` 串行化访问。
unsafe impl Send for VirtQueue {}

#[cfg(test)]
#[path = "virtio_queue/virtio_queue_tests.rs"]
mod queue_tests;
