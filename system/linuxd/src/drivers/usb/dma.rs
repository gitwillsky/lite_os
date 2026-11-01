//! xHCI DMA 存储；controller 的永久注册引用保活它，reset/Disable Slot 完成之前不得释放。
use super::{Error, Result};
use crate::memory::{DeviceBacking, DeviceMemoryKind, FrameAllocationClass, PAGE_SIZE};
use core::sync::atomic::Ordering;

pub(super) struct Dma {
    backing: DeviceBacking,
    pub(super) physical: u64,
    pub(super) bytes: usize,
}
impl Dma {
    pub(super) fn allocate(pages: usize) -> Result<Self> {
        let backing = DeviceBacking::try_allocate(
            pages,
            FrameAllocationClass::Reclaimable,
            DeviceMemoryKind::Driver,
        )
        .ok_or(Error::Memory)?;
        if backing.extent_count() != 1 {
            return Err(Error::Memory);
        }
        let physical = (backing.page(0).ok_or(Error::Memory)?.as_usize() * PAGE_SIZE) as u64;
        let bytes = pages * PAGE_SIZE;
        let dma = Self {
            backing,
            physical,
            bytes,
        };
        for offset in (0..bytes).step_by(PAGE_SIZE) {
            dma.write(offset, &[0; PAGE_SIZE])?;
        }
        Ok(dma)
    }
    pub(super) fn write(&self, offset: usize, bytes: &[u8]) -> Result<()> {
        self.backing
            .write(offset, bytes)
            .map_err(|()| Error::Protocol)
    }
    pub(super) fn read(&self, offset: usize, bytes: &mut [u8]) -> Result<()> {
        self.backing
            .read(offset, bytes)
            .map_err(|()| Error::Protocol)
    }
    pub(super) fn word(&self, offset: usize) -> Result<u32> {
        self.backing
            .atomic_u32(offset)
            .map(|word| word.load(Ordering::Acquire))
            .ok_or(Error::Protocol)
    }
    pub(super) fn set_word(&self, offset: usize, value: u32) -> Result<()> {
        self.backing
            .atomic_u32(offset)
            .ok_or(Error::Protocol)?
            .store(value, Ordering::Release);
        Ok(())
    }
    pub(super) fn pointer(&self, offset: usize, value: u64) -> Result<()> {
        self.write(offset, &value.to_le_bytes())
    }
}
#[derive(Clone, Copy, Default)]
pub(super) struct Trb(pub(super) [u32; 4]);
impl Trb {
    pub(super) fn new(pointer: u64, status: u32, control: u32) -> Self {
        Self([pointer as u32, (pointer >> 32) as u32, status, control])
    }
    pub(super) fn address(self) -> u64 {
        u64::from(self.0[0]) | u64::from(self.0[1]) << 32
    }
    pub(super) fn kind(self) -> u32 {
        self.0[3] >> 10 & 63
    }
    pub(super) fn code(self) -> u32 {
        self.0[2] >> 24
    }
    pub(super) fn slot(self) -> u8 {
        (self.0[3] >> 24) as u8
    }
    pub(super) fn endpoint(self) -> u8 {
        (self.0[3] >> 16 & 31) as u8
    }
}
pub(super) struct Ring {
    pub(super) memory: Dma,
    next: usize,
    head: usize,
    used: usize,
    cycle: bool,
}
impl Ring {
    pub(super) fn new(pages: usize) -> Result<Self> {
        Ok(Self {
            memory: Dma::allocate(pages)?,
            next: 0,
            head: 0,
            used: 0,
            cycle: true,
        })
    }
    pub(super) fn free(&self) -> usize {
        self.memory.bytes / 16 - 1 - self.used
    }
    pub(super) fn push(&mut self, mut trb: Trb) -> Result<u64> {
        if self.free() == 0 {
            return Err(Error::Busy);
        }
        let index = self.next;
        trb.0[3] = (trb.0[3] & !1) | u32::from(self.cycle);
        for (n, word) in trb.0.iter().copied().enumerate().take(3) {
            self.memory.set_word(index * 16 + n * 4, word)?;
        }
        self.memory.set_word(index * 16 + 12, trb.0[3])?;
        self.next += 1;
        self.used += 1;
        if self.next == self.memory.bytes / 16 - 1 {
            let link = Trb::new(self.memory.physical, 0, 6 << 10 | 2 | u32::from(self.cycle));
            for (n, word) in link.0.iter().copied().enumerate().take(3) {
                self.memory.set_word(self.next * 16 + n * 4, word)?;
            }
            self.memory.set_word(self.next * 16 + 12, link.0[3])?;
            self.next = 0;
            self.cycle = !self.cycle;
        }
        Ok(self.memory.physical + (index * 16) as u64)
    }
    pub(super) fn complete(&mut self, address: u64) -> Result<()> {
        let offset = address
            .checked_sub(self.memory.physical)
            .ok_or(Error::Protocol)? as usize;
        let capacity = self.memory.bytes / 16 - 1;
        if !offset.is_multiple_of(16) || offset / 16 >= capacity {
            return Err(Error::Protocol);
        }
        let index = offset / 16;
        let count = (index + capacity - self.head) % capacity + 1;
        if count > self.used {
            return Err(Error::Protocol);
        }
        self.head = (index + 1) % capacity;
        self.used -= count;
        Ok(())
    }
}
impl Ring {
    /// 仅在 endpoint Stop/Disable completion 后清空 CPU 与硬件共享 ring；调用者随后 Set TR Dequeue。
    pub(super) fn reset(&mut self) -> Result<()> {
        for offset in (0..self.memory.bytes).step_by(64) {
            self.memory.write(offset, &[0; 64])?;
        }
        self.next = 0;
        self.head = 0;
        self.used = 0;
        self.cycle = true;
        Ok(())
    }
}
