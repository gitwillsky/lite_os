use alloc::vec::Vec;

pub(super) const MAX_FILE_DESCRIPTORS: usize = 1_048_576;

const LEVELS: usize = 4;
const WORD_BITS: usize = usize::BITS as usize;
const _: () = assert!(
    MAX_FILE_DESCRIPTORS <= WORD_BITS * WORD_BITS * WORD_BITS * WORD_BITS,
    "four free-slot index levels must cover the fd domain"
);

/// @description 与 IndexedSlots 同锁拥有的四层 empty-slot 搜索索引。
///
/// level 0 每个 set bit 对应 materialized empty slot；上层 bit 表示 child word
/// 非零。slot vector 仍是语义事实；若两者分裂则 fail-stop，禁止猜测后重复发布
/// occupied fd 或永久隐藏可复用 fd。
struct FreeSlotIndex {
    levels: [Vec<usize>; LEVELS],
    // All materialized slots below this conservative lower bound are occupied.
    // Too-high publication would hide a reusable fd; release therefore lowers it,
    // occupy only advances it by one proven slot, and fork clones it with the bits.
    occupied_prefix: usize,
}

impl FreeSlotIndex {
    fn new() -> Self {
        Self {
            levels: core::array::from_fn(|_| Vec::new()),
            occupied_prefix: 0,
        }
    }

    fn try_clone(&self) -> Result<Self, ()> {
        let mut cloned = Self::new();
        for (target, source) in cloned.levels.iter_mut().zip(&self.levels) {
            target.try_reserve_exact(source.len()).map_err(|_| ())?;
            target.extend_from_slice(source);
        }
        cloned.occupied_prefix = self.occupied_prefix;
        Ok(cloned)
    }

    fn words_for_slots(slots: usize) -> [usize; LEVELS] {
        assert!(slots <= MAX_FILE_DESCRIPTORS);
        let mut words = [0; LEVELS];
        let mut bits = slots;
        for count in &mut words {
            *count = bits.div_ceil(WORD_BITS);
            bits = *count;
        }
        assert!(bits <= 1, "free-slot index hierarchy is too shallow");
        words
    }

    fn try_reserve_slots(&mut self, slots: usize) -> Result<(), ()> {
        for (level, required) in self.levels.iter_mut().zip(Self::words_for_slots(slots)) {
            if required > level.len() {
                level.try_reserve(required - level.len()).map_err(|_| ())?;
            }
        }
        Ok(())
    }

    fn grow(&mut self, old_slots: usize, new_slots: usize) {
        assert!(old_slots <= new_slots);
        assert!(self.occupied_prefix <= old_slots);
        let required = Self::words_for_slots(new_slots);
        for (level, length) in self.levels.iter_mut().zip(required) {
            level.resize(length, 0);
        }
        if old_slots == new_slots {
            return;
        }

        let first_word = old_slots / WORD_BITS;
        let last_word = (new_slots - 1) / WORD_BITS;
        for word in first_word..=last_word {
            let first_bit = if word == first_word {
                old_slots % WORD_BITS
            } else {
                0
            };
            let end_bit = if word == last_word {
                (new_slots - 1) % WORD_BITS + 1
            } else {
                WORD_BITS
            };
            let below_end = if end_bit == WORD_BITS {
                usize::MAX
            } else {
                (1usize << end_bit) - 1
            };
            self.levels[0][word] |= (usize::MAX << first_bit) & below_end;
            self.propagate(0, word);
        }
    }

    fn propagate(&mut self, mut level: usize, mut child_word: usize) {
        while level + 1 < LEVELS {
            let child_nonempty = self.levels[level][child_word] != 0;
            let parent_level = level + 1;
            let parent_word = child_word / WORD_BITS;
            let parent_bit = 1usize << (child_word % WORD_BITS);
            let before = self.levels[parent_level][parent_word];
            if child_nonempty {
                self.levels[parent_level][parent_word] |= parent_bit;
            } else {
                self.levels[parent_level][parent_word] &= !parent_bit;
            }
            if self.levels[parent_level][parent_word] == before {
                break;
            }
            level = parent_level;
            child_word = parent_word;
        }
    }

    fn first_free(&self, minimum: usize, slots: usize) -> Option<usize> {
        let mut level = 0;
        let mut start = minimum.max(self.occupied_prefix);
        if start >= slots {
            return None;
        }
        loop {
            let word_index = start / WORD_BITS;
            let word = *self.levels[level].get(word_index)? & (usize::MAX << (start % WORD_BITS));
            if word != 0 {
                let mut found = word_index * WORD_BITS + word.trailing_zeros() as usize;
                while level != 0 {
                    level -= 1;
                    let word = *self.levels[level]
                        .get(found)
                        .expect("free-slot summary child index is out of bounds");
                    assert_ne!(word, 0, "free-slot summary points at an empty child word");
                    found = found * WORD_BITS + word.trailing_zeros() as usize;
                }
                return Some(found);
            }
            level += 1;
            if level == LEVELS {
                return None;
            }
            start = word_index + 1;
        }
    }

    fn assert_occupied(&self, fd: usize) {
        let word = fd / WORD_BITS;
        let bit = 1usize << (fd % WORD_BITS);
        assert_eq!(
            self.levels[0][word] & bit,
            0,
            "occupied fd has a free index bit"
        );
    }

    fn occupy(&mut self, fd: usize) {
        let word = fd / WORD_BITS;
        let bit = 1usize << (fd % WORD_BITS);
        assert_ne!(
            self.levels[0][word] & bit,
            0,
            "fd publication targeted a non-free index slot"
        );
        self.levels[0][word] &= !bit;
        assert!(fd >= self.occupied_prefix);
        if fd == self.occupied_prefix {
            self.occupied_prefix += 1;
        }
        self.propagate(0, word);
    }

    fn release(&mut self, fd: usize) {
        self.assert_occupied(fd);
        let word = fd / WORD_BITS;
        let bit = 1usize << (fd % WORD_BITS);
        self.levels[0][word] |= bit;
        self.occupied_prefix = self.occupied_prefix.min(fd);
        self.propagate(0, word);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SlotInsertError {
    Limit,
    OutOfMemory,
}

/// @description `Vec<Option<T>>` 与 free-slot index 的唯一复合 owner。
///
/// Caller 只能经本 interface 改变 occupancy；所有 reserve 先于 logical grow，
/// 每次 empty/occupied transition 同步验证并更新 bitmap/prefix。
pub(super) struct IndexedSlots<T> {
    entries: Vec<Option<T>>,
    free: FreeSlotIndex,
}

impl<T> IndexedSlots<T> {
    pub(super) fn new() -> Self {
        Self {
            entries: Vec::new(),
            free: FreeSlotIndex::new(),
        }
    }

    fn ensure_len(&mut self, length: usize) -> Result<(), SlotInsertError> {
        if length <= self.entries.len() {
            return Ok(());
        }
        self.free
            .try_reserve_slots(length)
            .map_err(|_| SlotInsertError::OutOfMemory)?;
        let old_length = self.entries.len();
        self.entries
            .try_reserve(length - old_length)
            .map_err(|_| SlotInsertError::OutOfMemory)?;
        self.entries.resize_with(length, || None);
        self.free.grow(old_length, length);
        Ok(())
    }

    fn publish_empty(&mut self, fd: usize, value: T) {
        assert!(self.entries[fd].is_none());
        self.free.occupy(fd);
        self.entries[fd] = Some(value);
    }

    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(super) fn get(&self, fd: usize) -> Option<&T> {
        self.entries.get(fd)?.as_ref()
    }

    pub(super) fn get_mut(&mut self, fd: usize) -> Option<&mut T> {
        self.entries.get_mut(fd)?.as_mut()
    }

    pub(super) fn insert_with(
        &mut self,
        minimum: usize,
        limit: usize,
        make: impl FnOnce() -> T,
    ) -> Result<usize, SlotInsertError> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        if minimum >= limit {
            return Err(SlotInsertError::Limit);
        }
        let fd = self
            .free
            .first_free(minimum, self.entries.len())
            .unwrap_or(self.entries.len().max(minimum));
        if fd >= limit {
            return Err(SlotInsertError::Limit);
        }
        if fd >= self.entries.len() {
            self.ensure_len(fd + 1)?;
        }
        self.publish_empty(fd, make());
        Ok(fd)
    }

    pub(super) fn insert_pair_with(
        &mut self,
        limit: usize,
        make: impl FnOnce() -> (T, T),
    ) -> Result<(usize, usize), SlotInsertError> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        let first_fd = self
            .free
            .first_free(0, self.entries.len())
            .unwrap_or(self.entries.len());
        if first_fd >= limit {
            return Err(SlotInsertError::Limit);
        }
        let second_fd = self
            .free
            .first_free(first_fd + 1, self.entries.len())
            .unwrap_or(self.entries.len().max(first_fd + 1));
        if second_fd >= limit {
            return Err(SlotInsertError::Limit);
        }
        if second_fd >= self.entries.len() {
            self.ensure_len(second_fd + 1)?;
        }
        let (first, second) = make();
        self.publish_empty(first_fd, first);
        self.publish_empty(second_fd, second);
        Ok((first_fd, second_fd))
    }

    pub(super) fn take(&mut self, fd: usize) -> Option<T> {
        let value = self.entries.get_mut(fd)?.take()?;
        self.free.release(fd);
        Some(value)
    }

    pub(super) fn take_if(&mut self, fd: usize, predicate: impl FnOnce(&T) -> bool) -> Option<T> {
        if !predicate(self.entries.get(fd)?.as_ref()?) {
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
        if fd >= limit.min(MAX_FILE_DESCRIPTORS) {
            return Err(SlotInsertError::Limit);
        }
        if fd >= self.entries.len() {
            self.ensure_len(fd + 1)?;
        }
        let occupied = self.entries[fd].is_some();
        if occupied {
            self.free.assert_occupied(fd);
        }
        let value = make();
        if !occupied {
            self.free.occupy(fd);
        }
        Ok(self.entries[fd].replace(value))
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = (usize, &T)> {
        self.entries
            .iter()
            .enumerate()
            .filter_map(|(fd, entry)| entry.as_ref().map(|entry| (fd, entry)))
    }

    pub(super) fn take_all(&mut self) -> Self {
        core::mem::replace(self, Self::new())
    }
}

impl<T: Clone> IndexedSlots<T> {
    pub(super) fn try_clone(&self) -> Result<Self, ()> {
        let free = self.free.try_clone()?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| ())?;
        entries.extend(self.entries.iter().cloned());
        Ok(Self { entries, free })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grow(index: &mut FreeSlotIndex, free: &mut Vec<bool>, length: usize) {
        index.try_reserve_slots(length).unwrap();
        let old = free.len();
        index.grow(old, length);
        free.resize(length, true);
    }

    fn expected(free: &[bool], minimum: usize) -> Option<usize> {
        free.iter()
            .enumerate()
            .skip(minimum)
            .find_map(|(fd, free)| free.then_some(fd))
    }

    fn assert_queries(index: &FreeSlotIndex, free: &[bool], minima: &[usize]) {
        for &minimum in minima {
            assert_eq!(
                index.first_free(minimum, free.len()),
                expected(free, minimum)
            );
        }
    }

    #[test]
    fn hierarchy_boundaries_and_churn_match_a_linear_model() {
        let mut index = FreeSlotIndex::new();
        let mut free = Vec::new();
        grow(&mut index, &mut free, MAX_FILE_DESCRIPTORS);
        let holes = [
            0,
            63,
            64,
            65,
            4095,
            4096,
            4097,
            262_143,
            262_144,
            MAX_FILE_DESCRIPTORS - 1,
        ];
        for (fd, is_free) in free.iter_mut().enumerate() {
            if holes.binary_search(&fd).is_err() {
                index.occupy(fd);
                *is_free = false;
            }
        }
        assert_queries(
            &index,
            &free,
            &[
                0,
                1,
                63,
                64,
                66,
                4095,
                4096,
                4098,
                262_143,
                262_144,
                262_145,
                MAX_FILE_DESCRIPTORS - 1,
                MAX_FILE_DESCRIPTORS,
            ],
        );
        for &fd in &holes {
            index.occupy(fd);
            free[fd] = false;
        }
        assert_eq!(index.first_free(0, free.len()), None);
        for &fd in holes.iter().rev() {
            index.release(fd);
            free[fd] = true;
        }
        assert_queries(&index, &free, &holes);

        let mut state = 0xD1B5_4A32_D192_ED03u64;
        for _ in 0..20_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let fd = state as usize % free.len();
            if free[fd] {
                index.occupy(fd);
            } else {
                index.release(fd);
            }
            free[fd] = !free[fd];
            let minimum = state.rotate_left(29) as usize % (free.len() + 1);
            assert_eq!(
                index.first_free(minimum, free.len()),
                expected(&free, minimum)
            );
        }
    }

    #[test]
    fn dense_growth_clone_and_low_close_preserve_prefix_semantics() {
        const SLOTS: usize = 32_768;
        let mut slots = IndexedSlots::new();
        for fd in 0..SLOTS {
            assert_eq!(slots.insert_with(0, SLOTS, || fd), Ok(fd));
        }
        assert_eq!(
            slots.insert_with(0, SLOTS, || SLOTS),
            Err(SlotInsertError::Limit)
        );

        let mut cloned = slots.try_clone().unwrap();
        for fd in [16_384, 64, 0] {
            assert_eq!(cloned.take(fd), Some(fd));
            assert_eq!(cloned.insert_with(0, SLOTS, || fd), Ok(fd));
        }
        assert_eq!(slots.get(0), Some(&0));
    }

    #[test]
    fn pair_replace_take_if_and_take_all_share_one_occupancy_owner() {
        let mut slots = IndexedSlots::new();
        assert_eq!(slots.insert_pair_with(8, || (10, 11)), Ok((0, 1)));
        assert_eq!(slots.take(0), Some(10));
        assert_eq!(slots.insert_pair_with(8, || (20, 21)), Ok((0, 2)));
        assert_eq!(slots.take(1), Some(11));
        assert_eq!(slots.take(2), Some(21));
        assert_eq!(slots.insert_pair_with(8, || (30, 31)), Ok((1, 2)));

        assert_eq!(slots.replace_with(1, 8, || 40), Ok(Some(30)));
        assert_eq!(slots.take(2), Some(31));
        assert_eq!(slots.replace_with(2, 8, || 41), Ok(None));
        *slots.get_mut(2).unwrap() = 42;
        assert_eq!(slots.get(2), Some(&42));
        assert_eq!(slots.take_if(1, |value| *value == 99), None);
        assert_eq!(slots.take_if(1, |value| *value == 40), Some(40));

        let len = slots.len();
        let constructed = core::cell::Cell::new(false);
        assert_eq!(
            slots.insert_with(8, 8, || {
                constructed.set(true);
                50
            }),
            Err(SlotInsertError::Limit)
        );
        assert_eq!(
            slots.replace_with(8, 8, || {
                constructed.set(true);
                50
            }),
            Err(SlotInsertError::Limit)
        );
        assert!(!constructed.get());
        assert_eq!(slots.len(), len);

        let mut limited = IndexedSlots::new();
        assert_eq!(
            limited.insert_pair_with(1, || {
                constructed.set(true);
                (1, 2)
            }),
            Err(SlotInsertError::Limit)
        );
        assert!(!constructed.get());
        assert_eq!(limited.len(), 0);

        let mut sparse = IndexedSlots::new();
        assert_eq!(sparse.insert_with(7, 16, || 70), Ok(7));
        assert_eq!(sparse.insert_with(5, 16, || 50), Ok(5));
        assert_eq!(sparse.insert_with(0, 16, || 0), Ok(0));

        let live: Vec<_> = slots.iter().map(|(fd, value)| (fd, *value)).collect();
        let taken = slots.take_all();
        assert_eq!(slots.len(), 0);
        assert_eq!(
            taken
                .iter()
                .map(|(fd, value)| (fd, *value))
                .collect::<Vec<_>>(),
            live
        );
    }
}
