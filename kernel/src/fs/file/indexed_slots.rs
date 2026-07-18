use alloc::boxed::Box;

pub(super) const MAX_FILE_DESCRIPTORS: usize = 1_048_576;

const ROOT_BITS: usize = 7;
const BRANCH_BITS: usize = 7;
const CHUNK_BITS: usize = 6;
const ROOT_FANOUT: usize = 1 << ROOT_BITS;
const BRANCH_FANOUT: usize = 1 << BRANCH_BITS;
const CHUNK_SLOTS: usize = 1 << CHUNK_BITS;
const SUMMARY_WORDS: usize = ROOT_FANOUT / usize::BITS as usize;
const ROOT_SPAN: usize = BRANCH_FANOUT * CHUNK_SLOTS;
#[cfg(test)]
const RADIX_LEVELS: usize = 3;
const _: () = assert!(
    ROOT_FANOUT * BRANCH_FANOUT * CHUNK_SLOTS == MAX_FILE_DESCRIPTORS,
    "fd radix must cover the exact descriptor domain"
);
const _: () = assert!(
    ROOT_FANOUT == BRANCH_FANOUT && ROOT_FANOUT.is_multiple_of(usize::BITS as usize),
    "radix fullness summaries require complete machine words"
);

struct SlotChunk<T> {
    entries: [Option<T>; CHUNK_SLOTS],
}

impl<T> SlotChunk<T> {
    fn new() -> Self {
        Self {
            entries: core::array::from_fn(|_| None),
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.iter().all(Option::is_none)
    }

    fn is_full(&self) -> bool {
        self.entries.iter().all(Option::is_some)
    }
}

struct RadixBranch<T> {
    chunks: [Option<Box<SlotChunk<T>>>; BRANCH_FANOUT],
    full_chunks: [usize; SUMMARY_WORDS],
}

impl<T> RadixBranch<T> {
    fn new() -> Self {
        Self {
            chunks: core::array::from_fn(|_| None),
            full_chunks: [0; SUMMARY_WORDS],
        }
    }

    fn is_empty(&self) -> bool {
        self.chunks.iter().all(Option::is_none)
    }

    fn is_full(&self) -> bool {
        self.full_chunks.iter().all(|word| *word == usize::MAX)
    }
}

struct RadixRoot<T> {
    branches: [Option<Box<RadixBranch<T>>>; ROOT_FANOUT],
    full_branches: [usize; SUMMARY_WORDS],
}

impl<T> RadixRoot<T> {
    fn new() -> Self {
        Self {
            branches: core::array::from_fn(|_| None),
            full_branches: [0; SUMMARY_WORDS],
        }
    }

    fn is_empty(&self) -> bool {
        self.branches.iter().all(Option::is_none)
    }
}

/// @description 与 sparse radix 同步维护的 lowest-free summary owner。
///
/// Fullness bits 保存在对应 radix node，避免第二棵 dense bitmap；本 owner 只保留已有的
/// conservative occupied prefix。所有 occupancy transition 必须经 `IndexedSlots`，否则
/// full summary 会隐藏空 slot 或把 occupied slot 重复发布。
struct FreeSlotIndex {
    occupied_prefix: usize,
}

impl FreeSlotIndex {
    const fn new() -> Self {
        Self { occupied_prefix: 0 }
    }

    fn first_free<T>(
        &self,
        root: Option<&RadixRoot<T>>,
        minimum: usize,
        slots: usize,
    ) -> Option<usize> {
        Self::search(root, minimum.max(self.occupied_prefix), slots)
    }

    fn search<T>(root: Option<&RadixRoot<T>>, minimum: usize, slots: usize) -> Option<usize> {
        if minimum >= slots {
            return None;
        }
        let Some(root) = root else {
            return Some(minimum);
        };
        let mut root_minimum = minimum / ROOT_SPAN;
        while let Some(root_index) = first_clear(&root.full_branches, root_minimum) {
            let root_base = root_index * ROOT_SPAN;
            if root_base >= slots {
                return None;
            }
            let local_minimum = minimum.max(root_base);
            let Some(branch) = root.branches[root_index].as_deref() else {
                return Some(local_minimum);
            };
            let mut chunk_minimum = (local_minimum - root_base) / CHUNK_SLOTS;
            while let Some(chunk_index) = first_clear(&branch.full_chunks, chunk_minimum) {
                let chunk_base = root_base + chunk_index * CHUNK_SLOTS;
                if chunk_base >= slots {
                    return None;
                }
                let slot_minimum = local_minimum.saturating_sub(chunk_base).min(CHUNK_SLOTS);
                let Some(chunk) = branch.chunks[chunk_index].as_deref() else {
                    return Some(chunk_base + slot_minimum);
                };
                if let Some(slot) =
                    (slot_minimum..CHUNK_SLOTS).find(|slot| chunk.entries[*slot].is_none())
                {
                    let fd = chunk_base + slot;
                    return (fd < slots).then_some(fd);
                }
                chunk_minimum = chunk_index + 1;
            }
            root_minimum = root_index + 1;
        }
        None
    }

    fn occupy<T>(&mut self, root: &mut RadixRoot<T>, fd: usize, slots: usize) {
        refresh_fullness(root, fd);
        if fd == self.occupied_prefix {
            self.occupied_prefix = Self::search(Some(root), fd + 1, slots).unwrap_or(slots);
        }
    }

    fn release<T>(&mut self, root: &mut RadixRoot<T>, fd: usize) {
        refresh_fullness(root, fd);
        self.occupied_prefix = self.occupied_prefix.min(fd);
    }
}

fn first_clear(summary: &[usize; SUMMARY_WORDS], start: usize) -> Option<usize> {
    if start >= ROOT_FANOUT {
        return None;
    }
    let word_bits = usize::BITS as usize;
    let mut word_index = start / word_bits;
    let mut mask = usize::MAX << (start % word_bits);
    while word_index < summary.len() {
        let available = !summary[word_index] & mask;
        if available != 0 {
            return Some(word_index * word_bits + available.trailing_zeros() as usize);
        }
        word_index += 1;
        mask = usize::MAX;
    }
    None
}

fn set_summary(summary: &mut [usize; SUMMARY_WORDS], index: usize, full: bool) {
    let word_bits = usize::BITS as usize;
    let word = index / word_bits;
    let bit = 1usize << (index % word_bits);
    if full {
        summary[word] |= bit;
    } else {
        summary[word] &= !bit;
    }
}

fn coordinates(fd: usize) -> (usize, usize, usize) {
    (
        fd / ROOT_SPAN,
        fd / CHUNK_SLOTS % BRANCH_FANOUT,
        fd % CHUNK_SLOTS,
    )
}

fn refresh_fullness<T>(root: &mut RadixRoot<T>, fd: usize) {
    let (root_index, chunk_index, _) = coordinates(fd);
    let branch = root.branches[root_index]
        .as_deref_mut()
        .expect("occupied fd lost its radix branch");
    let chunk = branch.chunks[chunk_index]
        .as_deref()
        .expect("occupied fd lost its slot chunk");
    set_summary(&mut branch.full_chunks, chunk_index, chunk.is_full());
    set_summary(&mut root.full_branches, root_index, branch.is_full());
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotInsertError {
    Limit,
    OutOfMemory,
}

/// @description sparse fixed-depth radix 与 lowest-free summary 的唯一复合 owner。
///
/// 7/7/6-bit 路径把 lookup/replace/take 固定为三层；root、branch、64-slot chunk 全部在
/// occupancy publication 前 fallible prepare。`logical_len` 保留 Linux FDSize 语义，关闭
/// 高 fd 只回收空 radix path，不收缩已公布的 table capacity。
pub(super) struct IndexedSlots<T> {
    root: Option<Box<RadixRoot<T>>>,
    logical_len: usize,
    free: FreeSlotIndex,
}

impl<T> IndexedSlots<T> {
    pub(super) const fn new() -> Self {
        Self {
            root: None,
            logical_len: 0,
            free: FreeSlotIndex::new(),
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.logical_len
    }

    pub(super) fn get(&self, fd: usize) -> Option<&T> {
        if fd >= self.logical_len {
            return None;
        }
        let (root, chunk, slot) = coordinates(fd);
        self.root.as_deref()?.branches[root].as_deref()?.chunks[chunk]
            .as_deref()?
            .entries[slot]
            .as_ref()
    }

    pub(super) fn get_mut(&mut self, fd: usize) -> Option<&mut T> {
        if fd >= self.logical_len {
            return None;
        }
        let (root, chunk, slot) = coordinates(fd);
        self.root.as_deref_mut()?.branches[root]
            .as_deref_mut()?
            .chunks[chunk]
            .as_deref_mut()?
            .entries[slot]
            .as_mut()
    }

    pub(super) fn insert_with(
        &mut self,
        minimum: usize,
        limit: usize,
        make: impl FnOnce() -> T,
    ) -> Result<usize, SlotInsertError> {
        let mut allocation = || Ok(());
        self.insert_with_allocation(minimum, limit, make, &mut allocation)
    }

    fn insert_with_allocation(
        &mut self,
        minimum: usize,
        limit: usize,
        make: impl FnOnce() -> T,
        allocation: &mut impl FnMut() -> Result<(), ()>,
    ) -> Result<usize, SlotInsertError> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        if minimum >= limit {
            return Err(SlotInsertError::Limit);
        }
        let fd = self
            .free
            .first_free(self.root.as_deref(), minimum, self.logical_len)
            .unwrap_or(self.logical_len.max(minimum));
        if fd >= limit {
            return Err(SlotInsertError::Limit);
        }
        self.ensure_chunk(fd, allocation)?;
        self.publish_empty(fd, make());
        Ok(fd)
    }

    pub(super) fn insert_pair_with(
        &mut self,
        limit: usize,
        make: impl FnOnce() -> (T, T),
    ) -> Result<(usize, usize), SlotInsertError> {
        let mut allocation = || Ok(());
        self.insert_pair_with_allocation(limit, make, &mut allocation)
    }

    fn insert_pair_with_allocation(
        &mut self,
        limit: usize,
        make: impl FnOnce() -> (T, T),
        allocation: &mut impl FnMut() -> Result<(), ()>,
    ) -> Result<(usize, usize), SlotInsertError> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        let first = self
            .free
            .first_free(self.root.as_deref(), 0, self.logical_len)
            .unwrap_or(self.logical_len);
        if first >= limit {
            return Err(SlotInsertError::Limit);
        }
        let second = self
            .free
            .first_free(self.root.as_deref(), first + 1, self.logical_len)
            .unwrap_or(self.logical_len.max(first + 1));
        if second >= limit {
            return Err(SlotInsertError::Limit);
        }
        self.ensure_chunk(first, allocation)?;
        if let Err(error) = self.ensure_chunk(second, allocation) {
            self.prune_empty_path(first);
            return Err(error);
        }
        let (first_value, second_value) = make();
        self.publish_empty(first, first_value);
        self.publish_empty(second, second_value);
        Ok((first, second))
    }

    fn ensure_chunk(
        &mut self,
        fd: usize,
        allocation: &mut impl FnMut() -> Result<(), ()>,
    ) -> Result<(), SlotInsertError> {
        let (root_index, chunk_index, _) = coordinates(fd);
        if self.root.is_none() {
            let chunk = try_box(SlotChunk::new(), allocation)?;
            let mut branch = try_box(RadixBranch::new(), allocation)?;
            branch.chunks[chunk_index] = Some(chunk);
            let mut root = try_box(RadixRoot::new(), allocation)?;
            root.branches[root_index] = Some(branch);
            self.root = Some(root);
            return Ok(());
        }
        let root = self.root.as_deref_mut().unwrap();
        if root.branches[root_index].is_none() {
            let chunk = try_box(SlotChunk::new(), allocation)?;
            let mut branch = try_box(RadixBranch::new(), allocation)?;
            branch.chunks[chunk_index] = Some(chunk);
            root.branches[root_index] = Some(branch);
            return Ok(());
        }
        let branch = root.branches[root_index].as_deref_mut().unwrap();
        if branch.chunks[chunk_index].is_none() {
            branch.chunks[chunk_index] = Some(try_box(SlotChunk::new(), allocation)?);
        }
        Ok(())
    }

    fn publish_empty(&mut self, fd: usize, value: T) {
        self.logical_len = self.logical_len.max(fd + 1);
        let (root_index, chunk_index, slot) = coordinates(fd);
        let root = self.root.as_deref_mut().unwrap();
        let entry = &mut root.branches[root_index].as_deref_mut().unwrap().chunks[chunk_index]
            .as_deref_mut()
            .unwrap()
            .entries[slot];
        assert!(entry.is_none(), "fd publication targeted an occupied slot");
        *entry = Some(value);
        self.free.occupy(root, fd, self.logical_len);
    }

    pub(super) fn take(&mut self, fd: usize) -> Option<T> {
        if fd >= self.logical_len {
            return None;
        }
        let (root_index, chunk_index, slot) = coordinates(fd);
        let root = self.root.as_deref_mut()?;
        let value = root.branches[root_index].as_deref_mut()?.chunks[chunk_index]
            .as_deref_mut()?
            .entries[slot]
            .take()?;
        self.free.release(root, fd);
        self.prune_empty_path(fd);
        Some(value)
    }

    pub(super) fn take_if(&mut self, fd: usize, predicate: impl FnOnce(&T) -> bool) -> Option<T> {
        if !predicate(self.get(fd)?) {
            return None;
        }
        self.take(fd)
    }

    pub(super) fn replace_with(
        &mut self,
        fd: usize,
        limit: usize,
        make: impl FnOnce() -> T,
    ) -> Result<Option<T>, SlotInsertError> {
        let mut allocation = || Ok(());
        self.replace_with_allocation(fd, limit, make, &mut allocation)
    }

    fn replace_with_allocation(
        &mut self,
        fd: usize,
        limit: usize,
        make: impl FnOnce() -> T,
        allocation: &mut impl FnMut() -> Result<(), ()>,
    ) -> Result<Option<T>, SlotInsertError> {
        if fd >= limit.min(MAX_FILE_DESCRIPTORS) {
            return Err(SlotInsertError::Limit);
        }
        if self.get(fd).is_some() {
            let previous = core::mem::replace(self.get_mut(fd).unwrap(), make());
            return Ok(Some(previous));
        }
        self.ensure_chunk(fd, allocation)?;
        self.publish_empty(fd, make());
        Ok(None)
    }

    /// @description 按 fd 递增遍历 occupied entries，不读取未物化 chunk 的 slot array。
    pub(super) fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.root.as_deref().into_iter().flat_map(|root| {
            root.branches
                .iter()
                .enumerate()
                .filter_map(|(root_index, branch)| {
                    branch.as_deref().map(|branch| (root_index, branch))
                })
                .flat_map(|(root_index, branch)| {
                    branch
                        .chunks
                        .iter()
                        .enumerate()
                        .filter_map(move |(chunk_index, chunk)| {
                            chunk
                                .as_deref()
                                .map(|chunk| (root_index, chunk_index, chunk))
                        })
                })
                .flat_map(|(root_index, chunk_index, chunk)| {
                    chunk
                        .entries
                        .iter()
                        .enumerate()
                        .filter_map(move |(slot, entry)| {
                            entry.as_ref().map(|value| {
                                (
                                    root_index * ROOT_SPAN + chunk_index * CHUNK_SLOTS + slot,
                                    value,
                                )
                            })
                        })
                })
        })
    }

    /// @description 从 minimum 开始只扫描 materialized radix path，返回首个匹配 fd。
    pub(super) fn find_from(
        &self,
        minimum: usize,
        predicate: impl Fn(&T) -> bool,
    ) -> Option<usize> {
        if minimum >= self.logical_len {
            return None;
        }
        let (first_root, first_chunk, first_slot) = coordinates(minimum);
        let root = self.root.as_deref()?;
        for root_index in first_root..ROOT_FANOUT {
            let Some(branch) = root.branches[root_index].as_deref() else {
                continue;
            };
            let chunk_start = if root_index == first_root {
                first_chunk
            } else {
                0
            };
            for chunk_index in chunk_start..BRANCH_FANOUT {
                let Some(chunk) = branch.chunks[chunk_index].as_deref() else {
                    continue;
                };
                let slot_start = if root_index == first_root && chunk_index == first_chunk {
                    first_slot
                } else {
                    0
                };
                for slot in slot_start..CHUNK_SLOTS {
                    let fd = root_index * ROOT_SPAN + chunk_index * CHUNK_SLOTS + slot;
                    if fd >= self.logical_len {
                        return None;
                    }
                    if chunk.entries[slot].as_ref().is_some_and(&predicate) {
                        return Some(fd);
                    }
                }
            }
        }
        None
    }

    pub(super) fn take_all(&mut self) -> Self {
        core::mem::replace(self, Self::new())
    }

    fn prune_empty_path(&mut self, fd: usize) {
        let (root_index, chunk_index, _) = coordinates(fd);
        let Some(root) = self.root.as_deref_mut() else {
            return;
        };
        let Some(branch) = root.branches[root_index].as_deref_mut() else {
            return;
        };
        if branch.chunks[chunk_index]
            .as_deref()
            .is_some_and(SlotChunk::is_empty)
        {
            branch.chunks[chunk_index] = None;
        }
        if branch.is_empty() {
            root.branches[root_index] = None;
        }
        if root.is_empty() {
            self.root = None;
        }
    }
}

impl<T: Clone> IndexedSlots<T> {
    /// @description 只克隆 include 选中的 materialized entries，并保留 logical capacity。
    /// @errors 任一 radix allocation 失败时析构完整 partial clone，source 保持不变。
    pub(super) fn try_clone_where(&self, include: impl Fn(&T) -> bool) -> Result<Self, ()> {
        let mut allocation = || Ok(());
        self.try_clone_where_with_allocation(include, &mut allocation)
    }

    fn try_clone_where_with_allocation(
        &self,
        include: impl Fn(&T) -> bool,
        allocation: &mut impl FnMut() -> Result<(), ()>,
    ) -> Result<Self, ()> {
        let mut cloned = Self::new();
        for (fd, value) in self.iter().filter(|(_, value)| include(value)) {
            cloned
                .replace_with_allocation(fd, MAX_FILE_DESCRIPTORS, || value.clone(), allocation)
                .map_err(|_| ())?;
        }
        cloned.logical_len = self.logical_len;
        Ok(cloned)
    }
}

fn try_box<T>(
    value: T,
    allocation: &mut impl FnMut() -> Result<(), ()>,
) -> Result<Box<T>, SlotInsertError> {
    allocation().map_err(|_| SlotInsertError::OutOfMemory)?;
    Box::try_new(value).map_err(|_| SlotInsertError::OutOfMemory)
}

#[cfg(test)]
#[path = "indexed_slots/tests.rs"]
mod tests;
