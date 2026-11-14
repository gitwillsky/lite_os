//! socket 的固定共享 SQ/CQ 与载荷槽；两端只对共享存储作同宽原子访问。
//! metadata 的 Release/Acquire 交接与载荷槽寿命分开：CQ 被领取不代表调用者已经复制完载荷。
use super::*;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use device_model::DeviceBacking;

pub(super) const MAGIC: u64 = u64::from_le_bytes(*b"LSOCK006");
pub(super) const CAPACITY: usize = 8;
const _: () = assert!(CAPACITY.is_power_of_two() && CAPACITY <= u32::BITS as usize);
pub(super) const WORDS: usize = 16;
pub(super) type Record = [u64; WORDS];
const HEADER: usize = 256;
const SQ_HEAD: usize = 0;
const SQ_TAIL: usize = 64;
const CQ_HEAD: usize = 128;
const CQ_TAIL: usize = 192;
const CLOSED: usize = 8;
const ABORT: usize = 12;
const VERSION: usize = 16;
const SQ: usize = HEADER;
const CQ: usize = SQ + CAPACITY * WORDS * 8;
const PAYLOAD: usize = (CQ + CAPACITY * WORDS * 8).next_multiple_of(native_abi::PAGE_SIZE);
const BYTES: usize = PAYLOAD + CAPACITY * TRANSFER_LIMIT;

pub(super) struct Queue {
    backing: DeviceBacking,
    base: usize,
}
impl Queue {
    pub(super) fn create() -> Result<Self, Status> {
        let queue = Self::map(Vmo::create(BYTES as u64, 0)?)?;
        queue.word(VERSION).store(MAGIC.to_le(), Ordering::Release);
        Ok(queue)
    }
    pub(super) fn open(vmo: Vmo) -> Result<Self, Status> {
        let queue = Self::map(vmo)?;
        if u64::from_le(queue.word(VERSION).load(Ordering::Acquire)) != MAGIC {
            return Err(native_abi::ERR_NOT_SUPPORTED);
        }
        Ok(queue)
    }
    fn map(vmo: Vmo) -> Result<Self, Status> {
        if vmo.size()? != BYTES as u64 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let backing = DeviceBacking::from_vmo(vmo)?;
        let base = backing.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        Ok(Self { backing, base })
    }
    pub(super) fn vmo(&self) -> &Vmo {
        self.backing.vmo()
    }
    fn counter(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: 固定偏移 4 字节对齐、处于首映射页，只作 AtomicU32 访问且不与 u64 区间重叠。
        unsafe { &*((self.base + offset) as *const AtomicU32) }
    }
    fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: 本模块的固定布局/槽掩码/长度校验保证 8 字节对齐且不越界。
        // backing 保活固定大小 VMO 及 CPU 映射；载荷也只作 AtomicU64 访问，损坏对端不能制造 Rust 数据竞争。
        unsafe { &*((self.base + offset) as *const AtomicU64) }
    }
    fn positions(&self, head: usize, tail: usize) -> Result<(u32, u32), Status> {
        let head = u32::from_le(self.counter(head).load(Ordering::Acquire));
        let tail = u32::from_le(self.counter(tail).load(Ordering::Acquire));
        if head.wrapping_sub(tail) as usize > CAPACITY {
            return Err(native_abi::ERR_BAD_STATE);
        }
        Ok((head, tail))
    }
    fn push(&self, head: usize, tail: usize, base: usize, record: Record) -> Result<(), Status> {
        let (position, consumed) = self.positions(head, tail)?;
        if position.wrapping_sub(consumed) as usize == CAPACITY {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        let slot = position as usize & (CAPACITY - 1);
        for (index, word) in record.into_iter().enumerate() {
            self.word(base + (slot * WORDS + index) * 8)
                .store(word.to_le(), Ordering::Relaxed);
        }
        self.counter(head)
            .store(position.wrapping_add(1).to_le(), Ordering::Release);
        Ok(())
    }
    fn pop(&self, head: usize, tail: usize, base: usize) -> Result<Option<Record>, Status> {
        let (produced, position) = self.positions(head, tail)?;
        if position == produced {
            return Ok(None);
        }
        let slot = position as usize & (CAPACITY - 1);
        let record = core::array::from_fn(|index| {
            u64::from_le(
                self.word(base + (slot * WORDS + index) * 8)
                    .load(Ordering::Relaxed),
            )
        });
        self.counter(tail)
            .store(position.wrapping_add(1).to_le(), Ordering::Release);
        Ok(Some(record))
    }
    pub(super) fn submit(&self, request: Record) -> Result<(), Status> {
        self.push(SQ_HEAD, SQ_TAIL, SQ, request)
    }
    pub(super) fn request(&self) -> Result<Option<Record>, Status> {
        self.pop(SQ_HEAD, SQ_TAIL, SQ)
    }
    pub(super) fn complete(&self, response: Record) -> Result<(), Status> {
        self.push(CQ_HEAD, CQ_TAIL, CQ, response)
    }
    pub(super) fn response(&self) -> Result<Option<Record>, Status> {
        self.pop(CQ_HEAD, CQ_TAIL, CQ)
    }
    pub(super) fn has_requests(&self) -> Result<bool, Status> {
        self.positions(SQ_HEAD, SQ_TAIL)
            .map(|(head, tail)| head != tail)
    }
    pub(super) fn abort(&self) {
        self.counter(ABORT).store(1u32.to_le(), Ordering::Release);
    }
    pub(super) fn aborted(&self) -> bool {
        self.counter(ABORT).load(Ordering::Acquire) != 0
    }
    pub(super) fn close(&self) {
        self.counter(CLOSED).store(1u32.to_le(), Ordering::Release);
    }
    pub(super) fn closed(&self) -> bool {
        self.counter(CLOSED).load(Ordering::Acquire) != 0
    }
    fn payload_base(ticket: u64, length: usize) -> Result<usize, Status> {
        if ticket == 0 || length > TRANSFER_LIMIT {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(PAYLOAD + (ticket as usize & (CAPACITY - 1)) * TRANSFER_LIMIT)
    }
    /// 字节不作整数端序转换；每槽按 8 字节对齐，最后不足一字的部分填零且不跨槽。
    pub(super) fn write_payload(&self, ticket: u64, bytes: &[u8]) -> Result<(), Status> {
        self.write_payload_at(ticket, 0, bytes)
    }
    /// 控制记录与 payload 分段写同一槽；offset 必须按原子字对齐，不能创建跨槽或非原子别名。
    pub(super) fn write_payload_at(
        &self,
        ticket: u64,
        offset: usize,
        bytes: &[u8],
    ) -> Result<(), Status> {
        let total = offset
            .checked_add(bytes.len())
            .ok_or(native_abi::ERR_INVALID_ARGS)?;
        if !offset.is_multiple_of(8) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let base = Self::payload_base(ticket, total)? + offset;
        for (index, bytes) in bytes.chunks(8).enumerate() {
            let mut word = [0; 8];
            word[..bytes.len()].copy_from_slice(bytes);
            self.word(base + index * 8)
                .store(u64::from_ne_bytes(word), Ordering::Relaxed);
        }
        Ok(())
    }
    /// 只追加已读出的初始化字节；调用者从完成记录校验长度，槽在复制完成后才可归还。
    pub(super) fn read_payload(
        &self,
        ticket: u64,
        length: usize,
        output: &mut Vec<u8>,
    ) -> Result<(), Status> {
        let base = Self::payload_base(ticket, length)?;
        output
            .try_reserve(length)
            .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        for index in 0..length.div_ceil(8) {
            let word = self
                .word(base + index * 8)
                .load(Ordering::Relaxed)
                .to_ne_bytes();
            output.extend_from_slice(&word[..(length - index * 8).min(8)]);
        }
        Ok(())
    }
}
