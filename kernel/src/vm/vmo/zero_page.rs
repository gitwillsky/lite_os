//! 共享零页：未提交的匿名页被读取时映射全局只读零页，写入时才分配真实页。
//!
//! 许多匿名映射（BSS、`calloc`、预留的堆区）大部分页只会被读；为每个读缺页分配并清零一页既浪费内存又浪费清零时间
//! （Linux `zero_page`、Zircon 读缺页同样共享零页）。

use super::*;
use spin::Once;

// OWNER: 全局唯一的零页，由连续 VMO 持有，因此计入既有的“连续”内存类别而不产生未归属的页。
// 内核与设备从不写它：它只作为只读翻译出现，`pin`（DMA）与一切写解析都走真实页，缺失这一点会让零页被改写、
// 此后所有未写页读到脏数据。
static ZERO_PAGE: Once<Arc<Vmo>> = Once::new();

/// 创建共享零页；堆与帧分配器就绪后调用一次。失败时读缺页退回逐页分配。
pub(crate) fn initialize() {
    if let Ok(vmo) = Vmo::create_contiguous(PAGE_SIZE as u64) {
        ZERO_PAGE.call_once(|| vmo);
    }
}

fn shared_zero_page() -> Option<PhysicalPageNumber> {
    match &ZERO_PAGE.get()?.kind {
        Kind::Contiguous(frames) => Some(frames.ppn),
        _ => None,
    }
}

impl Vmo {
    /// 读解析是否可以用共享零页：只限没有祖先的根匿名对象。
    ///
    /// 有祖先的对象，后到的祖先写入必须对读者可见，而祖先的 zap 覆盖不到把零页映射在子对象里的翻译；
    /// pager 对象的未提交页要向 pager 请求内容，不是零。
    pub(super) fn shared_zero_locked(
        &self,
        state: &mut State,
        write: bool,
    ) -> Option<ResolvedPage> {
        if write
            || state.relation.is_some()
            || !matches!(
                self.kind,
                Kind::Paged {
                    pager: None,
                    content: PageContent::Anonymous,
                    ..
                }
            )
        {
            return None;
        }
        let ppn = shared_zero_page()?;
        state.zero_mapped = true;
        Some(ResolvedPage {
            source: FaultSource::Memory,
            ppn,
            writable: false,
            cache: CachePolicy::Cached,
        })
    }

    /// 刚在 `index` 提交了真实页：若此前有读缺页把该下标映射到零页，撤销那条翻译。
    ///
    /// 其他下标可能仍映射零页，所以标志保持；撤销需要在释放状态锁之后进行。
    pub(super) fn retire_zero_translation(
        self: &Arc<Self>,
        state: spin::MutexGuard<'_, State>,
        index: usize,
    ) -> Result<(), Resolve> {
        let stale = state.zero_mapped;
        drop(state);
        if stale {
            self.zap(index..index + 1, ZapMode::Unmap)
                .map_err(Resolve::Error)?;
        }
        Ok(())
    }
}
