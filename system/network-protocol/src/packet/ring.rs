//! 调用者提供 VMO 的定长帧接收环。

use crate::{PacketAddress, PacketChecksum, Reader, SocketError, Status, Vmo, Writer, native_abi};
use core::{
    ptr,
    sync::atomic::{AtomicU32, Ordering},
};
use device_model::DeviceBacking;

const FRAME_ALIGNMENT: usize = 16;
const MAX_BYTES: usize = 64 * 1024 * 1024;
// 所有权字、帧头、保留位和源地址的固定范围。
const METADATA_BYTES: usize = 52;

/// 两端均校验的接收环布局。零所有权字归生产者，消费者以 Release 写零归还槽位。
#[derive(Clone, Copy, Debug)]
pub struct FrameRingLayout {
    pub block_bytes: u32,
    pub blocks: u32,
    pub frame_bytes: u32,
    pub frames: u32,
    pub link_offset: u16,
    pub network_offset: u16,
    pub address_family: u16,
    pub user: u32,
    pub losing: u32,
    pub checksum_pending: u32,
    pub checksum_verified: u32,
}

impl FrameRingLayout {
    pub(crate) const VERSION: u32 = 1;
    pub(crate) const WIRE_BYTES: usize = 4 * 4 + 3 * 2 + 4 * 4;

    /// 校验页块、帧及元数据边界，返回所需的 VMO 字节数。
    /// # Errors
    /// 几何与偏移不合法或大小超过上限。
    pub fn byte_len(self) -> Result<usize, SocketError> {
        let block = self.block_bytes as usize;
        let frame = self.frame_bytes as usize;
        let link = usize::from(self.link_offset);
        let network = usize::from(self.network_offset);
        if block == 0
            || !block.is_multiple_of(native_abi::PAGE_SIZE)
            || frame == 0
            || !frame.is_multiple_of(FRAME_ALIGNMENT)
            || frame > block
            || link < METADATA_BYTES
            || network < link
            || !matches!(network - link, 0 | 14)
            || network >= frame
            || self.user == 0
        {
            return Err(SocketError::Invalid);
        }
        let count = self.blocks as usize;
        let bytes = block.checked_mul(count).ok_or(SocketError::NoMemory)?;
        if count == 0 || count.checked_mul(block / frame) != Some(self.frames as usize) {
            return Err(SocketError::Invalid);
        }
        if bytes > MAX_BYTES {
            return Err(SocketError::NoMemory);
        }
        Ok(bytes)
    }

    pub(super) fn read(input: &mut Reader<'_>) -> Result<Self, Status> {
        Ok(Self {
            block_bytes: input.u32()?,
            blocks: input.u32()?,
            frame_bytes: input.u32()?,
            frames: input.u32()?,
            link_offset: input.u16()?,
            network_offset: input.u16()?,
            address_family: input.u16()?,
            user: input.u32()?,
            losing: input.u32()?,
            checksum_pending: input.u32()?,
            checksum_verified: input.u32()?,
        })
    }

    pub(crate) fn write(self, output: &mut Writer) {
        output
            .u32(self.block_bytes)
            .u32(self.blocks)
            .u32(self.frame_bytes)
            .u32(self.frames)
            .u16(self.link_offset)
            .u16(self.network_offset)
            .u16(self.address_family)
            .u32(self.user)
            .u32(self.losing)
            .u32(self.checksum_pending)
            .u32(self.checksum_verified);
    }
}

/// 生产者持有 VMO 映射及唯一写入游标。消费者通过所有权字归还槽位。
pub struct CaptureRing {
    backing: DeviceBacking,
    layout: FrameRingLayout,
    per_block: usize,
    next: usize,
}

impl CaptureRing {
    /// 发布前准备等大的映射并提交物理页。
    /// # Errors
    /// 几何或 VMO 不合要求、无法映射或提交页面。
    pub(super) fn new(vmo: Vmo, layout: FrameRingLayout) -> Result<Self, SocketError> {
        let bytes = layout.byte_len()?;
        if vmo.size().map_err(|_| SocketError::Invalid)? != bytes as u64 {
            return Err(SocketError::Invalid);
        }
        vmo.op_range(native_abi::vmo::OP_COMMIT, 0, bytes as u64)
            .map_err(crate::client::transport)?;
        let backing = DeviceBacking::from_vmo(vmo).map_err(crate::client::transport)?;
        backing.cpu_address().ok_or(SocketError::NoMemory)?;
        Ok(Self {
            backing,
            layout,
            per_block: (layout.block_bytes / layout.frame_bytes) as usize,
            next: 0,
        })
    }

    fn offset(&self, slot: usize) -> usize {
        slot / self.per_block * self.layout.block_bytes as usize
            + slot % self.per_block * self.layout.frame_bytes as usize
    }

    fn word(&self, offset: usize) -> &AtomicU32 {
        self.backing
            .atomic_u32(offset)
            .expect("validated aligned frame")
    }

    /// 链路头所占字节数，必须与 endpoint 的报文类型匹配。
    pub fn link_header_length(&self) -> usize {
        usize::from(self.layout.network_offset - self.layout.link_offset)
    }

    /// 下一槽是否可由生产者写入。
    pub fn can_receive(&self) -> bool {
        self.word(self.offset(self.next)).load(Ordering::Acquire) == 0
    }

    /// 按序消费时，最新发布的槽位尚未归还表示环可读。
    pub fn readable(&self) -> bool {
        let previous = if self.next == 0 {
            self.layout.frames as usize - 1
        } else {
            self.next - 1
        };
        self.word(self.offset(previous)).load(Ordering::Acquire) != 0
    }

    /// 发布一帧，返回是否需要唤醒消费者；槽位仍被占用时返回 `None`。
    pub fn push(
        &mut self,
        source: PacketAddress,
        header: &[u8; 14],
        payload: &[u8],
        timestamp_ns: u64,
        checksum: PacketChecksum,
        losing: bool,
    ) -> Option<bool> {
        let offset = self.offset(self.next);
        let ownership = self.word(offset);
        if ownership.load(Ordering::Acquire) != 0 {
            return None;
        }
        let link = usize::from(self.layout.link_offset);
        let network = usize::from(self.layout.network_offset);
        let header = &header[..network - link];
        let original = (payload.len() + header.len()) as u32;
        let payload = &payload[..payload
            .len()
            .min(self.layout.frame_bytes as usize - network)];
        let captured = (payload.len() + header.len()) as u32;
        let mut metadata = [0u8; METADATA_BYTES - 4];
        let (seconds, nanos) = (timestamp_ns / 1_000_000_000, timestamp_ns % 1_000_000_000);
        metadata[..4].copy_from_slice(&original.to_ne_bytes());
        metadata[4..8].copy_from_slice(&captured.to_ne_bytes());
        metadata[8..10].copy_from_slice(&self.layout.link_offset.to_ne_bytes());
        metadata[10..12].copy_from_slice(&self.layout.network_offset.to_ne_bytes());
        metadata[12..16].copy_from_slice(&(seconds as u32).to_ne_bytes());
        metadata[16..20].copy_from_slice(&(nanos as u32).to_ne_bytes());
        metadata[28..30].copy_from_slice(&self.layout.address_family.to_ne_bytes());
        metadata[30..32].copy_from_slice(&source.protocol.to_ne_bytes());
        metadata[32..36].copy_from_slice(&source.interface_index.to_ne_bytes());
        metadata[36..38].copy_from_slice(&source.hardware_type.to_ne_bytes());
        metadata[38] = source.packet_type;
        metadata[39] = source.address_length;
        metadata[40..48].copy_from_slice(&source.address);
        let base = self.backing.cpu_address().expect("prepared mapping") + offset;
        // SAFETY: geometry was checked against the committed VMO, offsets are immutable and no
        // reference to consumer memory is created. The ownership word pairs with the consumer's
        // Release. Guest-written offsets and payload are never read or used as producer input.
        unsafe {
            ptr::copy_nonoverlapping(metadata.as_ptr(), (base + 4) as *mut u8, metadata.len());
            ptr::copy_nonoverlapping(header.as_ptr(), (base + link) as *mut u8, header.len());
            ptr::copy_nonoverlapping(payload.as_ptr(), (base + network) as *mut u8, payload.len());
        }
        let flags = match checksum {
            PacketChecksum::Partial => self.layout.checksum_pending,
            PacketChecksum::Verified => self.layout.checksum_verified,
            PacketChecksum::Unverified => 0,
        };
        ownership.store(
            self.layout.user | flags | if losing { self.layout.losing } else { 0 },
            Ordering::Release,
        );
        // 发布后才观察前一槽。消费者若刚好归还前一槽并进入 poll，新帧要么已对 poll 可见，
        // 要么由这次发布唤醒。先读前一槽会漏掉归还与发布之间的等待者。
        let notify = self.layout.frames == 1 || !self.readable();
        self.next += 1;
        if self.next == self.layout.frames as usize {
            self.next = 0;
        }
        Some(notify)
    }
}
