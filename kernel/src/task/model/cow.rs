use super::*;

impl TaskControlBlock {
    pub(crate) fn handle_cow_fault(&self, address: usize) -> Result<bool, MemoryError> {
        self.process
            .address_space
            .memory_set
            .lock()
            .handle_cow_fault(address)
    }
}
