//! 崩溃记录区：物理内存顶端一段保留的 RAM，跨 warm reset 保存 panic 现场文本。
//!
//! 布局为 `[Header][UTF-8 文本]`。记录区在 frame allocator 初始化前从 RAM 末尾划出并排除在分配
//! 之外，但仍在 kernel direct map 内；因此 reset 之后新内核从同一物理地址读回上一次的现场。

use core::fmt;

use spin::Once;

use crate::memory::PhysicalAddress;

/// 记录区总大小。512 KiB 容纳 backtrace、各 CPU 状态与 128 条 kmsg 尾部，且不占用可观的 RAM。
const REGION_SIZE: usize = 512 * 1024;
const MAGIC: u64 = u64::from_le_bytes(*b"LITECRSH");
const VERSION: u32 = 1;
const HEADER_SIZE: usize = 32;

/// 保留区在 direct map 中的位置。
#[derive(Clone, Copy)]
struct Region {
    base: usize,
    size: usize,
}

// OWNER: 本模块独占保留区；boot CPU 在 memory::init 中划出一次，之后只有 panic 所有者 CPU
// （`panic::enter` 的 CAS 胜出者）写入，boot 期 `recover` 在 SMP 启动前读取并清除。缺少这条
// 单写者规则，两个 CPU 同时 panic 会把记录文本交错写坏，校验失败后现场整体丢失。
static REGION: Once<Region> = Once::new();

/// 从 `memory_end` 向下划出崩溃记录区。
///
/// # Parameters
///
/// - `memory_end`: platform 报告的 RAM 末尾（exclusive）。
///
/// # Returns
///
/// 留给 frame allocator 的新末尾。
///
/// # Panics
///
/// 重复划分，或 RAM 小于记录区时 fail-stop。
pub(crate) fn reserve(memory_end: PhysicalAddress) -> PhysicalAddress {
    let end = memory_end.as_usize();
    let start = end
        .checked_sub(REGION_SIZE)
        .expect("RAM is smaller than the crash record region");
    let base = crate::arch::mmu::physical_to_virtual(start);
    let mut reserved = false;
    REGION.call_once(|| {
        reserved = true;
        Region {
            base,
            size: REGION_SIZE,
        }
    });
    assert!(reserved, "crash record region reserved twice");
    start.into()
}

fn region() -> Option<Region> {
    REGION.get().copied()
}

/// 记录区内容的 FNV-1a 校验；只用于判断 reset 之后残留数据是否完整，不是安全边界。
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn header_bytes(length: usize, sum: u64) -> [u8; HEADER_SIZE] {
    let mut header = [0u8; HEADER_SIZE];
    header[0..8].copy_from_slice(&MAGIC.to_le_bytes());
    header[8..12].copy_from_slice(&VERSION.to_le_bytes());
    header[12..16].copy_from_slice(&(length as u32).to_le_bytes());
    header[16..24].copy_from_slice(&sum.to_le_bytes());
    header
}

/// 读取 reset 之前留下的记录。
///
/// # Returns
///
/// 魔数、版本、长度与校验全部吻合时返回文本拷贝；否则 `None`。读取后记录区被作废，同一份现场
/// 不会在下一次启动再次报告。
pub(crate) fn recover() -> Option<alloc::vec::Vec<u8>> {
    let region = region()?;
    // SAFETY: 保留区位于 direct map，且 boot 期没有其他写者。
    let bytes = unsafe { core::slice::from_raw_parts_mut(region.base as *mut u8, region.size) };
    let valid = bytes[0..8] == MAGIC.to_le_bytes() && bytes[8..12] == VERSION.to_le_bytes();
    let length = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
    let stored = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
    let payload = bytes.get(HEADER_SIZE..HEADER_SIZE.checked_add(length)?)?;
    let intact = valid && checksum(payload) == stored;
    let copy = intact.then(|| {
        let mut copy = alloc::vec::Vec::new();
        copy.try_reserve_exact(payload.len()).ok()?;
        copy.extend_from_slice(payload);
        Some(copy)
    });
    bytes[0..8].fill(0);
    copy.flatten()
}

/// panic 所有者 CPU 独占的记录写入器；内容在 [`Recorder::commit`] 之后才对下一次启动可见。
pub(crate) struct Recorder {
    payload: &'static mut [u8],
    length: usize,
}

impl Recorder {
    /// 作废旧记录并开始写新记录。
    ///
    /// # Returns
    ///
    /// 记录区尚未划出（panic 早于 `memory::init`）时返回 `None`。
    ///
    /// # Safety
    ///
    /// 调用者必须是 panic 所有者，且不会同时存在另一个 `Recorder`。
    pub(crate) unsafe fn begin() -> Option<Self> {
        let region = region()?;
        // SAFETY: 保留区在 direct map 内；调用者保证单写者。
        let bytes = unsafe { core::slice::from_raw_parts_mut(region.base as *mut u8, region.size) };
        bytes[0..8].fill(0);
        Some(Self {
            payload: &mut bytes[HEADER_SIZE..],
            length: 0,
        })
    }

    /// 让当前已写入的文本成为一份完整记录。
    ///
    /// 每个 section 写完后调用一次：后续步骤（IPI 等待、绘制蓝屏）若自身再 fault，
    /// 已经提交的部分仍能在下一次启动读回。
    pub(crate) fn commit(&mut self) {
        let sum = checksum(&self.payload[..self.length]);
        // SAFETY: header 与 payload 相邻，位于同一段保留区；payload 切片从 header 之后开始。
        let header = unsafe { self.payload.as_mut_ptr().sub(HEADER_SIZE) };
        let bytes = header_bytes(self.length, sum);
        // SAFETY: header 的 HEADER_SIZE 字节属于保留区且与 payload 不重叠。
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), header, HEADER_SIZE) };
    }
}

impl fmt::Write for Recorder {
    /// 写满后静默截断：现场保存必须尽力而为，不能因为文本过长再次失败。
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let count = text.len().min(self.payload.len() - self.length);
        self.payload[self.length..self.length + count].copy_from_slice(&text.as_bytes()[..count]);
        self.length += count;
        Ok(())
    }
}
