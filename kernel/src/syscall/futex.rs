use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use alloc::sync::Arc;
use spin::Mutex;
use lazy_static::lazy_static;
use crate::task::{current_task, block_current_and_run_next};
use crate::memory::page_table::translated_ref_mut;

// Futex operations
pub const FUTEX_WAIT: i32 = 0;
pub const FUTEX_WAKE: i32 = 1;
pub const FUTEX_FD: i32 = 2;
pub const FUTEX_REQUEUE: i32 = 3;
pub const FUTEX_CMP_REQUEUE: i32 = 4;
pub const FUTEX_WAKE_OP: i32 = 5;
pub const FUTEX_LOCK_PI: i32 = 6;
pub const FUTEX_UNLOCK_PI: i32 = 7;
pub const FUTEX_TRYLOCK_PI: i32 = 8;
pub const FUTEX_WAIT_BITSET: i32 = 9;
pub const FUTEX_WAKE_BITSET: i32 = 10;

// Futex flags
pub const FUTEX_PRIVATE_FLAG: i32 = 128;
pub const FUTEX_CLOCK_REALTIME: i32 = 256;

struct FutexWaiter {
    task: Arc<crate::task::TaskControlBlock>,
    bitset: u32,
}

lazy_static! {
    // Map from futex address to waiting threads
    static ref FUTEX_WAITERS: Mutex<BTreeMap<usize, Vec<FutexWaiter>>> = Mutex::new(BTreeMap::new());
}

/// Futex system call implementation
pub fn sys_futex(uaddr: *mut i32, op: i32, val: i32) -> isize {
    let cmd = op & !FUTEX_PRIVATE_FLAG;
    
    match cmd {
        FUTEX_WAIT => futex_wait(uaddr, val),
        FUTEX_WAKE => futex_wake(uaddr, val),
        FUTEX_REQUEUE => futex_requeue(uaddr, val, 0, core::ptr::null_mut(), 0),
        FUTEX_CMP_REQUEUE => futex_requeue(uaddr, val, 0, core::ptr::null_mut(), 0),
        FUTEX_WAKE_OP => futex_wake_op(uaddr, val, 0, core::ptr::null_mut(), 0),
        _ => -22, // EINVAL
    }
}

/// Extended futex system call with more parameters
pub fn sys_futex_ext(
    uaddr: *mut i32, 
    op: i32, 
    val: i32, 
    timeout: *const u8,
    uaddr2: *mut i32,
    val3: i32
) -> isize {
    let cmd = op & !FUTEX_PRIVATE_FLAG;
    
    match cmd {
        FUTEX_WAIT => futex_wait(uaddr, val),
        FUTEX_WAKE => futex_wake(uaddr, val),
        FUTEX_REQUEUE => futex_requeue(uaddr, val, val3, uaddr2, 0),
        FUTEX_CMP_REQUEUE => futex_requeue(uaddr, val, val3, uaddr2, val3),
        FUTEX_WAKE_OP => futex_wake_op(uaddr, val, val3, uaddr2, val3),
        FUTEX_WAIT_BITSET => futex_wait_bitset(uaddr, val, val3 as u32),
        FUTEX_WAKE_BITSET => futex_wake_bitset(uaddr, val, val3 as u32),
        _ => -22, // EINVAL
    }
}

fn futex_wait(uaddr: *mut i32, expected: i32) -> isize {
    let task = current_task().unwrap();
    let token = task.mm.user_token();
    
    // Read the current value at uaddr
    let current_val = if let Some(val_ref) = Some(unsafe { translated_ref_mut(token, uaddr) }) {
        *val_ref
    } else {
        return -14; // EFAULT
    };
    
    // If value has changed, return immediately
    if current_val != expected {
        return -11; // EAGAIN
    }
    
    // Add current thread to waiters
    let addr = uaddr as usize;
    
    {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.entry(addr).or_insert_with(Vec::new).push(FutexWaiter {
            task: task.clone(),
            bitset: u32::MAX,
        });
    }
    
    // Block current thread
    block_current_and_run_next();
    
    0
}

fn futex_wait_bitset(uaddr: *mut i32, expected: i32, bitset: u32) -> isize {
    if bitset == 0 {
        return -22; // EINVAL
    }
    
    let task = current_task().unwrap();
    let token = task.mm.user_token();
    
    // Read the current value at uaddr
    let current_val = if let Some(val_ref) = Some(unsafe { translated_ref_mut(token, uaddr) }) {
        *val_ref
    } else {
        return -14; // EFAULT
    };
    
    // If value has changed, return immediately
    if current_val != expected {
        return -11; // EAGAIN
    }
    
    // Add current thread to waiters with bitset
    let addr = uaddr as usize;
    
    {
        let mut waiters = FUTEX_WAITERS.lock();
        waiters.entry(addr).or_insert_with(Vec::new).push(FutexWaiter {
            task: task.clone(),
            bitset,
        });
    }
    
    // Block current thread
    block_current_and_run_next();
    
    0
}

fn futex_wake(uaddr: *mut i32, max_wake: i32) -> isize {
    futex_wake_bitset(uaddr, max_wake, u32::MAX)
}

fn futex_wake_bitset(uaddr: *mut i32, max_wake: i32, bitset: u32) -> isize {
    if bitset == 0 {
        return -22; // EINVAL
    }
    
    let addr = uaddr as usize;
    let mut woken = 0;
    
    let mut waiters = FUTEX_WAITERS.lock();
    if let Some(wait_list) = waiters.get_mut(&addr) {
        let mut i = 0;
        while i < wait_list.len() && woken < max_wake {
            // Check if bitsets match
            if wait_list[i].bitset & bitset != 0 {
                let waiter = wait_list.remove(i);
                waiter.task.wakeup();
                woken += 1;
            } else {
                i += 1;
            }
        }
        
        // Remove empty wait list
        if wait_list.is_empty() {
            waiters.remove(&addr);
        }
    }
    
    woken as isize
}

fn futex_requeue(
    uaddr: *mut i32, 
    wake_count: i32, 
    requeue_count: i32,
    uaddr2: *mut i32,
    val3: i32
) -> isize {
    let addr1 = uaddr as usize;
    let addr2 = uaddr2 as usize;
    
    if addr2 == 0 {
        return -22; // EINVAL
    }
    
    let mut waiters = FUTEX_WAITERS.lock();
    let mut woken = 0;
    let mut requeued = 0;
    
    if let Some(wait_list1) = waiters.remove(&addr1) {
        let mut remaining = Vec::new();
        
        for waiter in wait_list1 {
            if woken < wake_count {
                waiter.task.wakeup();
                woken += 1;
            } else if requeued < requeue_count {
                // Move to second futex
                waiters.entry(addr2).or_insert_with(Vec::new).push(waiter);
                requeued += 1;
            } else {
                remaining.push(waiter);
            }
        }
        
        // Put back any remaining waiters
        if !remaining.is_empty() {
            waiters.insert(addr1, remaining);
        }
    }
    
    (woken + requeued) as isize
}

fn futex_wake_op(
    uaddr: *mut i32,
    wake1_count: i32,
    wake2_count: i32,
    uaddr2: *mut i32,
    op: i32
) -> isize {
    // Decode the operation
    let op_type = (op >> 28) & 0xf;
    let cmp_type = (op >> 24) & 0xf;
    let arg = (op >> 12) & 0xfff;
    let cmparg = op & 0xfff;
    
    let task = current_task().unwrap();
    let token = task.mm.user_token();
    
    // Perform the operation on uaddr2
    let oldval = {
        let val_ref = unsafe { translated_ref_mut(token, uaddr2) };
        let old = *val_ref;
        
        // Perform operation
        let newval = match op_type {
            0 => arg as i32,           // FUTEX_OP_SET
            1 => old + arg as i32,     // FUTEX_OP_ADD
            2 => old | arg as i32,     // FUTEX_OP_OR
            3 => old & !(arg as i32),  // FUTEX_OP_ANDN
            4 => old ^ arg as i32,     // FUTEX_OP_XOR
            _ => return -22,           // EINVAL
        };
        
        *val_ref = newval;
        old
    };
    
    // Check comparison
    let should_wake2 = match cmp_type {
        0 => oldval == cmparg as i32,      // FUTEX_OP_CMP_EQ
        1 => oldval != cmparg as i32,      // FUTEX_OP_CMP_NE
        2 => oldval < cmparg as i32,       // FUTEX_OP_CMP_LT
        3 => oldval <= cmparg as i32,      // FUTEX_OP_CMP_LE
        4 => oldval > cmparg as i32,       // FUTEX_OP_CMP_GT
        5 => oldval >= cmparg as i32,      // FUTEX_OP_CMP_GE
        _ => return -22,                    // EINVAL
    };
    
    // Wake threads
    let mut total_woken = futex_wake(uaddr, wake1_count);
    if should_wake2 {
        total_woken += futex_wake(uaddr2, wake2_count);
    }
    
    total_woken
}

/// Set robust list for thread
pub fn sys_set_robust_list(_head: *mut u8, _len: usize) -> isize {
    // Basic implementation - just accept the robust list
    // In a full implementation, this would be stored per-thread
    0
}

/// Get robust list for thread
pub fn sys_get_robust_list(_pid: i32, _head_ptr: *mut *mut u8) -> isize {
    // Basic implementation - return success
    // In a full implementation, this would retrieve the stored list
    0
}