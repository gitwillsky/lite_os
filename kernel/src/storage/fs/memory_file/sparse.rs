//! 稀疏页存储：页本身就是文件内容（单份），读写、洞与截断语义的唯一实现。
//!
//! 与页的具体形态（内核物理页）解耦：`P` 只需读、写、清零尾部。页的分配由调用者提供的
//! `allocate` 闭包完成，因此配额、物理内存耗尽与节点分配失败各自精确地成为停止原因。

use alloc::sync::Arc;

use crate::fallible_tree::FallibleMap;

pub(super) const PAGE_SIZE: usize = 4096;

/// 一个存储页的字节访问。
pub(super) trait PageBytes {
    type Error;
    // 换入须在 byte copy 前完成；Err 不能部分修改当前页或目标切片。
    fn read(&self, offset: usize, output: &mut [u8]) -> Result<(), Self::Error>;
    fn write(&self, offset: usize, input: &[u8]) -> Result<(), Self::Error>;
    /// 把 `offset` 到页尾清零。
    fn zero_from(&self, offset: usize) -> Result<(), Self::Error>;
}

/// 一次写入停止的原因。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Stop<E, AccessError> {
    /// 目录树节点分配失败。
    OutOfMemory,
    /// 页分配失败（配额或物理内存），原因由 `allocate` 给出。
    Allocator(E),
    /// 已存在页的换入/访问失败；与配额或新页 allocation 独立。
    Access(AccessError),
}

/// 单页 lookup/allocation 的结果；外层 Option 在 EOF 外单独表示没有页。
type PageLookup<P, E> = Result<Arc<P>, Stop<E, <P as PageBytes>::Error>>;

/// 稀疏页映射与文件长度。缺失的页是洞，读为零且不占内存。
pub(super) struct SparsePages<P> {
    pages: FallibleMap<u64, Arc<P>>,
    length: u64,
}

impl<P: PageBytes> SparsePages<P> {
    pub(super) const fn new() -> Self {
        Self {
            pages: FallibleMap::new(),
            length: 0,
        }
    }

    pub(super) const fn len(&self) -> u64 {
        self.length
    }

    /// 已分配的页数。
    pub(super) const fn allocated_pages(&self) -> usize {
        self.pages.len()
    }

    /// Clone one storage owner under the caller's state lock; swap IO occurs after lock release.
    pub(super) fn next_page_from(&self, index: u64) -> Option<(u64, Arc<P>)> {
        self.pages
            .ceiling(&index)
            .map(|(&index, page)| (index, page.clone()))
    }

    /// 从 `offset` 读入 `output`，洞读为零；返回已读字节数与停止错误（EOF 截断）。
    pub(super) fn read(&self, offset: u64, output: &mut [u8]) -> (usize, Option<P::Error>) {
        let count = usize::try_from(self.length.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(output.len());
        let mut done = 0;
        while done < count {
            let position = offset + done as u64;
            let page_offset = (position % PAGE_SIZE as u64) as usize;
            let part = (PAGE_SIZE - page_offset).min(count - done);
            let target = &mut output[done..done + part];
            match self.pages.get(&(position / PAGE_SIZE as u64)) {
                Some(page) => {
                    if let Err(error) = page.read(page_offset, target) {
                        return (done, Some(error));
                    }
                }
                None => target.fill(0),
            }
            done += part;
        }
        (count, None)
    }

    /// 取得（必要时创建）第 `index` 页，不改变文件长度。
    fn page_or_allocate<E>(
        &mut self,
        index: u64,
        allocate: &mut impl FnMut() -> Result<Arc<P>, E>,
    ) -> Result<Arc<P>, Stop<E, P::Error>> {
        if let Some(page) = self.pages.get(&index) {
            return Ok(page.clone());
        }
        // 先预留目录树节点，再分配页：节点失败不浪费一个物理页；页分配失败时节点随 slot 释放。
        let slot = FallibleMap::<u64, Arc<P>>::try_reserve_node().map_err(|_| Stop::OutOfMemory)?;
        let page = allocate().map_err(Stop::Allocator)?;
        self.pages.commit_vacant(slot.fill(index, page.clone()));
        Ok(page)
    }

    /// 在 `offset` 写入 `input`，缺页按需分配；文件长度随已写入的末端增长。
    ///
    /// # Returns
    ///
    /// 已写入的字节数，以及提前停止的原因（全部写完为 `None`）。已写入的部分不回滚：
    /// 每页独立发布，部分写入与 POSIX 短写一致。
    pub(super) fn write<E>(
        &mut self,
        offset: u64,
        input: &[u8],
        mut allocate: impl FnMut() -> Result<Arc<P>, E>,
    ) -> (usize, Option<Stop<E, P::Error>>) {
        let mut done = 0;
        let mut stop = None;
        while done < input.len() {
            let position = offset + done as u64;
            let page_offset = (position % PAGE_SIZE as u64) as usize;
            let part = (PAGE_SIZE - page_offset).min(input.len() - done);
            match self.page_or_allocate(position / PAGE_SIZE as u64, &mut allocate) {
                Ok(page) => {
                    if let Err(error) = page.write(page_offset, &input[done..done + part]) {
                        stop = Some(Stop::Access(error));
                        break;
                    }
                }
                Err(reason) => {
                    stop = Some(reason);
                    break;
                }
            }
            done += part;
        }
        if done != 0 {
            self.length = self.length.max(offset + done as u64);
        }
        (done, stop)
    }

    /// 取得 mmap 用的第 `index` 页；洞在首次访问时分配零页。
    ///
    /// # Returns
    ///
    /// 页首超过文件长度返回 `None`（越过 EOF）。
    pub(super) fn page<E>(
        &mut self,
        index: u64,
        allocate: impl FnMut() -> Result<Arc<P>, E>,
    ) -> Option<PageLookup<P, E>> {
        let mut allocate = allocate;
        (index.checked_mul(PAGE_SIZE as u64)? < self.length)
            .then(|| self.page_or_allocate(index, &mut allocate))
    }

    /// 预分配 `[offset, offset + length)` 内的全部页并把文件长度提升到范围末端。
    ///
    /// # Returns
    ///
    /// 范围内不再有洞；失败时已分配的页保留（长度不变）。
    pub(super) fn allocate_range<E>(
        &mut self,
        offset: u64,
        length: u64,
        mut allocate: impl FnMut() -> Result<Arc<P>, E>,
    ) -> Result<(), Stop<E, P::Error>> {
        let end = offset.checked_add(length).ok_or(Stop::OutOfMemory)?;
        let first = offset / PAGE_SIZE as u64;
        let last = end.div_ceil(PAGE_SIZE as u64);
        for index in first..last {
            self.page_or_allocate(index, &mut allocate)?;
        }
        self.length = self.length.max(end);
        Ok(())
    }

    /// 把文件长度改为 `size`：缩小时释放整页并清零保留页的尾部，增长只产生洞。
    pub(super) fn truncate(&mut self, size: u64) -> Result<(), P::Error> {
        // 1. 唯一 fallible consequence 先发生；失败不能先删除其他完整页或改变长度。
        if !size.is_multiple_of(PAGE_SIZE as u64)
            && let Some(page) = self.pages.get(&(size / PAGE_SIZE as u64))
        {
            page.zero_from((size % PAGE_SIZE as u64) as usize)?;
        }
        // 2. tail 已可用且清零；节点移交/Drop 和长度发布均不会分配或失败。
        let first_removed = size.div_ceil(PAGE_SIZE as u64);
        let removed = self.pages.split_off(&first_removed);
        drop(removed);
        self.length = size;
        Ok(())
    }
}
