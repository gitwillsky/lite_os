use alloc::vec::Vec;

use super::MAX_FILE_DESCRIPTORS;

const LEVELS: usize = 4;
const WORD_BITS: usize = usize::BITS as usize;
const _: () = assert!(
    MAX_FILE_DESCRIPTORS <= WORD_BITS * WORD_BITS * WORD_BITS * WORD_BITS,
    "four free-slot index levels must cover the fd domain"
);

/// @description 与 FileDescriptorTable 同锁拥有的四层 empty-slot 搜索索引。
///
/// level 0 每个 set bit 对应 materialized empty slot；上层 bit 表示 child word
/// 非零。slot vector 仍是语义事实；若两者分裂则 fail-stop，禁止猜测后重复发布
/// occupied fd 或永久隐藏可复用 fd。
pub(super) struct FreeSlotIndex {
    levels: [Vec<usize>; LEVELS],
    // All materialized slots below this conservative lower bound are occupied.
    // Too-high publication would hide a reusable fd; release therefore lowers it,
    // occupy only advances it by one proven slot, and fork clones it with the bits.
    occupied_prefix: usize,
}

impl FreeSlotIndex {
    /// @description 构造尚未 materialize 任何 slot 的规范空索引。
    /// @return 无 backing allocation 的四层 index owner。
    pub(super) fn new() -> Self {
        Self {
            levels: core::array::from_fn(|_| Vec::new()),
            occupied_prefix: 0,
        }
    }

    /// @description fallible 复制每层 logical bits，供 fork 同事务复制 fd table。
    /// @return 成功时返回与 source 查询结果相同的独立 index；OOM 不修改 source。
    pub(super) fn try_clone(&self) -> Result<Self, ()> {
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

    /// @description 为目标 slot length 的所有层预留摊销增长容量，不改变 logical bits。
    /// @param slots publication 后的 materialized slot length。
    /// @return reserve 成功返回零值；任一层 OOM 返回错误且 logical index 不变。
    pub(super) fn try_reserve_slots(&mut self, slots: usize) -> Result<(), ()> {
        for (level, required) in self.levels.iter_mut().zip(Self::words_for_slots(slots)) {
            if required > level.len() {
                level.try_reserve(required - level.len()).map_err(|_| ())?;
            }
        }
        Ok(())
    }

    /// @description reserve 成功后把新增 materialized slots 原子标记为空闲并发布 summary。
    /// @param old_slots 当前 slot vector length。
    /// @param new_slots 新 slot vector length，必须不小于 old_slots。
    pub(super) fn grow(&mut self, old_slots: usize, new_slots: usize) {
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

    /// @description 从已证明 occupied prefix 与 minimum 沿 hierarchy 返回最低 free slot。
    /// @param minimum caller 要求的 inclusive descriptor lower bound。
    /// @param slots 当前 FileDescriptorTable materialized slot length。
    /// @return 已有空洞的最低 fd；当前 materialized suffix 全满则为 None。
    pub(super) fn first_free(&self, minimum: usize, slots: usize) -> Option<usize> {
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

    /// @description entry publication 前把一个已证明 free 的 slot 标为 occupied。
    /// @param fd 当前 level-0 bit 必须为 set 的 materialized slot。
    pub(super) fn occupy(&mut self, fd: usize) {
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

    /// @description entry detach 后把一个已证明 occupied 的 slot 标为 free。
    /// @param fd 当前 level-0 bit 必须为 clear 的 materialized slot。
    pub(super) fn release(&mut self, fd: usize) {
        let word = fd / WORD_BITS;
        let bit = 1usize << (fd % WORD_BITS);
        assert_eq!(
            self.levels[0][word] & bit,
            0,
            "fd detach targeted an already-free index slot"
        );
        self.levels[0][word] |= bit;
        self.occupied_prefix = self.occupied_prefix.min(fd);
        self.propagate(0, word);
    }
}
