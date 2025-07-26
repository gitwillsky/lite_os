/// Multi-core synchronization primitives
/// 
/// This module provides SMP-safe synchronization primitives for the LiteOS kernel.
/// All primitives are designed to work correctly in multi-processor environments.

pub mod spinlock;
pub mod barrier;
pub mod rwlock;

pub use spinlock::{SpinLock, SpinLockGuard};
pub use rwlock::{RwSpinLock, ReadGuard, WriteGuard};
pub use barrier::*;

/// Re-export atomic types for convenience
pub use core::sync::atomic::{
    AtomicBool, AtomicUsize, AtomicU64, AtomicIsize, AtomicI64,
    Ordering, fence, compiler_fence
};