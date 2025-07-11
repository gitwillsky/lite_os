use crate::memory::{
    address::{PhysicalAddress, VirtualAddress},
    frame_allocator::FrameTracker,
};

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
    pub flags: u16,
    pub idx: u16,
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
    pub flags: u16,
    pub idx: u16,
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
    _frame_tracker: FrameTracker,
}

impl VirtQueue {
    pub fn new(size: u16, queue_token: usize) -> Option<Self> {
        if size == 0 || size & (size - 1) != 0 {
            return None; // 队列大小必须是2的幂
        }

        // 计算需要的内存大小
        let desc_size = size_of::<VirtqDesc>() * size as usize;
        let avail_size = size_of::<VirtqAvail>() + size_of::<u16>() * size as usize;
        let used_size = size_of::<VirtqUsed>() + size_of::<VirtqUsedElem>() * size as usize;
        
        // 对齐到页边界
        let total_size = (desc_size + avail_size + used_size + 4095) & !4095;
        
        // 分配内存
        let frame_tracker = crate::memory::frame_allocator::alloc()?;
        let va = VirtualAddress::from(frame_tracker.ppn.as_usize() * 4096);
        
        let desc = va.as_usize() as *mut VirtqDesc;
        let avail = (va.as_usize() + desc_size) as *mut VirtqAvail;
        let used = (va.as_usize() + desc_size + avail_size) as *mut VirtqUsed;

        // 初始化描述符链
        unsafe {
            for i in 0..size {
                (*desc.add(i as usize)).next = if i == size - 1 { 0 } else { i + 1 };
            }
            
            // 初始化available ring
            (*avail).flags = 0;
            (*avail).idx = 0;
            
            // 初始化used ring
            (*used).flags = 0;
            (*used).idx = 0;
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
            _frame_tracker: frame_tracker,
        })
    }

    pub fn physical_address(&self) -> PhysicalAddress {
        // 简单地返回虚拟地址对应的物理地址
        let va = VirtualAddress::from(self.desc as usize);
        PhysicalAddress::from(va.as_usize())
    }

    pub fn add_buffer(&mut self, inputs: &[&[u8]], outputs: &[&mut [u8]]) -> Option<u16> {
        if self.num_free < (inputs.len() + outputs.len()) as u16 {
            return None;
        }

        let head = self.free_head;
        let mut desc_idx = head;
        
        // 添加输入缓冲区
        for (i, input) in inputs.iter().enumerate() {
            unsafe {
                let desc = &mut *self.desc.add(desc_idx as usize);
                desc.addr = input.as_ptr() as u64;
                desc.len = input.len() as u32;
                desc.flags = if i == inputs.len() - 1 && outputs.is_empty() { 
                    0 
                } else { 
                    VIRTQ_DESC_F_NEXT 
                };
                if i < inputs.len() - 1 || !outputs.is_empty() {
                    desc_idx = desc.next;
                }
            }
        }

        // 添加输出缓冲区
        for (i, output) in outputs.iter().enumerate() {
            unsafe {
                let desc = &mut *self.desc.add(desc_idx as usize);
                desc.addr = output.as_ptr() as u64;
                desc.len = output.len() as u32;
                desc.flags = VIRTQ_DESC_F_WRITE | if i == outputs.len() - 1 { 
                    0 
                } else { 
                    VIRTQ_DESC_F_NEXT 
                };
                if i < outputs.len() - 1 {
                    desc_idx = desc.next;
                }
            }
        }

        // 更新free_head
        unsafe {
            self.free_head = (*self.desc.add(desc_idx as usize)).next;
        }
        self.num_free -= (inputs.len() + outputs.len()) as u16;

        Some(head)
    }

    pub fn add_to_avail(&mut self, desc_idx: u16) {
        unsafe {
            let ring_ptr = (self.avail as *mut u16).add(2 + self.avail_idx as usize);
            *ring_ptr = desc_idx;
            
            // 内存屏障
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            
            (*self.avail).idx = (*self.avail).idx.wrapping_add(1);
            self.avail_idx = self.avail_idx.wrapping_add(1);
        }
    }

    pub fn get_used(&mut self) -> Option<(u16, u32)> {
        unsafe {
            let used_idx = (*self.used).idx;
            if self.last_used_idx == used_idx {
                return None;
            }

            let ring_ptr = (self.used as *mut VirtqUsedElem).add(2 + (self.last_used_idx as usize % self.size as usize));
            let used_elem = *ring_ptr;
            
            self.last_used_idx = self.last_used_idx.wrapping_add(1);
            
            // 释放描述符
            self.recycle_descriptors(used_elem.id as u16);
            
            Some((used_elem.id as u16, used_elem.len))
        }
    }

    fn recycle_descriptors(&mut self, head: u16) {
        let mut desc_idx = head;
        loop {
            unsafe {
                let desc = &mut *self.desc.add(desc_idx as usize);
                let next = desc.next;
                desc.next = self.free_head;
                self.free_head = desc_idx;
                self.num_free += 1;
                
                if desc.flags & VIRTQ_DESC_F_NEXT == 0 {
                    break;
                }
                desc_idx = next;
            }
        }
    }
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}