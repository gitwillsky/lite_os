use crate::memory::{
    FrameAllocationClass, FrameTracker, PAGE_SIZE, PhysicalAddress, VirtualAddress,
    alloc_contiguous,
};
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU16, Ordering, fence};

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
    // 队列共享内存的物理基地址与各段偏移，供MMIO寄存器编程
    mem_paddr: PhysicalAddress,
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
            mem_paddr: base_pa,
        })
    }

    pub(super) fn physical_address(&self) -> PhysicalAddress {
        self.mem_paddr
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
    fn append_pa_segments(&self, segments: &mut Vec<(u64, u32)>, ptr: *const u8, len: usize) {
        let mut processed: usize = 0;
        while processed < len {
            let cur_va = VirtualAddress::from(ptr as usize + processed);
            let page_off = cur_va.page_offset();
            let remain = len - processed;
            let to_page_end = PAGE_SIZE - page_off;
            let chunk = core::cmp::min(remain, to_page_end);

            let pa = {
                let kernel_space = crate::memory::KERNEL_SPACE.wait().lock();
                match kernel_space.translate_kernel_address(cur_va) {
                    Some(pa) => pa,
                    None => panic!("VirtQueue: failed to translate VA {:#x}", cur_va.as_usize()),
                }
            };
            let addr = pa.as_usize() as u64;
            segments.push((addr, chunk as u32));
            processed += chunk;
        }
    }

    pub(super) fn add_buffer(
        &mut self,
        inputs: &[&[u8]],
        outputs: &mut [&mut [u8]],
    ) -> Option<u16> {
        // 1. 预计算分段数并完成唯一可失败分配；缺失时 TX/RX
        //    runtime 会在 Vec::push 中进入 kernel-wide allocation abort。
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
        let mut in_segs = Vec::new();
        in_segs.try_reserve_exact(input_count).ok()?;
        let mut out_segs = Vec::new();
        out_segs.try_reserve_exact(output_count).ok()?;

        for input in inputs.iter() {
            self.append_pa_segments(&mut in_segs, input.as_ptr(), input.len());
        }
        for output in outputs.iter_mut() {
            self.append_pa_segments(&mut out_segs, output.as_ptr(), output.len());
        }

        // 2. total_count 已不大于 u16 num_free，转换不会截断。
        let total_needed = total_count as u16;

        let head = self.free_head;
        let mut desc_idx = head;

        // 填充输入段
        for (seg_i, (addr, len)) in in_segs.iter().enumerate() {
            let is_last_in = seg_i == in_segs.len() - 1;
            let mut flags: u16 = 0;
            if !(is_last_in && out_segs.is_empty()) {
                flags |= VIRTQ_DESC_F_NEXT;
            }

            let desc = &mut self.desc_shadow[desc_idx as usize];
            desc.addr = *addr;
            desc.len = *len;
            desc.flags = flags;

            let next_idx = desc.next;
            self.write_desc(desc_idx);
            if !is_last_in || !out_segs.is_empty() {
                desc_idx = next_idx;
            }
        }

        // 填充输出段
        for (seg_i, (addr, len)) in out_segs.iter().enumerate() {
            let is_last_out = seg_i == out_segs.len() - 1;
            let mut flags: u16 = VIRTQ_DESC_F_WRITE;
            if !is_last_out {
                flags |= VIRTQ_DESC_F_NEXT;
            }

            let desc = &mut self.desc_shadow[desc_idx as usize];
            desc.addr = *addr;
            desc.len = *len;
            desc.flags = flags;

            let next_idx = desc.next;
            self.write_desc(desc_idx);
            if !is_last_out {
                desc_idx = next_idx;
            }
        }

        // 更新free_head与计数
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

        // Write barrier: ensure descriptor table updates are visible before avail ring update
        fence(Ordering::SeqCst);

        // Increment avail index - this makes the descriptor available to device
        self.avail_idx = self.avail_idx.wrapping_add(1);
        // SAFETY: `avail` points into the queue pages retained by `_frame_tracker`; producer
        // access is serialized by `&mut self`, and Release publishes prior descriptor writes.
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
