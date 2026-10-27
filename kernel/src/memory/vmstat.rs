//! Linux `global_node_page_state` 的全局页状态计数：每项只在其页 owner 的状态转换处增减。
//!
//! 1. 计数单位是 4KiB 页；各 owner 在自己的锁或原子转换内得出 0↔1 等精确边沿后才调用本模块。
//! 2. 读取是各项独立的 Relaxed 快照，与 Linux `/proc/meminfo` 一样不保证跨项同一时刻。
//!
//! 缺失任一 owner 的调用时，对应 meminfo 字段会永久漂移，而不是瞬时误差。

use core::sync::atomic::{AtomicUsize, Ordering};

/// 一个全局页状态项；对应 Linux `enum node_stat_item` 的同名语义。
#[derive(Debug, Clone, Copy)]
pub(in crate::memory) enum PageState {
    /// `NR_ANON_MAPPED`：至少被一个映射安装的私有匿名页，每个物理页只计一次。
    AnonMapped,
    /// `NR_FILE_MAPPED`：至少被一个映射安装的文件、shmem 与 `vm_insert_page` 驱动页。
    FileMapped,
    /// `NR_SHMEM`：驻留的 tmpfs、memfd、SysV/共享匿名页与 GEM shmem 页。
    Shmem,
    /// `NR_PAGETABLE`：页表页。
    PageTables,
}

// OWNER: 每个计数只由 PageState 对应页 owner 的转换路径修改；Relaxed 足够，因为读者只需要单项数值，
// 不依赖它与页内容或其他计数的发布顺序。缺失时 meminfo 只能报告 0 或扫描全部 MM。
static COUNTS: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// 增加 `pages` 个页到 `item`。
pub(in crate::memory) fn add(item: PageState, pages: usize) {
    COUNTS[item as usize].fetch_add(pages, Ordering::Relaxed);
}

/// 从 `item` 减去 `pages` 个页；下溢表示 owner 的增减不对称。
pub(in crate::memory) fn sub(item: PageState, pages: usize) {
    let previous = COUNTS[item as usize].fetch_sub(pages, Ordering::Relaxed);
    debug_assert!(previous >= pages, "{item:?} page state underflow");
}

/// 按布尔转换增减一页：`false→true` 加一，`true→false` 减一。
pub(in crate::memory) fn transition(item: PageState, before: bool, after: bool) {
    match (before, after) {
        (false, true) => add(item, 1),
        (true, false) => sub(item, 1),
        _ => {}
    }
}

/// `/proc/meminfo` 使用的全局页状态快照，单位为页。
#[derive(Debug, Clone, Copy)]
pub(crate) struct PageStateSnapshot {
    pub(crate) anon_mapped: usize,
    pub(crate) file_mapped: usize,
    pub(crate) shmem: usize,
    pub(crate) page_tables: usize,
}

/// 读取全部全局页状态项。
///
/// # Returns
///
/// 各项独立的 Relaxed 快照，单位为页。
pub(crate) fn snapshot() -> PageStateSnapshot {
    let read = |item: PageState| COUNTS[item as usize].load(Ordering::Relaxed);
    PageStateSnapshot {
        anon_mapped: read(PageState::AnonMapped),
        file_mapped: read(PageState::FileMapped),
        shmem: read(PageState::Shmem),
        page_tables: read(PageState::PageTables),
    }
}
