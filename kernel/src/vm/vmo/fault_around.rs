//! fault-around：缺页时顺带只读装入同一对齐窗口内已驻留的相邻文件页，摊销顺序访问的缺页成本。
//!
//! 只收集不安装：安装在 `AddressSpace::install_around`，两者由 [`Vmo::with_resolved_around`] 放进主页解析的
//! 同一树锁事务，回收的 zap 因此不可能夹在“收集”和“安装”之间。

use super::*;

/// 一次缺页顺带装入相邻已驻留页的最大页数（同时是对齐窗口大小）。
///
/// 64 KiB 窗口与 Linux fault-around 的默认值同量级：足以把顺序访问的缺页次数降一个数量级，
/// 又不会在稀疏访问时映射大量未用页。
pub(crate) const FAULT_AROUND_PAGES: usize = 16;

impl Vmo {
    /// 收集 `range` 中已驻留的文件页（fault-around）；调用者持树锁。
    ///
    /// 只返回对象自身或祖先链上 pager 支撑的、已驻留的页，且一律按只读解析：不分配、不请求 pager、
    /// 不复制。`skip` 是已由主缺页处理的页索引。返回数组前 `count` 项有效。
    pub(super) fn collect_resident_file_pages_locked(
        self: &Arc<Self>,
        range: core::ops::Range<usize>,
        skip: usize,
    ) -> [Option<(usize, ResolvedPage)>; FAULT_AROUND_PAGES] {
        debug_assert!(range.len() <= FAULT_AROUND_PAGES);
        let mut pages = [None; FAULT_AROUND_PAGES];
        let mut count = 0;
        for index in range.filter(|&index| index != skip) {
            if let Some(page) = self.peek_resident_file_locked(index) {
                pages[count] = Some((index, page));
                count += 1;
            }
        }
        pages
    }

    /// 只读地查看一页是否已驻留于 pager 支撑的对象；调用者持树锁。
    fn peek_resident_file_locked(self: &Arc<Self>, index: usize) -> Option<ResolvedPage> {
        let resident = |ppn| ResolvedPage {
            source: FaultSource::Memory,
            ppn,
            writable: false,
            cache: CachePolicy::Cached,
        };
        match &self.kind {
            Kind::Slice => {
                let (parent, offset) = {
                    let state = self.state.lock();
                    if index >= state.pages {
                        return None;
                    }
                    match &state.relation {
                        Some(Relation::Slice { parent, offset }) => (parent.clone(), *offset),
                        _ => unreachable!("slice VMO without slice parent"),
                    }
                };
                return parent.peek_resident_file_locked(index + offset);
            }
            Kind::Paged { .. } | Kind::Hidden => {}
            Kind::Physical { .. } | Kind::Contiguous(_) => return None,
        }
        let state = self.state.lock();
        if index >= state.pages {
            return None;
        }
        let file_backed = |vmo: &Vmo| matches!(vmo.kind, Kind::Paged { pager: Some(_), .. });
        if let Some(page) = state.committed.get(&index) {
            // 自身已提交的页若不是文件页（私有写时复制副本、匿名页），映射为只读会让之后的写
            // 多一次缺页，且它不是 fault-around 要摊销的 pager 补页成本。
            return file_backed(self).then(|| resident(page.frame.ppn));
        }
        let mut ancestor = match &state.relation {
            Some(Relation::Cow {
                parent,
                offset,
                limit,
            }) if index < *limit => Some((parent.clone(), index + offset)),
            _ => None,
        };
        while let Some((vmo, ancestor_index)) = ancestor {
            let ancestor_state = vmo.state.lock();
            if ancestor_index >= ancestor_state.pages {
                return None;
            }
            if let Some(page) = ancestor_state.committed.get(&ancestor_index) {
                return file_backed(&vmo).then(|| resident(page.frame.ppn));
            }
            let next = match &ancestor_state.relation {
                Some(Relation::Cow {
                    parent,
                    offset,
                    limit,
                }) if ancestor_index < *limit => Some((parent.clone(), ancestor_index + offset)),
                _ => None,
            };
            drop(ancestor_state);
            ancestor = next;
        }
        None
    }

    /// [`Self::with_resolved`]，并在主页解析成功的同一树锁事务内把 `around` 窗口中相邻的已驻留文件页
    /// 交给 `with_around`（fault-around，摊销顺序访问的缺页成本）。
    ///
    /// 相邻页只来自对象自身或祖先链上 pager 支撑的、已驻留的页：不分配、不请求 pager、不复制，且只读。
    /// 与主页同事务意味着回收的 zap（需要同一把树锁）不可能夹在“收集”和“安装”之间，
    /// 否则安装会留下指向已退休页框的悬空翻译。
    ///
    /// # Parameters
    ///
    /// - `around`: 页索引窗口，长度不超过 [`FAULT_AROUND_PAGES`]；`None` 表示不做 fault-around。
    /// - `with_around`: 在持树锁时执行，接收主页的结果与相邻页，必须在其中安装翻译。
    ///
    /// # Errors
    /// 越界、分配失败、pager 失败或等待被打断。
    pub(crate) fn with_resolved_around<R>(
        self: &Arc<Self>,
        index: usize,
        write: bool,
        around: Option<core::ops::Range<usize>>,
        mut with_page: impl FnMut(ResolvedPage) -> Result<R, VmError>,
        with_around: impl FnOnce(&mut R, &[Option<(usize, ResolvedPage)>]),
    ) -> Result<R, VmError> {
        let mut source = FaultSource::Memory;
        loop {
            let pending = {
                let _tree = self.lock_tree()?;
                match self.resolve_locked(index, write) {
                    Ok(mut page) => {
                        page.source = source;
                        let mut result = with_page(page)?;
                        if let Some(range) = around {
                            let pages = self.collect_resident_file_pages_locked(range, index);
                            with_around(&mut result, &pages);
                        }
                        return Ok(result);
                    }
                    Err(Resolve::Error(error)) => return Err(error),
                    Err(Resolve::NeedPage { vmo, index }) => {
                        let request = vmo.request_page(index)?;
                        (vmo, request)
                    }
                }
            };
            pending.0.wait_for_page(&pending.1)?;
            source = FaultSource::Pager;
        }
    }
}
