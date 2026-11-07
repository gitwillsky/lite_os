//! VMO 的已提交页列表：按 64 页一块的稀疏数组，块再用有序表索引（Zircon `VmPageList` 的结构）。
//!
//! 每页一个有序表节点（约 64 字节）对稠密的文件/堆区是纯开销，区间扫描还要逐节点追指针。
//! 已提交页和补页请求分别使用同一分块索引；压缩页 owner 与 Arc 请求槽均为 8 字节，区间操作变成块内数组遍历。
//!
//! # 可失败与不可失败
//!
//! 页的插入可能需要新块（分配）。持锁的提交阶段不允许失败，所以需要“准备可失败、提交不失败”的调用者先用
//! [`PageList::reserve_range`] 把区间内的块建好（可失败，改动前完成），再用 [`PageList::insert_reserved`]
//! 无分配地放入。块在最后一槽移除时立即释放；仅准备与提交之间允许临时空块，放弃准备时清理。

use alloc::{boxed::Box, sync::Arc};
use fallible_tree::FallibleMap;

mod aging;
use aging::{ChunkAges, GenerationIndex};

use super::{Page, PageRequestCompletion};

/// 一个 u64 位图覆盖的页块；把分代链表的固定开销分摊到 64 个槽。
pub(super) const CHUNK_PAGES: usize = 64;

/// 列表槽的回收身份。请求和匿名页没有分代成员资格；存储页只在干净且未固定时入队。
pub(super) trait Slot {
    fn reclaim_generation(&self) -> Option<u8>;
}
impl Slot for Page {
    fn reclaim_generation(&self) -> Option<u8> {
        self.is_reclaimable().then(|| self.generation())
    }
}
impl Slot for Arc<PageRequestCompletion> {
    fn reclaim_generation(&self) -> Option<u8> {
        None
    }
}

struct Chunk<T> {
    slots: [Option<T>; CHUNK_PAGES],
    // 已占用槽数；为零的块立即从索引中移除。
    used: u8,
    // OWNER: 仅 pager 的已提交页块有分代成员位图/链表；请求、匿名和 bootfs 不分配它。
    ages: Option<Box<ChunkAges>>,
}

impl<T> Chunk<T> {
    /// 槽号不小于 `from` 的第一页，连同它的页号（块号为 `key`）。
    fn first_from(&self, key: usize, from: usize) -> Option<(usize, &T)> {
        self.slots[from..]
            .iter()
            .enumerate()
            .find_map(|(slot, page)| {
                page.as_ref()
                    .map(|page| (key * CHUNK_PAGES + from + slot, page))
            })
    }

    fn try_empty(tracked: bool) -> Result<Self, ()> {
        Ok(Self {
            slots: [const { None }; CHUNK_PAGES],
            used: 0,
            ages: if tracked {
                Some(Box::try_new(ChunkAges::new()).map_err(|_| ())?)
            } else {
                None
            },
        })
    }
}

/// VMO 已提交页或在途请求的分块列表；两者使用相同的准备/提交契约。
pub(super) struct PageList<T: Slot = Page> {
    chunks: FallibleMap<usize, Chunk<T>>,
    pages: usize,
    // OWNER: 分代队列唯一索引；槽与索引只在 PageList 修改事务内一起变更，不把可变槽借给调用者。
    generations: Option<Box<GenerationIndex>>,
}

impl<T: Slot> Default for PageList<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Slot> PageList<T> {
    pub(super) const fn new() -> Self {
        Self {
            chunks: FallibleMap::new(),
            pages: 0,
            generations: None,
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

    pub(super) fn get(&self, index: &usize) -> Option<&T> {
        self.chunks
            .get(&(index / CHUNK_PAGES))?
            .slots
            .get(index % CHUNK_PAGES)?
            .as_ref()
    }

    /// 修改槽且同步回收成员资格；闭包不能把可变槽借用带出事务，不允许绕过索引更新。
    pub(super) fn with_mut<R>(
        &mut self,
        index: &usize,
        update: impl FnOnce(&mut T) -> R,
    ) -> Option<R> {
        let tracked = self.generations.is_some();
        let page =
            self.chunks.get_mut(&(index / CHUNK_PAGES))?.slots[index % CHUNK_PAGES].as_mut()?;
        let before = if tracked {
            page.reclaim_generation()
        } else {
            None
        };
        let result = update(page);
        let after = if tracked {
            page.reclaim_generation()
        } else {
            None
        };
        self.refresh_generation(*index, before, after);
        Some(result)
    }

    pub(super) fn contains_key(&self, index: &usize) -> bool {
        self.get(index).is_some()
    }

    /// 按页号升序遍历全部已提交页。
    pub(super) fn values(&self) -> impl Iterator<Item = &T> {
        self.iter_from(&0).map(|(_, page)| page)
    }

    /// 从页号 `start`（含）起按升序遍历 `(页号, 页)`。
    pub(super) fn iter_from(&self, start: &usize) -> impl Iterator<Item = (usize, &T)> {
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
    pub(super) fn ceiling(&self, index: &usize) -> Option<(usize, &T)> {
        let first = index / CHUNK_PAGES;
        for (&chunk, content) in self.chunks.iter_from(&first) {
            let from = if chunk == first {
                index % CHUNK_PAGES
            } else {
                0
            };
            if let Some(found) = content.first_from(chunk, from) {
                return Some(found);
            }
        }
        None
    }

    /// 页号最小的已提交页。
    pub(super) fn first_key_value(&self) -> Option<(usize, &T)> {
        self.ceiling(&0)
    }

    /// 有界区间修改；按块更新位图，仅成员资格变化的块需要改变代队列链接。
    pub(super) fn for_each_range_mut(
        &mut self,
        range: core::ops::Range<usize>,
        mut visit: impl FnMut(usize, &mut T),
    ) {
        let mut cursor = range.start / CHUNK_PAGES;
        while let Some(key) = self.chunks.ceiling(&cursor).map(|(key, _)| *key) {
            if key > range.end.saturating_sub(1) / CHUNK_PAGES || range.is_empty() {
                break;
            }
            let changes = {
                let chunk = self.chunks.get_mut(&key).expect("range chunk disappeared");
                let before = chunk.ages.as_ref().map(|ages| ages.masks);
                for (slot, page) in chunk.slots.iter_mut().enumerate() {
                    let index = key * CHUNK_PAGES + slot;
                    if range.contains(&index)
                        && let Some(page) = page
                    {
                        visit(index, page);
                    }
                }
                before.map(|before| {
                    let mut after = [0u64; 8];
                    for (slot, page) in chunk.slots.iter().enumerate() {
                        if let Some(generation) = page.as_ref().and_then(Slot::reclaim_generation) {
                            after[generation as usize] |= 1 << slot;
                        }
                    }
                    chunk.ages.as_mut().expect("tracked chunk lost ages").masks = after;
                    (before, after)
                })
            };
            if let Some((before, after)) = changes {
                for (generation, (before, after)) in before.into_iter().zip(after).enumerate() {
                    if before != 0 && after == 0 {
                        self.unlink_generation(key, generation);
                    }
                    if before == 0 && after != 0 {
                        self.link_generation(key, generation);
                    }
                }
            }
            cursor = key + 1;
        }
    }

    #[cfg(test)]
    pub(super) fn for_each_mut(&mut self, visit: impl FnMut(usize, &mut T)) {
        self.for_each_range_mut(0..usize::MAX, visit);
    }

    /// 为 `[first, first + count)` 建好块，使其中任意页之后都能被 [`Self::insert_reserved`] 无分配地放入。
    ///
    /// # Errors
    /// 区间溢出或分配失败；清理空的预留块，既有槽位不变。
    pub(super) fn reserve_range(&mut self, first: usize, count: usize) -> Result<(), ()> {
        if count == 0 {
            return Ok(());
        }
        let last = first.checked_add(count - 1).ok_or(())?;
        for chunk in first / CHUNK_PAGES..=last / CHUNK_PAGES {
            if self.reserve_chunk(chunk).is_err() {
                self.discard_empty_chunks();
                return Err(());
            }
        }
        Ok(())
    }

    /// 为 `source` 中页号在 `[offset, offset + visible)` 内的每一页建好其平移 `-offset` 之后的块。
    ///
    /// 用于把另一个列表的页并入本列表之前的预留：只为真实存在的页建块，不按区间长度建块
    /// （稀疏的大对象不会因此分配海量空块）。
    ///
    /// # Errors
    /// 分配失败；清理空的预留块，既有槽位不变。
    pub(super) fn reserve_for_shifted(
        &mut self,
        source: &PageList<T>,
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
                if self.reserve_chunk(chunk).is_err() {
                    self.discard_empty_chunks();
                    return Err(());
                }
                last = chunk;
            }
        }
        Ok(())
    }

    /// 预留失败或上层提交放弃时归还空块，不改变任何已发表槽位；不分配。
    pub(super) fn discard_empty_chunks(&mut self) {
        let mut cursor = 0;
        while let Some((&key, chunk)) = self.chunks.ceiling(&cursor) {
            let empty = chunk.used == 0;
            if empty {
                drop(self.chunks.remove(&key));
            }
            let Some(next) = key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
    }

    fn reserve_chunk(&mut self, chunk: usize) -> Result<(), ()> {
        if self.chunks.contains_key(&chunk) {
            return Ok(());
        }
        let content = Chunk::try_empty(self.generations.is_some())?;
        let entry = FallibleMap::try_prepare(chunk, content).map_err(|_| ())?;
        self.chunks.commit_vacant(entry);
        Ok(())
    }

    /// 插入一页，必要时分配块。已有页被替换并返回。
    ///
    /// # Errors
    /// 分配失败；`page` 随错误丢弃。
    pub(super) fn try_insert(&mut self, index: usize, page: T) -> Result<Option<T>, ()> {
        self.reserve_chunk(index / CHUNK_PAGES)?;
        Ok(self.insert_reserved(index, page))
    }

    /// 无分配地插入一页；其块必须已由 [`Self::reserve_range`] 建好。已有页被替换并返回。
    ///
    /// # Panics
    /// 块不存在——调用者违反了“先预留再提交”的契约。
    pub(super) fn insert_reserved(&mut self, index: usize, page: T) -> Option<T> {
        let tracked = self.generations.is_some();
        let after = if tracked {
            page.reclaim_generation()
        } else {
            None
        };
        let chunk = self
            .chunks
            .get_mut(&(index / CHUNK_PAGES))
            .expect("page chunk was not reserved before the commit phase");
        let previous = chunk.slots[index % CHUNK_PAGES].replace(page);
        let before = if tracked {
            previous.as_ref().and_then(Slot::reclaim_generation)
        } else {
            None
        };
        if previous.is_none() {
            chunk.used += 1;
            self.pages += 1;
        }
        self.refresh_generation(index, before, after);
        previous
    }

    /// 移除一槽；先退出分代队列，再释放最后一个槽所在的块。
    pub(super) fn remove(&mut self, index: &usize) -> Option<T> {
        let key = index / CHUNK_PAGES;
        let chunk = self.chunks.get_mut(&key)?;
        let page = chunk.slots[index % CHUNK_PAGES].take()?;
        chunk.used -= 1;
        let empty = chunk.used == 0;
        self.pages -= 1;
        self.refresh_generation(*index, page.reclaim_generation(), None);
        if empty {
            drop(self.chunks.remove(&key));
        }
        Some(page)
    }
}
