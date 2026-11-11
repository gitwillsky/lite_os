//! 四个单生产者/单消费者环；本端的队列锁串行化发布与领取。
//! 元数据全部使用同宽原子访问，错误对端也不能制造 Rust 非原子数据竞争。
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use device_model::DeviceBacking;
use native_runtime::Vmo;
use native_runtime::native_abi::{self, Status};

pub(super) const MAX_BATCH: usize = device_model::network::RECEIVE_BATCH_CAPACITY;
const HEADER: usize = 576;
const CLOSED: usize = 512;
const ABORT: usize = 516;
const KICK: usize = 520;
// 队列零的唯一链路快照；复用现有首部保留空间，不增加 VMO/页或改变环起点。
// u64 一次交接 carrier/speed/error，避免两字段拼出不存在的物理状态；只由服务端发布。
const LINK: usize = 528;
// 同一 Release 字原子发布状态、速度和代际，合并通知仍能识别断开后恢复。
const LINK_STATE: u64 = 7;
const LINK_EPOCH_MASK: u64 = (1 << 29) - 1;
const MAX_CAPACITY: usize = 1 << 16;
/// RX/TX: [slot, ticket, length, checksum, segmentation]；Fill/CQ 的 metadata 必须为零。
pub(super) type Record = [u64; 5];

#[derive(Clone, Copy)]
pub(super) enum Ring {
    Receive = 0,
    Fill = 1,
    Transmit = 2,
    Completion = 3,
}

pub(super) struct Queue {
    backing: DeviceBacking,
    view: RingView,
}
impl core::ops::Deref for Queue {
    type Target = RingView;
    fn deref(&self) -> &RingView {
        &self.view
    }
}
impl Queue {
    pub(super) fn capacity_for(depth: usize) -> Result<usize, Status> {
        let capacity = depth
            .max(MAX_BATCH)
            .checked_next_power_of_two()
            .ok_or(native_abi::ERR_OUT_OF_RANGE)?;
        Self::bytes(capacity)?;
        Ok(capacity)
    }
    fn bytes(capacity: usize) -> Result<usize, Status> {
        if !capacity.is_power_of_two() || !(MAX_BATCH..=MAX_CAPACITY).contains(&capacity) {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        Ok((HEADER + capacity * 4 * core::mem::size_of::<Record>())
            .next_multiple_of(native_abi::PAGE_SIZE))
    }
    pub(super) fn create(capacity: usize) -> Result<Self, Status> {
        Self::open(Vmo::create(Self::bytes(capacity)? as u64, 0)?, capacity)
    }
    pub(super) fn open(vmo: Vmo, capacity: usize) -> Result<Self, Status> {
        if vmo.size()? != Self::bytes(capacity)? as u64 {
            return Err(native_abi::ERR_INVALID_ARGS);
        }
        let backing = DeviceBacking::from_vmo(vmo)?;
        let base = backing.cpu_address().ok_or(native_abi::ERR_NO_MEMORY)?;
        // SAFETY: 整页映射覆盖经过检查的布局，backing 保活它，字段没有混合宽度的访问。
        let view = unsafe { RingView::new(base, capacity) };
        Ok(Self { backing, view })
    }
    pub(super) fn vmo(&self) -> &Vmo {
        self.backing.vmo()
    }
}

/// 不单独拥有映射；只有上方 Queue（或测试的原子字数组）能构造且保活它。
pub(super) struct RingView {
    base: usize,
    capacity: usize,
}
impl RingView {
    /// SAFETY: 调用者保证 base 按 8 字节对齐、覆盖 Queue::bytes(capacity)，且在本 view 寿命内有效。
    unsafe fn new(base: usize, capacity: usize) -> Self {
        Self { base, capacity }
    }
    pub(super) fn capacity(&self) -> usize {
        self.capacity
    }
    fn counter(&self, offset: usize) -> &AtomicU32 {
        // SAFETY: 头部的常量偏移均 4-byte 对齐且不重叠；映射由 owner 保活。
        unsafe { &*((self.base + offset) as *const AtomicU32) }
    }
    fn word(&self, offset: usize) -> &AtomicU64 {
        // SAFETY: LINK 首部常量与记录偏移均按 8 对齐且互不重叠；同一位置只用 AtomicU64，映射保活。
        unsafe { &*((self.base + offset) as *const AtomicU64) }
    }
    pub(super) fn publish_link(
        &self,
        link: Result<device_model::network::NetworkLink, device_model::network::NetworkError>,
    ) {
        let value = match link {
            Ok(link) => {
                1 | (u64::from(link.carrier) << 1)
                    | (u64::from(link.speed_mbps.unwrap_or(u32::MAX)) << 32)
            }
            Err(error) => 5 | (u64::from(super::encode_error(error) as u32) << 32),
        };
        let word = self.word(LINK);
        // 唯一写者在 link_publish 下串行；读者不会看见新载波配旧代际。
        let previous = u64::from_le(word.load(Ordering::Relaxed));
        let epoch = ((previous >> 3) & LINK_EPOCH_MASK)
            + u64::from((previous & LINK_STATE) != (value & LINK_STATE));
        word.store(
            (value | ((epoch & LINK_EPOCH_MASK) << 3)).to_le(),
            Ordering::Release,
        );
    }
    pub(super) fn link(
        &self,
    ) -> Result<device_model::network::NetworkLink, device_model::network::NetworkError> {
        use device_model::network::{NetworkError, NetworkLink};
        if self.closed() {
            return Err(NetworkError::Disconnected);
        }
        if self.aborted() {
            return Err(NetworkError::Device);
        }
        let value = u64::from_le(self.word(LINK).load(Ordering::Acquire));
        match value & LINK_STATE {
            0 if value == 0 => Err(NetworkError::WouldBlock),
            1 | 3 => {
                let speed = (value >> 32) as u32;
                Ok(NetworkLink {
                    carrier: value & 2 != 0,
                    carrier_epoch: ((value >> 3) & LINK_EPOCH_MASK) as u32,
                    speed_mbps: (speed <= i32::MAX as u32).then_some(speed),
                })
            }
            5 if value >> 32 != 0 => Err(super::decode_error((value >> 32) as u32 as i32)),
            _ => Err(NetworkError::Device),
        }
    }
    fn positions(&self, ring: Ring) -> Result<(u32, u32), Status> {
        let offset = ring as usize * 128;
        let head = u32::from_le(self.counter(offset).load(Ordering::Acquire));
        let tail = u32::from_le(self.counter(offset + 64).load(Ordering::Acquire));
        if head.wrapping_sub(tail) as usize > self.capacity {
            return Err(native_abi::ERR_BAD_STATE);
        }
        Ok((head, tail))
    }
    pub(super) fn pending(&self, ring: Ring) -> Result<usize, Status> {
        self.positions(ring)
            .map(|(head, tail)| head.wrapping_sub(tail) as usize)
    }
    pub(super) fn free(&self, ring: Ring) -> Result<usize, Status> {
        Ok(self.capacity - self.pending(ring)?)
    }
    pub(super) fn push(&self, ring: Ring, records: &[Record]) -> Result<(), Status> {
        let (head, tail) = self.positions(ring)?;
        if records.len() > self.capacity - head.wrapping_sub(tail) as usize {
            return Err(native_abi::ERR_SHOULD_WAIT);
        }
        for (index, record) in records.iter().enumerate() {
            let slot = head.wrapping_add(index as u32) as usize & (self.capacity - 1);
            for (word, value) in record.iter().enumerate() {
                self.word(
                    HEADER
                        + (ring as usize * self.capacity + slot) * core::mem::size_of::<Record>()
                        + word * 8,
                )
                .store(value.to_le(), Ordering::Relaxed);
            }
        }
        // Release 同时交出记录和 TX payload；consumer Acquire 后才访问。
        self.counter(ring as usize * 128).store(
            head.wrapping_add(records.len() as u32).to_le(),
            Ordering::Release,
        );
        Ok(())
    }
    pub(super) fn pop(&self, ring: Ring, output: &mut [Record]) -> Result<usize, Status> {
        let (head, tail) = self.positions(ring)?;
        let count = output.len().min(head.wrapping_sub(tail) as usize);
        for (index, record) in output[..count].iter_mut().enumerate() {
            let slot = tail.wrapping_add(index as u32) as usize & (self.capacity - 1);
            for (word, value) in record.iter_mut().enumerate() {
                *value = u64::from_le(
                    self.word(
                        HEADER
                            + (ring as usize * self.capacity + slot)
                                * core::mem::size_of::<Record>()
                            + word * 8,
                    )
                    .load(Ordering::Relaxed),
                );
            }
        }
        // 拷贝完成后才归还 ring 槽；packet 槽仍由 ticket 的 owner 持有。
        self.counter(ring as usize * 128 + 64)
            .store(tail.wrapping_add(count as u32).to_le(), Ordering::Release);
        Ok(count)
    }
    pub(super) fn closed(&self) -> bool {
        self.counter(CLOSED).load(Ordering::Acquire) != 0
    }
    /// 服务端只在 drain 或同步停止 DMA 后发表；不能用关闭通知提前复用 DMA 区间。
    pub(super) fn close(&self) {
        self.counter(CLOSED).store(1, Ordering::Release);
    }
    pub(super) fn aborted(&self) -> bool {
        self.counter(ABORT).load(Ordering::Acquire) != 0
    }
    pub(super) fn request_abort(&self) {
        self.counter(ABORT).store(1, Ordering::Release);
    }
    /// 发表后仅 0→1 通知服务；否则已有一次唤醒负责排空，避免每帧 syscall。
    pub(super) fn needs_kick(&self) -> bool {
        self.counter(KICK).swap(1, Ordering::AcqRel) == 0
    }
    /// 必须在排空之前清除；并发 producer 要么被本轮消费，要么重新置位并唤醒。
    pub(super) fn begin_dispatch(&self) {
        self.counter(KICK).swap(0, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn publication_wrap_capacity_and_kick_rearm() {
        // 仅替换 native 映射分配，四条生产 codec 原样执行；数组首地址满足实际字段对齐。
        let mut words = vec![0u64; Queue::bytes(MAX_BATCH).unwrap() / 8];
        // SAFETY: words 覆盖完整布局且按 8 对齐，view 不离开该 owner 寿命；之后只通过原子访问。
        let ring = unsafe { RingView::new(words.as_mut_ptr() as usize, MAX_BATCH) };
        for kind in [Ring::Receive, Ring::Fill, Ring::Transmit, Ring::Completion] {
            let offset = kind as usize * 128;
            ring.counter(offset)
                .store((u32::MAX - 4).to_le(), Ordering::Relaxed);
            ring.counter(offset + 64)
                .store((u32::MAX - 4).to_le(), Ordering::Relaxed);
            let input = [[7, 100, 1514, 0, 0]; MAX_BATCH];
            ring.push(kind, &input).unwrap();
            assert_eq!(ring.free(kind), Ok(0));
            assert_eq!(
                ring.push(kind, &input[..1]),
                Err(native_abi::ERR_SHOULD_WAIT)
            );
            let mut output = [[0; 5]; MAX_BATCH];
            assert_eq!(ring.pop(kind, &mut output), Ok(MAX_BATCH));
            assert_eq!(input, output);
            assert_eq!(ring.pending(kind), Ok(0));
        }
        assert!(ring.needs_kick());
        assert!(!ring.needs_kick());
        ring.begin_dispatch();
        assert!(ring.needs_kick());
        ring.counter(0).store(1000u32.to_le(), Ordering::Relaxed);
        assert_eq!(ring.pending(Ring::Receive), Err(native_abi::ERR_BAD_STATE));
    }

    #[test]
    fn published_link_uses_reserved_header_and_never_masks_terminal_or_invalid_state() {
        use device_model::network::{NetworkError, NetworkLink};
        // 原环用例不覆盖新首部字段：偏移重叠会破坏队列，错误/退出被当成 UP 会阻止地址生命周期撤销。
        let mut words = vec![0u64; Queue::bytes(MAX_BATCH).unwrap() / 8];
        // SAFETY: 同一 backing 覆盖完整布局并按 8 对齐；此后只用生产 RingView 的同宽原子访问。
        let ring = unsafe { RingView::new(words.as_mut_ptr() as usize, MAX_BATCH) };
        assert_eq!(ring.link().err(), Some(NetworkError::WouldBlock));
        for (epoch, carrier, speed) in [
            (1, true, Some(1000)),
            (1, true, Some(100)),
            (2, false, Some(10)),
            (3, true, None),
        ] {
            ring.publish_link(Ok(NetworkLink {
                carrier,
                carrier_epoch: 0,
                speed_mbps: speed,
            }));
            assert_eq!(
                ring.link()
                    .map(|link| (link.carrier_epoch, link.carrier, link.speed_mbps)),
                Ok((epoch, carrier, speed))
            );
            assert!(!ring.closed() && !ring.aborted());
            let record = [[1, 9, 64, 0, 0]];
            ring.push(Ring::Transmit, &record).unwrap();
            let mut output = [[0; 5]; 1];
            assert_eq!(ring.pop(Ring::Transmit, &mut output), Ok(1));
            assert_eq!(output, record);
        }
        for error in [
            NetworkError::Device,
            NetworkError::Disconnected,
            NetworkError::WouldBlock,
            NetworkError::NoResources,
        ] {
            ring.publish_link(Err(error));
            assert_eq!(ring.link().err(), Some(error));
        }
        for invalid in [1u64 << 32, 7, 5, 2 | (1 << 10)] {
            ring.word(LINK).store(invalid.to_le(), Ordering::Release);
            assert_eq!(ring.link().err(), Some(NetworkError::Device));
        }
        ring.publish_link(Ok(NetworkLink {
            carrier: true,
            carrier_epoch: 0,
            speed_mbps: Some(1000),
        }));
        ring.request_abort();
        assert_eq!(ring.link().err(), Some(NetworkError::Device));
        ring.close();
        assert_eq!(ring.link().err(), Some(NetworkError::Disconnected));
    }
}
