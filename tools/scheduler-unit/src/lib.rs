#[cfg(test)]
extern crate alloc;

#[cfg(test)]
#[path = "../../../kernel/src/task/scheduler/preallocated_heap.rs"]
mod preallocated_heap;

#[cfg(test)]
mod tests {
    use super::preallocated_heap::PreallocatedHeap;

    #[test]
    fn spare_capacity_never_invokes_the_compaction_predicate() {
        let mut heap = PreallocatedHeap::try_with_capacity(64).unwrap();
        let reserved = heap.capacity();
        let mut checks = 0;

        for value in 0..64 {
            assert_eq!(
                heap.make_room(1, |_| {
                    checks += 1;
                    true
                }),
                0
            );
            heap.push(value);
        }

        assert_eq!(checks, 0);
        assert_eq!(heap.len(), 64);
        assert_eq!(heap.capacity(), reserved);
    }

    #[test]
    fn capacity_pressure_compacts_once_and_preserves_heap_order() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in 0..8 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let reserved = heap.capacity();
        let mut checks = 0;

        let removed = heap.make_room(1, |value| {
            checks += 1;
            value % 2 == 0
        });
        heap.push(9);

        assert_eq!(removed, 4);
        assert_eq!(checks, 8);
        assert_eq!(heap.capacity(), reserved);
        let mut popped = alloc::vec::Vec::new();
        while let Some(value) = heap.pop() {
            popped.push(value);
        }
        assert_eq!(popped, [9, 6, 4, 2, 0]);
    }

    #[test]
    fn batch_pressure_compacts_once_and_preserves_reserved_backing() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in 0..8 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let reserved = heap.capacity();
        let mut checks = 0;

        assert_eq!(
            heap.make_room(4, |value| {
                checks += 1;
                value % 2 == 0
            }),
            4
        );
        for value in 8..12 {
            heap.push(value);
        }

        assert_eq!(checks, 8);
        assert_eq!(heap.len(), 8);
        assert_eq!(heap.capacity(), reserved);
        let mut popped = alloc::vec::Vec::new();
        while let Some(value) = heap.pop() {
            popped.push(value);
        }
        assert_eq!(popped, [11, 10, 9, 8, 6, 4, 2, 0]);
    }

    #[test]
    #[should_panic(expected = "preallocated heap capacity exhausted by live entries")]
    fn live_capacity_divergence_is_fail_stop() {
        let mut heap = PreallocatedHeap::try_with_capacity(4).unwrap();
        for value in 0..4 {
            heap.make_room(1, |_| true);
            heap.push(value);
        }

        heap.make_room(1, |_| true);
    }

    #[test]
    fn root_pruning_stops_at_the_first_live_heap_root() {
        let mut heap = PreallocatedHeap::try_with_capacity(8).unwrap();
        for value in [10, 20, 40, 50, 99, 100] {
            heap.make_room(1, |_| true);
            heap.push(value);
        }
        let mut checks = 0;

        let removed = heap.discard_invalid_roots(|value| {
            checks += 1;
            *value <= 50
        });

        assert_eq!(removed, 2);
        assert_eq!(checks, 3);
        assert_eq!(heap.peek(), Some(&50));
    }
}
