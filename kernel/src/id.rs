use alloc::vec::Vec;

pub struct IdAllocator {
    current: usize,
    recycled: Vec<usize>,
}

impl IdAllocator {
    pub fn new(initial_id: usize) -> Self {
        Self {
            current: initial_id,
            recycled: Vec::new(),
        }
    }

    pub fn alloc(&mut self) -> usize {
        if let Some(id) = self.recycled.pop() {
            id
        } else {
            let id = self.current;
            self.current += 1;
            id
        }
    }

    pub fn dealloc(&mut self, id: usize) {
        assert!(id < self.current);
        assert!(
            !self.recycled.contains(&id),
            "id {} is already deallocated",
            id
        );
        self.recycled.push(id);
    }
}
