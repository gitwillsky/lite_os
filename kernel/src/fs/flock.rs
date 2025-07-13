use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::task::TaskControlBlock;
use spin::Mutex;

/// File lock types corresponding to POSIX flock constants
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockType {
    Shared = 1,     // LOCK_SH
    Exclusive = 2,  // LOCK_EX
}

/// File lock operation flags
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockOp {
    Unlock = 8,     // LOCK_UN
    NonBlock = 4,   // LOCK_NB
}

/// File lock structure
#[derive(Debug, Clone)]
pub struct FileLock {
    pub lock_type: LockType,
    pub owner_pid: usize,
    pub owner_task: Arc<TaskControlBlock>,
}

impl FileLock {
    pub fn new(lock_type: LockType, owner_pid: usize, owner_task: Arc<TaskControlBlock>) -> Self {
        Self {
            lock_type,
            owner_pid,
            owner_task,
        }
    }
}

/// Global file lock manager
pub struct FileLockManager {
    // Map from inode ID to list of locks on that inode
    // In a real filesystem, we'd use inode numbers, but for simplicity we'll use memory addresses
    locks: Mutex<BTreeMap<usize, Vec<FileLock>>>,
}

impl FileLockManager {
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Get a unique identifier for an inode (using memory address as a simple approach)
    fn get_inode_id(inode: &Arc<dyn super::Inode>) -> usize {
        // Use the Arc's internal pointer to get a unique identifier
        Arc::as_ptr(inode) as *const () as usize
    }

    /// Try to acquire a lock on the given inode
    pub fn try_lock(
        &self,
        inode: &Arc<dyn super::Inode>,
        lock_type: LockType,
        owner_pid: usize,
        owner_task: Arc<TaskControlBlock>,
        non_blocking: bool,
    ) -> Result<(), LockError> {
        let inode_id = Self::get_inode_id(inode);
        let mut locks = self.locks.lock();
        let inode_locks = locks.entry(inode_id).or_insert_with(Vec::new);

        // Check if this process already has a lock on this file
        for (i, existing_lock) in inode_locks.iter().enumerate() {
            if existing_lock.owner_pid == owner_pid {
                // Replace existing lock with new one
                inode_locks[i] = FileLock::new(lock_type, owner_pid, owner_task);
                return Ok(());
            }
        }

        // Check for conflicting locks
        if self.has_conflicting_lock(inode_locks, lock_type, owner_pid) {
            if non_blocking {
                return Err(LockError::WouldBlock);
            } else {
                // In a real implementation, we would block the process here
                // For simplicity, we'll return an error for now
                return Err(LockError::WouldBlock);
            }
        }

        // Add the new lock
        inode_locks.push(FileLock::new(lock_type, owner_pid, owner_task));
        Ok(())
    }

    /// Remove a lock from the given inode
    pub fn unlock(
        &self,
        inode: &Arc<dyn super::Inode>,
        owner_pid: usize,
    ) -> Result<(), LockError> {
        let inode_id = Self::get_inode_id(inode);
        let mut locks = self.locks.lock();
        
        if let Some(inode_locks) = locks.get_mut(&inode_id) {
            inode_locks.retain(|lock| lock.owner_pid != owner_pid);
            if inode_locks.is_empty() {
                locks.remove(&inode_id);
            }
            Ok(())
        } else {
            Err(LockError::NotLocked)
        }
    }

    /// Remove all locks owned by a process (called when process exits)
    pub fn remove_process_locks(&self, owner_pid: usize) {
        let mut locks = self.locks.lock();
        for inode_locks in locks.values_mut() {
            inode_locks.retain(|lock| lock.owner_pid != owner_pid);
        }
        locks.retain(|_, inode_locks| !inode_locks.is_empty());
    }

    /// Check if there are conflicting locks
    fn has_conflicting_lock(
        &self,
        inode_locks: &[FileLock],
        requested_type: LockType,
        requester_pid: usize,
    ) -> bool {
        for existing_lock in inode_locks {
            // Skip locks owned by the same process
            if existing_lock.owner_pid == requester_pid {
                continue;
            }

            match (existing_lock.lock_type, requested_type) {
                // Exclusive locks conflict with everything
                (LockType::Exclusive, _) | (_, LockType::Exclusive) => return true,
                // Shared locks don't conflict with each other
                (LockType::Shared, LockType::Shared) => continue,
            }
        }
        false
    }

    /// Get information about locks on an inode (for debugging)
    pub fn get_locks(&self, inode: &Arc<dyn super::Inode>) -> Vec<FileLock> {
        let inode_id = Self::get_inode_id(inode);
        let locks = self.locks.lock();
        locks.get(&inode_id).cloned().unwrap_or_default()
    }
}

/// File lock error types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockError {
    WouldBlock,
    NotLocked,
    InvalidOperation,
    PermissionDenied,
}

/// Global file lock manager instance
static FILE_LOCK_MANAGER: FileLockManager = FileLockManager {
    locks: Mutex::new(BTreeMap::new()),
};

/// Get the global file lock manager
pub fn get_file_lock_manager() -> &'static FileLockManager {
    &FILE_LOCK_MANAGER
}