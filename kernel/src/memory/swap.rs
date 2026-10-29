//! Swap 的唯一 area/slot owner；物理存储 lease 由 storage adapter 实现。

mod header;

use alloc::{sync::Arc, vec::Vec};
use core::fmt::Debug;
use spin::{Mutex, Once};

use crate::execution::sync::{TaskEvent, TaskMutex, TaskMutexWaitPreparation};

const MAX_AREAS: usize = 32;
const RESERVED_SLOT: u32 = u32::MAX;

/// Swap admission、I/O 与 drain 的领域错误；ABI 层负责 errno。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SwapError {
    Invalid,
    Busy,
    NotFound,
    OutOfMemory,
    Io,
    NoSpace,
    Interrupted,
}

impl From<super::SharedFileError> for SwapError {
    fn from(error: super::SharedFileError) -> Self {
        match error {
            super::SharedFileError::OutOfMemory => Self::OutOfMemory,
            super::SharedFileError::Io | super::SharedFileError::BeyondEof => Self::Io,
        }
    }
}

fn check_interrupted() -> Result<(), SwapError> {
    if crate::execution::sync::task_interrupted() {
        Err(SwapError::Interrupted)
    } else {
        Ok(())
    }
}

/// 持有排他 storage lease 的页 I/O seam；不依赖 filesystem/device 的具体实现。
pub(crate) trait SwapStorage: Send + Sync + Debug {
    /// 跨 pathname alias 稳定的存储 identity。
    fn identity(&self) -> crate::memory::SharedFileId;
    /// 可读取的实际字节长度，不从 header 推断设备容量。
    fn size_bytes(&self) -> u64;
    /// Header comes from the original logical source, independently of the immutable swap-page map.
    fn read_header(&self, output: &mut [u8; 4096]) -> Result<(), SwapError> {
        self.read_page(0, output)
    }
    /// Storage kind projection for /proc/swaps.
    fn is_file(&self) -> bool {
        false
    }
    /// 同步完整读取一个 4KiB 页；返回时 DMA 已完成。
    fn read_page(&self, page: usize, output: &mut [u8; 4096]) -> Result<(), SwapError>;
    /// 同步完整写入一个 4KiB 页；返回时设备不再引用 input。
    fn write_page(&self, page: usize, input: &[u8; 4096]) -> Result<(), SwapError>;
    /// 最后一个 slot 已释放、area 已移除时同步交还排他 lease；其余 Arc 不再保有 claim。
    fn close(&self);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Active,
    Draining,
    Removed,
}

struct SlotState {
    phase: Phase,
    references: Vec<u32>,
    free_bitmap: Vec<usize>,
    free_pages: usize,
    cursor: usize,
}

/// activated area 的 identity、header-derived slot truth 与排他 backing lease。
struct SwapArea {
    storage: Arc<dyn SwapStorage>,
    path: Vec<u8>,
    priority: i32,
    capacity: usize,
    // OWNER: references 与 free bitmap/counter/cursor 在同一锁中提交；锁内无 allocation/I/O。
    // Slot 引用跨 fork 和整个同步 DMA 保活该 owner，不能用 Arc count 代替实际 slot truth。
    state: Mutex<SlotState>,
    // OWNER: operation lock 保证一个 drain waiter；最后一个 slot 锁外 signal。
    // Weak mm 已退休但字段尚未析构时，不能把正常退出误报 Busy 或提前关闭 storage。
    retired: TaskEvent,
}

struct AreaTable {
    areas: [Option<Arc<SwapArea>>; MAX_AREAS],
    cursor: usize,
    next_auto_priority: i32,
}

struct SwapRegistry {
    // OWNER: activation/drain 串行，但正常 reserve/page-in 不取得此 task-only lock。
    operation: TaskMutex<()>,
    // OWNER: activated area publication 的唯一集合；slot state 归各 area，同锁快照无 I/O。
    table: Mutex<AreaTable>,
}

static SWAP_AREAS: Once<SwapRegistry> = Once::new();

fn registry() -> &'static SwapRegistry {
    SWAP_AREAS.call_once(|| SwapRegistry {
        operation: TaskMutex::new(()),
        table: Mutex::new(AreaTable {
            areas: core::array::from_fn(|_| None),
            cursor: 0,
            next_auto_priority: -1,
        }),
    })
}

fn zeroed<T: Clone>(length: usize, value: T) -> Result<Vec<T>, SwapError> {
    let mut result = Vec::new();
    result
        .try_reserve_exact(length)
        .map_err(|_| SwapError::OutOfMemory)?;
    result.resize(length, value);
    Ok(result)
}

impl SwapArea {
    fn prepare(
        storage: Arc<dyn SwapStorage>,
        path: &[u8],
        priority: i32,
    ) -> Result<Arc<Self>, SwapError> {
        let mut bytes = [0u8; 4096];
        storage.read_header(&mut bytes)?;
        let header = header::Header::parse(&bytes, storage.size_bytes())?;
        let mut references = zeroed(header.pages, 0u32)?;
        references[0] = RESERVED_SLOT;
        for page in header.bad_pages() {
            references[page] = RESERVED_SLOT;
        }
        let mut free_bitmap = zeroed(header.pages.div_ceil(usize::BITS as usize), 0usize)?;
        let mut free_pages = 0;
        for (page, count) in references.iter().enumerate() {
            if *count == 0 {
                free_bitmap[page / usize::BITS as usize] |= 1usize << (page % usize::BITS as usize);
                free_pages += 1;
            }
        }
        if free_pages == 0 {
            return Err(SwapError::Invalid);
        }
        let mut name = Vec::new();
        name.try_reserve_exact(path.len())
            .map_err(|_| SwapError::OutOfMemory)?;
        name.extend_from_slice(path);
        Arc::try_new(Self {
            storage,
            path: name,
            priority,
            capacity: free_pages,
            retired: TaskEvent::new(),
            state: Mutex::new(SlotState {
                phase: Phase::Active,
                references,
                free_bitmap,
                free_pages,
                cursor: 0,
            }),
        })
        .map_err(|_| SwapError::OutOfMemory)
    }

    fn reserve(self: &Arc<Self>) -> Option<SwapSlot> {
        let mut state = self.state.lock();
        if state.phase != Phase::Active || state.free_pages == 0 {
            return None;
        }
        let words = state.free_bitmap.len();
        for delta in 0..words {
            let word = (state.cursor + delta) % words;
            let available = state.free_bitmap[word];
            if available == 0 {
                continue;
            }
            let bit = available.trailing_zeros() as usize;
            let page = word * usize::BITS as usize + bit;
            assert_eq!(state.references[page], 0, "swap free bitmap diverged");
            state.references[page] = 1;
            state.free_bitmap[word] &= !(1usize << bit);
            state.free_pages -= 1;
            state.cursor = word;
            return Some(SwapSlot {
                area: self.clone(),
                page,
            });
        }
        panic!("swap free count diverged from bitmap");
    }
}

/// 一份 swapped-page ownership；克隆仅增加同一 area 的 slot reference，不分配新槽。
pub(in crate::memory) struct SwapSlot {
    area: Arc<SwapArea>,
    page: usize,
}

impl Debug for SwapSlot {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("SwapSlot")
            .field("storage", &self.identity())
            .field("page", &self.page)
            .finish()
    }
}

impl Clone for SwapSlot {
    fn clone(&self) -> Self {
        let mut state = self.area.state.lock();
        let count = &mut state.references[self.page];
        assert!(
            *count > 0 && *count < RESERVED_SLOT - 1,
            "invalid/exhausted swap reference"
        );
        *count += 1;
        Self {
            area: self.area.clone(),
            page: self.page,
        }
    }
}

impl Drop for SwapSlot {
    fn drop(&mut self) {
        let mut state = self.area.state.lock();
        let count = &mut state.references[self.page];
        assert!(
            *count > 0 && *count != RESERVED_SLOT,
            "swap slot released twice"
        );
        *count -= 1;
        if *count == 0 {
            state.free_bitmap[self.page / usize::BITS as usize] |=
                1usize << (self.page % usize::BITS as usize);
            state.free_pages += 1;
        }
        let retired = state.phase == Phase::Draining && state.free_pages == self.area.capacity;
        drop(state);
        if retired {
            self.area.retired.signal();
        }
    }
}

impl SwapSlot {
    /// Retained swapcache can be reused only while the area still admits swap-out.
    pub(in crate::memory) fn active(&self) -> bool {
        self.area.state.lock().phase == Phase::Active
    }
    /// 待 drain 的稳定 storage identity。
    pub(in crate::memory) fn identity(&self) -> crate::memory::SharedFileId {
        self.area.storage.identity()
    }
    /// Slot lifetime 跨整个同步 I/O，不允许调用者先归还槽再等待 DMA。
    pub(in crate::memory) fn read(&self, output: &mut [u8; 4096]) -> Result<(), SwapError> {
        self.area.storage.read_page(self.page, output)
    }
    /// 尚未发布的槽仅在此写入成功后才能接替 resident frame。
    pub(in crate::memory) fn write(&self, input: &[u8; 4096]) -> Result<(), SwapError> {
        self.area.storage.write_page(self.page, input)
    }
}

/// Linux swapon flags validation；discard 是可选 advisory，不伪造 device discard 能力。
pub(crate) fn valid_flags(flags: u32) -> bool {
    flags & !0x7ffff == 0
}

/// 在完整 header/slot preparation 成功后一次发布 area；失败由 storage lease 的 Drop 清理。
pub(crate) fn activate(
    storage: Arc<dyn SwapStorage>,
    path: &[u8],
    flags: u32,
) -> Result<(), SwapError> {
    if !valid_flags(flags) {
        return Err(SwapError::Invalid);
    }
    let registry = registry();
    let _operation = registry
        .operation
        .lock()
        .map_err(|_| SwapError::OutOfMemory)?;
    let priority = if flags & 0x8000 != 0 {
        (flags & 0x7fff) as i32
    } else {
        registry.table.lock().next_auto_priority
    };
    let prepared = SwapArea::prepare(storage, path, priority)?;
    let mut table = registry.table.lock();
    if table
        .areas
        .iter()
        .flatten()
        .any(|area| area.storage.identity() == prepared.storage.identity())
    {
        drop(table);
        return Err(SwapError::Busy);
    }
    let Some(index) = table.areas.iter().position(Option::is_none) else {
        drop(table);
        return Err(SwapError::NoSpace);
    };
    if flags & 0x8000 == 0 {
        table.next_auto_priority = priority.saturating_sub(1);
    }
    table.areas[index] = Some(prepared);
    Ok(())
}

/// 从最高 priority 的 active area 领取槽，同级按 publication slot 轮转；不分配、不 I/O。
pub(in crate::memory) fn reserve() -> Option<SwapSlot> {
    let mut table = registry().table.lock();
    let priority = table
        .areas
        .iter()
        .flatten()
        .filter_map(|area| {
            let state = area.state.lock();
            (state.phase == Phase::Active && state.free_pages != 0).then_some(area.priority)
        })
        .max()?;
    for delta in 0..MAX_AREAS {
        let index = (table.cursor + delta) % MAX_AREAS;
        if let Some(area) = &table.areas[index]
            && area.priority == priority
            && let Some(slot) = area.reserve()
        {
            table.cursor = (index + 1) % MAX_AREAS;
            return Some(slot);
        }
    }
    None
}

/// 禁止新增换出后，依次恢复唯一 live-mm registry 中的引用；失败保留剩余槽与 storage。
pub(crate) fn deactivate(
    identity: crate::memory::SharedFileId,
    wait: &mut TaskMutexWaitPreparation,
) -> Result<(), SwapError> {
    let registry = registry();
    let _operation = registry
        .operation
        .lock()
        .map_err(|_| SwapError::OutOfMemory)?;
    let area = registry
        .table
        .lock()
        .areas
        .iter()
        .flatten()
        .find(|area| area.storage.identity() == identity)
        .cloned()
        .ok_or(SwapError::NotFound)?;
    area.state.lock().phase = Phase::Draining;
    // Linux try_to_unuse：空 area 成功优先于 pending signal；有槽才执行可中断 maintenance。
    let restored = if area.state.lock().free_pages == area.capacity {
        Ok(())
    } else {
        super::swappable_page::restore_storage(identity, &mut check_interrupted).and_then(|()| {
            super::shared_file::restore_swapped_pages(identity, wait, &mut check_interrupted)
        })
    };
    if let Err(error) = restored {
        let mut state = area.state.lock();
        // 所有 checkpoint 共用最终 usage 复查；不能因最后槽退休后的无关 owner 报 EINTR。
        if error != SwapError::Interrupted || state.free_pages != area.capacity {
            state.phase = Phase::Active;
            return Err(error);
        }
    }
    loop {
        let mut state = area.state.lock();
        if state.free_pages == area.capacity {
            state.phase = Phase::Removed;
            break;
        }
        // live mm 已扫描并恢复；余下引用属于正在退出的字段 retirement。
        // 条件复查与 TaskEvent pending/arming 协议覆盖 unlock 到 wait 之间的最后一个 Drop。
        drop(state);
        if area.retired.wait_interruptible() == crate::execution::sync::WaitResult::Interrupted {
            // 已恢复页无需回滚；剩余槽仍持 storage，Active 重新允许换出和下一次 swapoff。
            // 完整 drain 与中断竞争时，最后槽先退休则允许成功，与 Linux try_to_unuse 一致。
            let mut state = area.state.lock();
            if state.free_pages != area.capacity {
                state.phase = Phase::Active;
                return Err(SwapError::Interrupted);
            }
        }
    }
    let removed = {
        let mut table = registry.table.lock();
        let index = table
            .areas
            .iter()
            .position(|candidate| {
                candidate
                    .as_ref()
                    .is_some_and(|candidate| Arc::ptr_eq(candidate, &area))
            })
            .expect("draining swap area lost registry owner");
        table.areas[index].take().unwrap()
    };
    // Slot count=0 不等于最后一个 Slot.area Arc 已 Drop；显式 close 同步交还 claim。
    // 所有临时强引用仍只能观察 Removed，不能延后 block claim retirement。
    removed.storage.close();
    Ok(())
}

/// activated area 的容量/空闲页投影；不计 header、坏页；retained swapcache 槽仍由该 owner 计作占用。
pub(crate) fn statistics() -> (usize, usize) {
    registry()
        .table
        .lock()
        .areas
        .iter()
        .flatten()
        .fold((0, 0), |(total, free), area| {
            (total + area.capacity, free + area.state.lock().free_pages)
        })
}

/// 在锁外格式化 /proc/swaps；有限 stack snapshot 只保活既有 owners，不分配集合。
pub(crate) fn write_snapshot(output: &mut impl core::fmt::Write) -> core::fmt::Result {
    let areas = registry().table.lock().areas.clone();
    writeln!(output, "Filename\t\t\t\tType\t\tSize\t\tUsed\t\tPriority")?;
    for area in areas.iter().flatten() {
        let free = area.state.lock().free_pages;
        for &byte in &area.path {
            match byte {
                0..=b' ' | b'\\' | 0x7f..=0xff => write!(output, "\\{:03o}", byte)?,
                _ => output.write_char(char::from(byte))?,
            }
        }
        writeln!(
            output,
            "\t{}\t{}\t{}\t{}",
            if area.storage.is_file() {
                "file"
            } else {
                "partition"
            },
            area.capacity * 4,
            (area.capacity - free) * 4,
            area.priority
        )?;
    }
    Ok(())
}
