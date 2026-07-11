use alloc::vec::Vec;

pub(crate) struct IdAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl IdAllocator {
    pub(crate) fn new(initial_id: usize) -> Self {
        Self {
            current: initial_id,
            recycled: Vec::new(),
        }
    }

    pub(crate) fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            let id = self.current;
            self.current += 1;
            id
        }
    }

    pub(crate) fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.contains(&id),
            "id {id} is already deallocated"
        );
        self.recycled.push(id);
    }
}
