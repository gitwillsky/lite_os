#[cfg(test)]
extern crate alloc;

#[cfg(test)]
const MAX_FILE_DESCRIPTORS: usize = 1_048_576;

#[cfg(test)]
#[path = "../../../kernel/src/fs/file/free_slot_index.rs"]
mod free_slot_index;

#[cfg(test)]
mod tests {
    use super::{MAX_FILE_DESCRIPTORS, free_slot_index::FreeSlotIndex};

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
    fn grow_and_transition_across_every_hierarchy_boundary() {
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
    }

    #[test]
    fn incremental_growth_and_clone_preserve_lowest_free_semantics() {
        let mut index = FreeSlotIndex::new();
        let mut free = Vec::new();
        for length in [1, 63, 64, 65, 4095, 4096, 4097, 16_385] {
            grow(&mut index, &mut free, length);
            for fd in free.len().saturating_sub(17)..free.len() {
                if free[fd] {
                    index.occupy(fd);
                    free[fd] = false;
                }
            }
            assert_queries(&index, &free, &[0, 1, 62, 63, 64, 65, 4095, 4096, length]);
        }

        let cloned = index.try_clone().unwrap();
        assert_queries(
            &cloned,
            &free,
            &[0, 63, 64, 4095, 4096, free.len() - 1, free.len()],
        );
    }

    #[test]
    fn dense_growth_and_low_close_track_the_occupied_prefix() {
        const SLOTS: usize = 32_768;

        let mut index = FreeSlotIndex::new();
        let mut free = Vec::new();
        for fd in 0..SLOTS {
            grow(&mut index, &mut free, fd + 1);
            assert_eq!(index.first_free(0, free.len()), Some(fd));
            index.occupy(fd);
            free[fd] = false;
            assert_eq!(index.first_free(0, free.len()), None);
        }

        for fd in [16_384, 64, 0] {
            index.release(fd);
            free[fd] = true;
            assert_eq!(index.first_free(0, free.len()), expected(&free, 0));
        }
    }

    #[test]
    fn deterministic_churn_matches_a_linear_reference_model() {
        const SLOTS: usize = 8193;
        const STEPS: usize = 20_000;

        let mut index = FreeSlotIndex::new();
        let mut free = Vec::new();
        grow(&mut index, &mut free, SLOTS);
        let mut state = 0xD1B5_4A32_D192_ED03u64;

        for step in 0..STEPS {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let fd = state as usize % SLOTS;
            if free[fd] {
                index.occupy(fd);
            } else {
                index.release(fd);
            }
            free[fd] = !free[fd];

            let minimum = (state.rotate_left(29) as usize) % (SLOTS + 1);
            assert_eq!(
                index.first_free(minimum, free.len()),
                expected(&free, minimum)
            );
            if step % 257 == 0 {
                assert_queries(&index, &free, &[0, 63, 64, 4095, 4096, 8192, 8193]);
            }
        }
    }
}
