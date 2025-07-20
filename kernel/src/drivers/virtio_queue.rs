use crate::memory::{
    address::{PhysicalAddress, VirtualAddress},
    frame_allocator::{self, FrameTracker},
};
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
}

impl VirtQueue {
    pub fn new(size: u16, queue_token: usize) -> Option<Self> {
        if size == 0 || size & (size - 1) != 0 {
            return None; // 队列大小必须是2的幂
        }

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

        // debug!(
        //     "[VirtQueue] physical_address: desc VA={:#x}, VPN={:#x}, PPN={:#x}, PA={:#x}",
        //     va.as_usize(),
        //     vpn.as_usize(),
        //     pte.ppn().as_usize(),
        //     pa
        // );

        // 验证物理地址的可访问性
        // debug!(
        //     "[VirtQueue] queue layout: desc={:#x}, avail={:#x}, used={:#x}",
        //     self.desc as usize, self.avail as usize, self.used as usize
        // );

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

        // debug!(
        //     "[VirtQueue] share_buffer: VA={:#x} -> PA={:#x}",
        //     va.as_usize(),
        //     pa
        // );
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

        // debug!(
        //     "[VirtQueue] share_buffer_mut: VA={:#x} -> PA={:#x}",
        //     va.as_usize(),
        //     pa
        // );
        pa as u64
    }

    pub fn add_buffer(&mut self, inputs: &[&[u8]], outputs: &mut [&mut [u8]]) -> Option<u16> {
        if self.num_free < (inputs.len() + outputs.len()) as u16 {
            error!("[VIRTIO_QUEUE] Not enough free descriptors: need {}, have {}",
                   inputs.len() + outputs.len(), self.num_free);
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
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}
