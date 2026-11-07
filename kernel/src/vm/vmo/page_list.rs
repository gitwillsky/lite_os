//! VMO 的已提交页列表：按 16 页一块的稀疏数组，块再用有序表索引（Zircon `VmPageList` 的结构）。
//!
//! 每页一个有序表节点（约 64 字节）对稠密的文件/堆区是纯开销，区间扫描还要逐节点追指针。
//! 分块后稠密区间每页约 28 字节，区间操作变成块内数组遍历。
//!
//! # 可失败与不可失败
//!
//! 页的插入可能需要新块（分配）。持锁的提交阶段不允许失败，所以需要“准备可失败、提交不失败”的调用者先用
//! [`PageList::reserve_range`] 把区间内的块建好（可失败，改动前完成），再用 [`PageList::insert_reserved`]
//! 无分配地放入。块在最后一页移除时立即释放，列表不保留空块。

use fallible_tree::FallibleMap;

use super::Page;

/// 一块覆盖的页数。16 页（64 KiB）既让稠密区间的元数据摊薄，也不会在稀疏访问时浪费过多。
pub(super) const CHUNK_PAGES: usize = 16;

struct Chunk {
    slots: [Option<Page>; CHUNK_PAGES],
    // 已占用槽数；为零的块立即从索引中移除。
    used: u8,
}

impl Chunk {
    /// 槽号不小于 `from` 的第一页，连同它的页号（块号为 `key`）。
    fn first_from(&self, key: usize, from: usize) -> Option<(usize, &Page)> {
        self.slots[from..]
            .iter()
            .enumerate()
            .find_map(|(slot, page)| {
                page.as_ref()
                    .map(|page| (key * CHUNK_PAGES + from + slot, page))
            })
    }

    fn empty() -> Self {
        Self {
            slots: [const { None }; CHUNK_PAGES],
            used: 0,
        }
    }
}

/// 已提交页列表。
pub(super) struct PageList {
    chunks: FallibleMap<usize, Chunk>,
    pages: usize,
}

impl Default for PageList {
    fn default() -> Self {
        Self::new()
    }
}

impl PageList {
    pub(super) const fn new() -> Self {
        Self {
            chunks: FallibleMap::new(),
            pages: 0,
        }
    }

    /// 已提交页数。
    pub(super) fn len(&self) -> usize {
        self.pages
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.pages == 0
    }

    pub(super) fn get(&self, index: &usize) -> Option<&Page> {
        self.chunks
            .get(&(index / CHUNK_PAGES))?
            .slots
            .get(index % CHUNK_PAGES)?
            .as_ref()
    }

    pub(super) fn get_mut(&mut self, index: &usize) -> Option<&mut Page> {
        self.chunks
            .get_mut(&(index / CHUNK_PAGES))?
            .slots
            .get_mut(index % CHUNK_PAGES)?
            .as_mut()
    }

    pub(super) fn contains_key(&self, index: &usize) -> bool {
        self.get(index).is_some()
    }

    /// 按页号升序遍历全部已提交页。
    pub(super) fn values(&self) -> impl Iterator<Item = &Page> {
        self.iter_from(&0).map(|(_, page)| page)
    }

    /// 从页号 `start`（含）起按升序遍历 `(页号, 页)`。
    pub(super) fn iter_from(&self, start: &usize) -> impl Iterator<Item = (usize, &Page)> {
        let start = *start;
        self.chunks
            .iter_from(&(start / CHUNK_PAGES))
            .flat_map(|(&chunk, content)| {
                content
                    .slots
                    .iter()
                    .enumerate()
                    .filter_map(move |(slot, page)| {
                        page.as_ref().map(|page| (chunk * CHUNK_PAGES + slot, page))
                    })
            })
            .filter(move |(index, _)| *index >= start)
    }

    /// 页号不小于 `index` 的最小已提交页。
    pub(super) fn ceiling(&self, index: &usize) -> Option<(usize, &Page)> {
        let chunk = index / CHUNK_PAGES;
        if let Some(content) = self.chunks.get(&chunk)
            && let Some(found) = content.first_from(chunk, index % CHUNK_PAGES)
        {
            return Some(found);
        }
        // 块里没有不小于它的页：下一个非空块的第一页（空块不保留，所以必在槽里）。
        let (&next, content) = self.chunks.ceiling(&(chunk + 1))?;
        content.first_from(next, 0)
    }

    /// 页号最小的已提交页。
    pub(super) fn first_key_value(&self) -> Option<(usize, &Page)> {
        self.ceiling(&0)
    }

    /// 对全部已提交页调用 `visit(页号, 页)`。
    pub(super) fn for_each_mut(&mut self, mut visit: impl FnMut(usize, &mut Page)) {
        self.chunks.for_each_mut(|&chunk, content| {
            for (slot, page) in content.slots.iter_mut().enumerate() {
                if let Some(page) = page {
                    visit(chunk * CHUNK_PAGES + slot, page);
                }
            }
        });
    }

    /// 为 `[first, first + count)` 建好块，使其中任意页之后都能被 [`Self::insert_reserved`] 无分配地放入。
    ///
    /// # Errors
    /// 分配失败；已建的块保留（它们不占页，不影响一致性），列表内容不变。
    pub(super) fn reserve_range(&mut self, first: usize, count: usize) -> Result<(), ()> {
        if count == 0 {
            return Ok(());
        }
        for chunk in first / CHUNK_PAGES..=(first + count - 1) / CHUNK_PAGES {
            self.reserve_chunk(chunk)?;
        }
        Ok(())
    }

    /// 为 `source` 中页号在 `[offset, offset + visible)` 内的每一页建好其平移 `-offset` 之后的块。
    ///
    /// 用于把另一个列表的页并入本列表之前的预留：只为真实存在的页建块，不按区间长度建块
    /// （稀疏的大对象不会因此分配海量空块）。
    ///
    /// # Errors
    /// 分配失败；已建的块保留，列表内容不变。
    pub(super) fn reserve_for_shifted(
        &mut self,
        source: &PageList,
        offset: usize,
        visible: usize,
    ) -> Result<(), ()> {
        let end = offset.saturating_add(visible);
        let mut last = usize::MAX;
        for (index, _) in source.iter_from(&offset) {
            if index >= end {
                break;
            }
            let chunk = (index - offset) / CHUNK_PAGES;
            if chunk != last {
                self.reserve_chunk(chunk)?;
                last = chunk;
            }
        }
        Ok(())
    }

    fn reserve_chunk(&mut self, chunk: usize) -> Result<(), ()> {
        if self.chunks.contains_key(&chunk) {
            return Ok(());
        }
        let entry = FallibleMap::try_prepare(chunk, Chunk::empty()).map_err(|_| ())?;
        self.chunks.commit_vacant(entry);
        Ok(())
    }

    /// 插入一页，必要时分配块。已有页被替换并返回。
    ///
    /// # Errors
    /// 分配失败；`page` 随错误丢弃。
    pub(super) fn try_insert(&mut self, index: usize, page: Page) -> Result<Option<Page>, ()> {
        self.reserve_chunk(index / CHUNK_PAGES)?;
        Ok(self.insert_reserved(index, page))
    }

    /// 无分配地插入一页；其块必须已由 [`Self::reserve_range`] 建好。已有页被替换并返回。
    ///
    /// # Panics
    /// 块不存在——调用者违反了“先预留再提交”的契约。
    pub(super) fn insert_reserved(&mut self, index: usize, page: Page) -> Option<Page> {
        let chunk = self
            .chunks
            .get_mut(&(index / CHUNK_PAGES))
            .expect("page chunk was not reserved before the commit phase");
        let previous = chunk.slots[index % CHUNK_PAGES].replace(page);
        if previous.is_none() {
            chunk.used += 1;
            self.pages += 1;
        }
        previous
    }

    /// 移除一页；块空了就释放块。
    pub(super) fn remove(&mut self, index: &usize) -> Option<Page> {
        let key = index / CHUNK_PAGES;
        let chunk = self.chunks.get_mut(&key)?;
        let page = chunk.slots[index % CHUNK_PAGES].take()?;
        chunk.used -= 1;
        self.pages -= 1;
        if chunk.used == 0 {
            drop(self.chunks.remove(&key));
        }
        Some(page)
    }
}
