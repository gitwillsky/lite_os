use crate::memory::{
    FrameAllocationClass, FrameTracker, KERNEL_SPACE, MemorySet, PAGE_SIZE, PhysicalAddress,
    VirtualAddress, alloc_contiguous,
};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU16, Ordering};

use super::hal::VirtQueueAddresses;

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
        let va = VirtualAddress::from(frame_tracker.ppn.as_usize() * 4096);

        let desc = va.as_usize() as *mut VirtqDesc;
        let avail = (va.as_usize() + avail_offset) as *mut VirtqAvail;
        let used = (va.as_usize() + used_offset) as *mut VirtqUsed;

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

    fn segment_count(ptr: *const u8, len: usize) -> Option<usize> {
        if len == 0 {
            return Some(0);
        }
        (ptr as usize % PAGE_SIZE)
            .checked_add(len)
            .map(|span| span.div_ceil(PAGE_SIZE))
    }

    // Simple HAL-like buffer sharing following virtio-drivers pattern
    fn write_segments(
        &mut self,
        kernel_space: &MemorySet,
        ptr: *const u8,
        len: usize,
        writable: bool,
        descriptor: &mut u16,
        remaining: &mut usize,
    ) {
        let mut processed: usize = 0;
        while processed < len {
            let cur_va = VirtualAddress::from(ptr as usize + processed);
            let page_off = cur_va.page_offset();
            let remain = len - processed;
            let to_page_end = PAGE_SIZE - page_off;
            let chunk = core::cmp::min(remain, to_page_end);

            let pa = match kernel_space.translate_kernel_address(cur_va) {
                Some(pa) => pa,
                None => panic!("VirtQueue: failed to translate VA {:#x}", cur_va.as_usize()),
            };
            let current = *descriptor;
            let next = self.desc_shadow[current as usize].next;
            *remaining -= 1;
            let desc = &mut self.desc_shadow[current as usize];
            desc.addr = pa.as_usize() as u64;
            desc.len = chunk as u32;
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
            processed += chunk;
        }
    }

    pub(super) fn add_buffer(
        &mut self,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> Option<u16> {
        // 1. 预计算全部物理分段，只在容量充足时开始修改 free chain。
        //    若在 runtime 构造临时 Vec，网络和块 I/O 的每次提交都会分配，并在
        //    memory pressure 下把本可返回的设备错误放大为 kernel-wide allocation abort。
        let input_count = inputs.iter().try_fold(0usize, |count, input| {
            count.checked_add(Self::segment_count(input.as_ptr(), input.len())?)
        })?;
        let output_count = outputs.iter().try_fold(0usize, |count, output| {
            count.checked_add(Self::segment_count(output.as_ptr(), output.len())?)
        })?;
        let total_count = input_count.checked_add(output_count)?;
        if total_count == 0 || total_count > usize::from(self.num_free) {
            return None;
        }

        // 2. total_count 已不大于 u16 num_free；直接以固定局部状态遍历
        //    buffer，避免为每次 descriptor submission 构造 heap collection。
        let total_needed = total_count as u16;
        let head = self.free_head;
        let mut desc_idx = head;
        let mut remaining = total_count;
        // Buffer owners keep their mappings live through completion; this lock only stabilizes
        // page-table traversal while the chain is translated.  Take it once for the whole chain
        // instead of once per physical segment: block, network and display buffers commonly span
        // several pages, and repeated spin-lock traffic provided no stronger lifetime proof.
        let kernel_space = KERNEL_SPACE.wait().lock();
        for input in inputs {
            self.write_segments(
                &kernel_space,
                input.as_ptr(),
                input.len(),
                false,
                &mut desc_idx,
                &mut remaining,
            );
        }
        for output in outputs {
            self.write_segments(
                &kernel_space,
                output.as_ptr(),
                output.len(),
                true,
                &mut desc_idx,
                &mut remaining,
            );
        }
        assert_eq!(remaining, 0, "VirtIO segment count diverged from fill");
        drop(kernel_space);

        // 3. 全部 descriptor 已完整写入，最后一次提交 free-list owner。
        self.free_head = self.desc_shadow[desc_idx as usize].next;
        self.num_free -= total_needed;

        Some(head)
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

    pub(super) fn used(&mut self) -> Result<Option<(u16, u32)>, ()> {
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
                return Err(());
            }

            self.last_used_idx = self.last_used_idx.wrapping_add(1);

            // 释放描述符
            self.recycle_descriptors(used_elem.id as u16)?;

            Ok(Some((used_elem.id as u16, used_elem.len)))
        }
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
}

// SAFETY: 队列指针都指向 `_frame_tracker` 独占且在对象销毁前有效的连续页；
// 所有可变队列操作都要求 `&mut self`，设备实例又使用 `Mutex<VirtQueue>` 串行化访问。
unsafe impl Send for VirtQueue {}
