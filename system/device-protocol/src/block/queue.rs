//! 块会话的提交/完成环。两端分别串行化本端 producer/consumer；发布后才交出槽位。
//! 所有共享字段都使用原子字：错误对端改写记录也不会制造 Rust 非原子数据竞争。
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use device_model::DeviceBacking;
use native_runtime::Vmo;
use native_runtime::native_abi::{self, Status};

pub(super) const MAX_BATCH: usize = 64;
pub(super) const READ: u64 = 0;
pub(super) const WRITE: u64 = 1;
pub(super) const FLUSH: u64 = 2;
const HEADER_BYTES: usize = 256;
const REQUEST_WORDS: usize = 6;
const RESPONSE_WORDS: usize = 2;
const SQ_HEAD: usize = 0;
const SQ_TAIL: usize = 64;
const CQ_HEAD: usize = 128;
const CQ_TAIL: usize = 192;
const CLOSED: usize = 8;
const ABORT: usize = 12;
/// 有界设备 queue 的上限来自 virtqueue u16 容量；不是随消息数增长的缓存。
const MAX_CAPACITY: usize = 1 << 16;

/// `[request_id, operation, vmoid, device_block, blocks, buffer_offset]`，以小端原子字传输。
pub(super) type Request = [u64; REQUEST_WORDS];
/// `[request_id, status as u32]`；状态只能在对应 DMA 完成之后发布。
pub(super) type Response = [u64; RESPONSE_WORDS];

pub(super) struct Queue {
    backing: DeviceBacking,
    base: usize,
    capacity: usize,
}
impl Queue {
    pub(super) fn capacity_for(depth: usize) -> Result<usize, Status> {
        let capacity = depth
            .max(MAX_BATCH)
            .checked_next_power_of_two()
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        if capacity > MAX_CAPACITY {
            return Err(native_abi::ERR_OUT_OF_RANGE);
        }
        Ok(capacity)
    }
    fn bytes(capacity: usize) -> Result<usize, Status> {
        if !capacity.is_power_of_two() || !(MAX_BATCH..=MAX_CAPACITY).contains(&capacity) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok(
            (HEADER_BYTES + capacity * (REQUEST_WORDS + RESPONSE_WORDS) * 8)
                .next_multiple_of(native_abi::PAGE_SIZE),
        )
    }
    pub(super) fn create(capacity: usize) -> Result<Self, Status> {
        let vmo = Vmo::create(Self::bytes(capacity)? as u64, 0)?;
        Self::open(vmo, capacity)
    }
    pub(super) fn open(vmo: Vmo, capacity: usize) -> Result<Self, Status> {
        if vmo.size()? != Self::bytes(capacity)? as u64 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let backing = DeviceBacking::from_vmo(vmo)?;
        let base = backing.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        Ok(Self {
            backing,
            base,
            capacity,
        })
    }
    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }
    pub(super) fn vmo(&self) -> &Vmo {
        self.backing.vmo()
    }
    fn counter(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: 头部偏移均为 4 字节对齐且处于首个映射页；backing 保活映射。
        // 本协议的头部只作 AtomicU32 访问，不与记录的 AtomicU64 区间重叠。
        unsafe { &*((self.base + offset) as *const AtomicU32) }
    }
    fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: 记录偏移由固定宽度、已验证 capacity 和掩码槽号组成，8 字节对齐，
        // 不越过 backing；两端对载荷也只使用同宽原子访问，错误 producer 不产生数据竞争。
        unsafe { &*((self.base + offset) as *const AtomicU64) }
    }
    fn positions(&self, head: usize, tail: usize) -> Result<(u32, u32), Status> {
        let head = u32::from_le(self.counter(head).load(Ordering::Acquire));
        let tail = u32::from_le(self.counter(tail).load(Ordering::Acquire));
        if head.wrapping_sub(tail) as usize > self.capacity {
            return Err(native_abi::ERR_BAD_STATE);
        }
        Ok((head, tail))
    }
    fn push<const WORDS: usize>(
        &self,
        head_offset: usize,
        tail_offset: usize,
        base: usize,
        count: usize,
        mut record: impl FnMut(usize) -> [u64; WORDS],
    ) -> Result<(), Status> {
        let (head, tail) = self.positions(head_offset, tail_offset)?;
        if count > self.capacity - head.wrapping_sub(tail) as usize {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        for index in 0..count {
            let record = record(index);
            let slot = head.wrapping_add(index as u32) as usize & (self.capacity - 1);
            for (word, value) in record.iter().enumerate() {
                self.word(base + (slot * WORDS + word) * 8)
                    .store(value.to_le(), Ordering::Relaxed);
            }
        }
        // OWNER: producer 的 Release 交出整批记录；consumer 的 Acquire 之后才读取。
        self.counter(head_offset)
            .store(head.wrapping_add(count as u32).to_le(), Ordering::Release);
        Ok(())
    }
    fn pop<const WORDS: usize>(
        &self,
        head_offset: usize,
        tail_offset: usize,
        base: usize,
        records: &mut [[u64; WORDS]],
        stop_at_flush: bool,
    ) -> Result<usize, Status> {
        let (head, tail) = self.positions(head_offset, tail_offset)?;
        let available = (head.wrapping_sub(tail) as usize).min(records.len());
        let mut count = 0;
        for record in &mut records[..available] {
            let slot = tail.wrapping_add(count as u32) as usize & (self.capacity - 1);
            for (word, value) in record.iter_mut().enumerate() {
                *value = u64::from_le(
                    self.word(base + (slot * WORDS + word) * 8)
                        .load(Ordering::Relaxed),
                );
            }
            count += 1;
            if stop_at_flush && record[1] == FLUSH {
                break;
            }
        }
        // OWNER: consumer 拷贝完记录才归还槽位；producer 的 Acquire 阻止提前覆盖。
        self.counter(tail_offset)
            .store(tail.wrapping_add(count as u32).to_le(), Ordering::Release);
        Ok(count)
    }
    fn response_base(&self) -> usize {
        HEADER_BYTES + self.capacity * REQUEST_WORDS * 8
    }
    /// 所有失败都在调用 record 之前；回调准备每条记录后，一次 Release 发表整批。
    pub(super) fn submit(
        &self,
        count: usize,
        record: impl FnMut(usize) -> Request,
    ) -> Result<(), Status> {
        self.push(SQ_HEAD, SQ_TAIL, HEADER_BYTES, count, record)
    }
    pub(super) fn take_requests(&self, output: &mut [Request]) -> Result<usize, Status> {
        self.pop(SQ_HEAD, SQ_TAIL, HEADER_BYTES, output, true)
    }
    pub(super) fn complete(&self, records: &[Response]) -> Result<(), Status> {
        self.push(
            CQ_HEAD,
            CQ_TAIL,
            self.response_base(),
            records.len(),
            |index| records[index],
        )
    }
    pub(super) fn take_responses(&self, output: &mut [Response]) -> Result<usize, Status> {
        self.pop(CQ_HEAD, CQ_TAIL, self.response_base(), output, false)
    }
    pub(super) fn has_requests(&self) -> Result<bool, Status> {
        self.positions(SQ_HEAD, SQ_TAIL)
            .map(|(head, tail)| head != tail)
    }
    pub(super) fn closed(&self) -> bool {
        self.counter(CLOSED).load(Ordering::Acquire) != 0
    }
    /// 只在所有服务工作退出、登记缓冲回收后发表，不能由单个失败请求提前发布。
    pub(super) fn close(&self) {
        self.counter(CLOSED).store(1, Ordering::Release);
    }
    pub(super) fn abort_requested(&self) -> bool {
        self.counter(ABORT).load(Ordering::Acquire) != 0
    }
    /// 客户端协议异常只请求停止；仍等待 close/对端退出，不能直接归还在途 DMA 的区间。
    pub(super) fn request_abort(&self) {
        self.counter(ABORT).store(1, Ordering::Release);
    }
}
