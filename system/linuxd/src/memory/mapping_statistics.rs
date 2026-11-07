//! Linux 映射驻留的字节/定点归属；原生物理来源在此投影为 smaps 分类。
use super::PAGE_SIZE;
use native_runtime::native_abi::{VmarMappingStats, residency};

/// smaps 的驻留归属；普通字段为字节，pss 字段保留原生逐页分摊的 12 位小数。
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MappingStatistics {
    pub(crate) rss: u64,
    pub(crate) pss: u64,
    pub(crate) pss_dirty: u64,
    pub(crate) shared_clean: u64,
    pub(crate) shared_dirty: u64,
    pub(crate) private_clean: u64,
    pub(crate) private_dirty: u64,
    pub(crate) anonymous: u64,
    pub(crate) pss_anon: u64,
    pub(crate) pss_file: u64,
    pub(crate) pss_shmem: u64,
}

impl MappingStatistics {
    pub(super) fn account(&mut self, range: &VmarMappingStats, bytes: u64, shared_anonymous: bool) {
        let anonymous = range.flags & residency::ANONYMOUS != 0 && !shared_anonymous;
        let shmem = shared_anonymous || range.flags & residency::VOLATILE_PAGER != 0;
        let dirty = range.flags & residency::DIRTY != 0;
        let shared = range.flags & residency::SHARED != 0;
        self.rss += bytes;
        let pss = range.pss_per_page * (bytes / PAGE_SIZE as u64);
        self.pss += pss;
        if dirty {
            self.pss_dirty += pss;
        }
        *match (shared, dirty) {
            (false, false) => &mut self.private_clean,
            (false, true) => &mut self.private_dirty,
            (true, false) => &mut self.shared_clean,
            (true, true) => &mut self.shared_dirty,
        } += bytes;
        if anonymous {
            self.anonymous += bytes;
            self.pss_anon += pss;
        } else if shmem {
            self.pss_shmem += pss;
        } else {
            self.pss_file += pss;
        }
    }

    /// rollup 先累加字节与定点量，文本输出时才转 KiB，避免逐 VMA 截断损失。
    pub(crate) fn add(&mut self, other: &Self) {
        self.rss += other.rss;
        self.pss += other.pss;
        self.pss_dirty += other.pss_dirty;
        self.shared_clean += other.shared_clean;
        self.shared_dirty += other.shared_dirty;
        self.private_clean += other.private_clean;
        self.private_dirty += other.private_dirty;
        self.anonymous += other.anonymous;
        self.pss_anon += other.pss_anon;
        self.pss_file += other.pss_file;
        self.pss_shmem += other.pss_shmem;
    }
}
