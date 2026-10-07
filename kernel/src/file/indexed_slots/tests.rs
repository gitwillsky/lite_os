use alloc::{rc::Rc, vec::Vec};
use core::{cell::Cell, num::NonZeroUsize};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AllocationMetrics {
    roots: usize,
    branches: usize,
    chunks: usize,
    heap_bytes: usize,
}

impl<T> IndexedSlots<T> {
    fn allocation_metrics(&self) -> AllocationMetrics {
        let roots = usize::from(self.root.is_some());
        let branches = self.root.as_deref().map_or(0, |root| {
            root.branches
                .iter()
                .filter(|branch| branch.is_some())
                .count()
        });
        let chunks = self.root.as_deref().map_or(0, |root| {
            root.branches
                .iter()
                .filter_map(Option::as_deref)
                .map(|branch| branch.chunks.iter().filter(|chunk| chunk.is_some()).count())
                .sum()
        });
        AllocationMetrics {
            roots,
            branches,
            chunks,
            heap_bytes: roots * core::mem::size_of::<RadixRoot<T>>()
                + branches * core::mem::size_of::<RadixBranch<T>>()
                + chunks * core::mem::size_of::<SlotChunk<T>>(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelDescriptor {
    ofd: NonZeroUsize,
    cloexec: bool,
    published: bool,
}

fn descriptor(value: usize) -> ModelDescriptor {
    ModelDescriptor {
        ofd: NonZeroUsize::new(value + 1).unwrap(),
        cloexec: false,
        published: true,
    }
}

fn allocation_budget(remaining: usize) -> impl FnMut() -> Result<(), ()> {
    let mut remaining = remaining;
    move || {
        if remaining == 0 {
            return Err(());
        }
        remaining -= 1;
        Ok(())
    }
}

#[test]
fn empty_and_two_extreme_fds_match_the_reviewed_memory_budget() {
    assert_eq!(RADIX_LEVELS, 3);
    assert_eq!(core::mem::size_of::<Option<ModelDescriptor>>(), 16);
    assert_eq!(core::mem::size_of::<IndexedSlots<ModelDescriptor>>(), 24);
    assert_eq!(core::mem::size_of::<RadixRoot<ModelDescriptor>>(), 1_040);
    assert_eq!(core::mem::size_of::<RadixBranch<ModelDescriptor>>(), 1_040);
    assert_eq!(core::mem::size_of::<SlotChunk<ModelDescriptor>>(), 1_024);

    let mut slots = IndexedSlots::new();
    assert_eq!(slots.allocation_metrics().heap_bytes, 0);
    assert_eq!(
        slots.replace_with(0, MAX_FILE_DESCRIPTORS, || descriptor(0)),
        Ok(None)
    );
    assert_eq!(
        slots.replace_with(MAX_FILE_DESCRIPTORS - 1, MAX_FILE_DESCRIPTORS, || {
            descriptor(MAX_FILE_DESCRIPTORS - 1)
        }),
        Ok(None)
    );
    assert_eq!(
        slots.allocation_metrics(),
        AllocationMetrics {
            roots: 1,
            branches: 2,
            chunks: 2,
            heap_bytes: 5_168,
        }
    );
    assert_eq!(16 * MAX_FILE_DESCRIPTORS / 5_168, 3_246);
}

#[test]
fn dense_lowest_free_and_sparse_iteration_match_a_linear_model() {
    const DENSE: usize = 16_384;
    let mut slots = IndexedSlots::new();
    let mut model = vec![None; DENSE];
    for (fd, entry) in model.iter_mut().enumerate().take(DENSE) {
        assert_eq!(slots.insert_with(0, DENSE, || fd), Ok(fd));
        *entry = Some(fd);
    }
    for fd in [0, 63, 64, 8_191, 8_192, DENSE - 1] {
        assert_eq!(slots.take(fd), model[fd].take());
    }
    for minimum in [0, 1, 63, 64, 65, 8_191, 8_192, DENSE - 1] {
        let expected = model
            .iter()
            .enumerate()
            .skip(minimum)
            .find_map(|(fd, value)| value.is_none().then_some(fd));
        match expected {
            Some(expected) => {
                assert_eq!(slots.insert_with(minimum, DENSE, || expected), Ok(expected));
                model[expected] = Some(expected);
            }
            None => assert_eq!(
                slots.insert_with(minimum, DENSE, || usize::MAX),
                Err(SlotInsertError::Limit)
            ),
        }
    }

    assert_eq!(
        slots.replace_with(MAX_FILE_DESCRIPTORS - 1, MAX_FILE_DESCRIPTORS, || {
            usize::MAX
        }),
        Ok(None)
    );
    let iterated: Vec<_> = slots.iter().map(|(fd, value)| (fd, *value)).collect();
    assert_eq!(
        iterated.last(),
        Some(&(MAX_FILE_DESCRIPTORS - 1, usize::MAX))
    );
    assert_eq!(
        slots.find_from(DENSE, |value| *value == usize::MAX),
        Some(MAX_FILE_DESCRIPTORS - 1)
    );
    assert_eq!(slots.find_from(DENSE, |_| false), None);
}

#[test]
fn randomized_transitions_keep_full_summaries_and_lowest_free_exact() {
    const LIMIT: usize = 32_768;
    let mut slots = IndexedSlots::new();
    let mut model = vec![None; LIMIT];
    let mut state = 0xD1B5_4A32_D192_ED03u64;
    for step in 0..20_000usize {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let fd = state as usize % LIMIT;
        if model[fd].is_some() {
            assert_eq!(slots.take(fd), model[fd].take());
        } else {
            assert_eq!(slots.replace_with(fd, LIMIT, || step), Ok(None));
            model[fd] = Some(step);
        }
        let minimum = state.rotate_left(29) as usize % LIMIT;
        let expected = model
            .iter()
            .enumerate()
            .skip(minimum)
            .find_map(|(fd, value)| value.is_none().then_some(fd));
        let actual = slots
            .free
            .first_free(slots.root.as_deref(), minimum, slots.logical_len)
            .or_else(|| {
                (slots.logical_len.max(minimum) < LIMIT).then_some(slots.logical_len.max(minimum))
            });
        assert_eq!(actual, expected);
    }
}

#[test]
fn single_and_pair_oom_publish_neither_nodes_nor_value_side_effects() {
    for successful_allocations in 0..3 {
        let mut slots = IndexedSlots::new();
        let constructed = Cell::new(false);
        let result = slots.replace_with_allocation(
            MAX_FILE_DESCRIPTORS - 1,
            MAX_FILE_DESCRIPTORS,
            || {
                constructed.set(true);
                1
            },
            &mut allocation_budget(successful_allocations),
        );
        assert_eq!(result, Err(SlotInsertError::OutOfMemory));
        assert!(!constructed.get());
        assert_eq!(slots.len(), 0);
        assert_eq!(slots.allocation_metrics().heap_bytes, 0);
    }

    let mut slots = IndexedSlots::new();
    for fd in 0..ROOT_SPAN - 1 {
        assert_eq!(slots.insert_with(0, ROOT_SPAN, || fd), Ok(fd));
    }
    let before = slots.allocation_metrics();
    for successful_allocations in 0..2 {
        let constructed = Cell::new(false);
        assert_eq!(
            slots.insert_pair_with_allocation(
                MAX_FILE_DESCRIPTORS,
                || {
                    constructed.set(true);
                    (10, 11)
                },
                &mut allocation_budget(successful_allocations),
            ),
            Err(SlotInsertError::OutOfMemory)
        );
        assert!(!constructed.get());
        assert_eq!(slots.allocation_metrics(), before);
        assert_eq!(slots.iter().count(), ROOT_SPAN - 1);
        assert_eq!(slots.get(ROOT_SPAN - 1), None);
    }
    assert_eq!(
        slots.insert_pair_with_allocation(
            MAX_FILE_DESCRIPTORS,
            || (10, 11),
            &mut allocation_budget(2),
        ),
        Ok((ROOT_SPAN - 1, ROOT_SPAN))
    );
}

struct CountedClone {
    live: Rc<Cell<usize>>,
    value: usize,
}

impl CountedClone {
    fn new(live: Rc<Cell<usize>>, value: usize) -> Self {
        live.set(live.get() + 1);
        Self { live, value }
    }
}

impl Clone for CountedClone {
    fn clone(&self) -> Self {
        Self::new(self.live.clone(), self.value)
    }
}

impl Drop for CountedClone {
    fn drop(&mut self) {
        self.live.set(self.live.get() - 1);
    }
}

#[test]
fn sparse_clone_copies_only_materialized_chunks_and_oom_rolls_back_values() {
    let live = Rc::new(Cell::new(0));
    let mut source = IndexedSlots::new();
    for fd in [0, 1, ROOT_SPAN, MAX_FILE_DESCRIPTORS - 1] {
        source
            .replace_with(fd, MAX_FILE_DESCRIPTORS, || {
                CountedClone::new(live.clone(), fd)
            })
            .unwrap();
    }
    assert_eq!(live.get(), 4);
    let source_metrics = source.allocation_metrics();
    assert_eq!((source_metrics.branches, source_metrics.chunks), (3, 3));

    for successful_allocations in
        0..source_metrics.roots + source_metrics.branches + source_metrics.chunks
    {
        let result = source.try_clone_where_with_allocation(
            |_| true,
            &mut allocation_budget(successful_allocations),
        );
        assert!(result.is_err());
        assert_eq!(live.get(), 4);
        assert_eq!(source.iter().count(), 4);
    }
    let cloned = source.try_clone_where(|_| true).unwrap();
    assert_eq!(cloned.allocation_metrics(), source_metrics);
    assert_eq!(live.get(), 8);
    drop(cloned);
    assert_eq!(live.get(), 4);
}

#[test]
fn pair_replace_take_if_and_take_all_share_one_occupancy_owner() {
    let mut slots = IndexedSlots::new();
    assert_eq!(slots.insert_pair_with(8, || (10, 11)), Ok((0, 1)));
    assert_eq!(slots.take(0), Some(10));
    assert_eq!(slots.insert_pair_with(8, || (20, 21)), Ok((0, 2)));
    assert_eq!(slots.replace_with(1, 8, || 40), Ok(Some(11)));
    assert_eq!(slots.take_if(1, |value| *value == 99), None);
    assert_eq!(slots.take_if(1, |value| *value == 40), Some(40));

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
