use alloc::{sync::Arc, vec::Vec};

const CAPACITY: usize = 64;

struct Entry {
    block: u32,
    bytes: Arc<Vec<u8>>,
    last_use: u64,
}

/// Filesystem-owned bounded cache for decoded-as-bytes directory and indirect-pointer blocks.
pub(super) struct MetadataBlockCache {
    entries: [Option<Entry>; CAPACITY],
    clock: u64,
    generation: u64,
}

impl MetadataBlockCache {
    pub(super) const fn new() -> Self {
        Self {
            entries: [const { None }; CAPACITY],
            clock: 0,
            generation: 0,
        }
    }

    pub(super) fn get(&mut self, block: u32) -> Option<Arc<Vec<u8>>> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let entry = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.block == block)?;
        entry.last_use = clock;
        Some(entry.bytes.clone())
    }

    pub(super) const fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn insert_if_unchanged(&mut self, generation: u64, block: u32, bytes: Arc<Vec<u8>>) {
        if self.generation != generation {
            return;
        }
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        if let Some(entry) = self
            .entries
            .iter_mut()
            .flatten()
            .find(|entry| entry.block == block)
        {
            entry.last_use = clock;
            return;
        }
        let slot_index = self
            .entries
            .iter()
            .position(Option::is_none)
            .or_else(|| {
                self.entries
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, entry)| entry.as_ref().map_or(0, |entry| entry.last_use))
                    .map(|(index, _)| index)
            })
            .expect("metadata cache has a non-zero fixed capacity");
        self.entries[slot_index] = Some(Entry {
            block,
            bytes,
            last_use: clock,
        });
    }

    pub(super) fn update_if_present(&mut self, block: u32, bytes: &[u8]) {
        self.generation = self.generation.wrapping_add(1);
        let Some(slot_index) = self
            .entries
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.block == block))
        else {
            return;
        };
        if let Some(entry) = self.entries[slot_index].as_mut() {
            if let Some(cached) = Arc::get_mut(&mut entry.bytes) {
                cached.copy_from_slice(bytes);
            } else {
                self.entries[slot_index] = None;
            }
        }
    }

    pub(super) fn invalidate(&mut self, block: u32) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(slot) = self
            .entries
            .iter_mut()
            .find(|entry| entry.as_ref().is_some_and(|entry| entry.block == block))
        {
            *slot = None;
        }
    }

    pub(super) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.fill_with(|| None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(byte: u8) -> Arc<Vec<u8>> {
        Arc::try_new(vec![byte; 16]).unwrap()
    }

    #[test]
    fn capacity_is_bounded_and_reclaims_lru_identity() {
        let mut cache = MetadataBlockCache::new();
        for block in 0..CAPACITY as u32 {
            cache.insert_if_unchanged(cache.generation(), block, image(block as u8));
        }
        assert_eq!(cache.get(0).unwrap()[0], 0);
        cache.insert_if_unchanged(cache.generation(), CAPACITY as u32, image(0xff));
        assert!(cache.get(1).is_none());
        assert_eq!(cache.get(0).unwrap()[0], 0);
        assert_eq!(cache.get(CAPACITY as u32).unwrap()[0], 0xff);
        assert_eq!(cache.entries.iter().flatten().count(), CAPACITY);
    }

    #[test]
    fn write_with_outstanding_reader_invalidates_old_identity() {
        let mut cache = MetadataBlockCache::new();
        cache.insert_if_unchanged(cache.generation(), 7, image(1));
        let reader = cache.get(7).unwrap();
        cache.update_if_present(7, &[2; 16]);
        assert_eq!(reader[0], 1);
        assert!(cache.get(7).is_none());
    }

    #[test]
    fn generation_change_rejects_stale_miss_admission() {
        let mut cache = MetadataBlockCache::new();
        let generation = cache.generation();
        cache.invalidate(9);
        cache.insert_if_unchanged(generation, 9, image(3));
        assert!(cache.get(9).is_none());
    }
}
