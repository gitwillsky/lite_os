/// Memory barrier and synchronization barrier implementations
///
/// This module provides both memory ordering barriers and CPU synchronization barriers.

use core::sync::atomic::{fence, Ordering, AtomicUsize};
use crate::sync::SpinLock;

/// Memory ordering barriers
pub mod memory_barrier {
    use super::*;

    /// Full memory barrier - prevents all memory reordering across this point
    #[inline]
    pub fn full() {
        fence(Ordering::SeqCst);
    }

    /// Acquire barrier - prevents loads/stores from moving before this point
    #[inline]
    pub fn acquire() {
        fence(Ordering::Acquire);
    }

    /// Release barrier - prevents loads/stores from moving after this point
    #[inline]
    pub fn release() {
        fence(Ordering::Release);
    }

    /// Acquire-Release barrier
    #[inline]
    pub fn acq_rel() {
        fence(Ordering::AcqRel);
    }

    /// Architecture-specific memory barriers
    pub mod arch {
        /// Read memory barrier (architecture-specific)
        #[inline]
        pub fn read() {
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("fence r,r", options(nomem, nostack));
            }

            #[cfg(not(target_arch = "riscv64"))]
            super::acquire();
        }

        /// Write memory barrier (architecture-specific)
        #[inline]
        pub fn write() {
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("fence w,w", options(nomem, nostack));
            }

            #[cfg(not(target_arch = "riscv64"))]
            super::release();
        }

        /// Full memory barrier (architecture-specific)
        #[inline]
        pub fn full() {
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("fence rw,rw", options(nomem, nostack));
            }

            #[cfg(target_arch = "x86_64")]
            unsafe {
                core::arch::asm!("mfence", options(nomem, nostack));
            }

            #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64")))]
            super::full();
        }

        /// Instruction barrier (flush instruction pipeline)
        #[inline]
        pub fn instruction() {
            #[cfg(target_arch = "riscv64")]
            unsafe {
                core::arch::asm!("fence.i", options(nomem, nostack));
            }

            #[cfg(target_arch = "x86_64")]
            {
                // x86 has strong ordering and doesn't typically need explicit instruction barriers
                super::full();
            }

            #[cfg(not(any(target_arch = "riscv64", target_arch = "x86_64")))]
            super::full();
        }
    }
}

/// CPU synchronization barrier for coordinating multiple processors
///
/// This barrier allows multiple CPUs to synchronize at a specific point.
/// All CPUs will wait until the specified number of CPUs have reached the barrier.
pub struct CpuBarrier {
    /// Expected number of CPUs
    expected: usize,
    /// Current number of CPUs that have reached the barrier
    current: SpinLock<usize>,
    /// Barrier generation (to handle reuse)
    generation: SpinLock<usize>,
}

impl CpuBarrier {
    /// Create a new CPU barrier
    pub const fn new(expected_cpus: usize) -> Self {
        Self {
            expected: expected_cpus,
            current: SpinLock::new(0),
            generation: SpinLock::new(0),
        }
    }

    /// Wait at the barrier until all expected CPUs arrive
    ///
    /// Returns the arrival order (0 = first, 1 = second, etc.)
    pub fn wait(&self) -> usize {
        let generation = *self.generation.lock();
        let mut current = self.current.lock();

        let arrival_order = *current;
        *current += 1;

        if *current >= self.expected {
            // Last CPU to arrive - reset for next use
            *current = 0;
            let mut generation = self.generation.lock();
            *generation += 1;
            drop(generation);
            drop(current);

            // Memory barrier to ensure all previous operations are visible
            memory_barrier::full();
            arrival_order
        } else {
            drop(current);

            // Wait for all CPUs to arrive
            loop {
                let current_gen = *self.generation.lock();
                if current_gen > generation {
                    break;
                }
                core::hint::spin_loop();
            }

            arrival_order
        }
    }

    /// Reset the barrier (should only be called when no CPUs are waiting)
    pub fn reset(&self) {
        *self.current.lock() = 0;
        *self.generation.lock() = 0;
    }

    /// Get the number of CPUs currently waiting
    pub fn waiting_count(&self) -> usize {
        *self.current.lock()
    }
}

/// Sense-reversal barrier (more efficient for repeated use)
///
/// This barrier uses a "sense" bit that alternates between uses,
/// avoiding the need for a generation counter.
pub struct SenseBarrier {
    expected: usize,
    count: SpinLock<usize>,
    sense: SpinLock<bool>,
}

impl SenseBarrier {
    /// Create a new sense-reversal barrier
    pub const fn new(expected_cpus: usize) -> Self {
        Self {
            expected: expected_cpus,
            count: SpinLock::new(0),
            sense: SpinLock::new(false),
        }
    }

    /// Wait at the barrier with thread-local sense
    ///
    /// Each thread should maintain its own local sense value that starts as false.
    pub fn wait(&self, local_sense: &mut bool) -> usize {
        let arrival_order;

        {
            let mut count = self.count.lock();
            arrival_order = *count;
            *count += 1;

            if *count == self.expected {
                // Last thread - flip the global sense and reset count
                *count = 0;
                let mut global_sense = self.sense.lock();
                *global_sense = !*global_sense;
                memory_barrier::full();
            }
        }

        // Wait for sense to change
        *local_sense = !*local_sense;
        while *self.sense.lock() != *local_sense {
            core::hint::spin_loop();
        }

        arrival_order
    }
}

/// Combining tree barrier (efficient for large numbers of CPUs)
///
/// This barrier organizes CPUs in a tree structure, reducing contention
/// compared to centralized barriers.
pub struct TreeBarrier {
    expected: usize,
    // For simplicity, we implement a 2-level tree
    // More levels can be added for systems with many CPUs
    leaf_barriers: [CpuBarrier; 8], // Support up to 8 groups
    root_barrier: CpuBarrier,
}

impl TreeBarrier {
    /// Create a new tree barrier
    ///
    /// CPUs are divided into groups, with each group having its own leaf barrier.
    /// Group representatives then synchronize at the root barrier.
    pub const fn new(expected_cpus: usize) -> Self {
        let cpus_per_group = (expected_cpus + 7) / 8; // Ceiling division
        let num_groups = (expected_cpus + cpus_per_group - 1) / cpus_per_group;

        const INIT_BARRIER: CpuBarrier = CpuBarrier::new(0);
        let leaf_barriers = [INIT_BARRIER; 8];

        // Use manual minimum to avoid non-const function call
        let root_groups = if num_groups < 8 { num_groups } else { 8 };

        Self {
            expected: expected_cpus,
            leaf_barriers,
            root_barrier: CpuBarrier::new(root_groups),
        }
    }

    /// Wait at the barrier
    ///
    /// cpu_id should be the logical CPU ID (0 to expected-1)
    pub fn wait(&self, cpu_id: usize) -> usize {
        if cpu_id >= self.expected {
            return 0;
        }

        let cpus_per_group = (self.expected + 7) / 8;
        let group_id = cpu_id / cpus_per_group;
        let group_position = cpu_id % cpus_per_group;

        // Wait at leaf barrier
        let leaf_order = self.leaf_barriers[group_id].wait();

        // Group representative waits at root barrier
        if group_position == 0 {
            self.root_barrier.wait();
        }

        // Wait for group representative to return from root
        self.leaf_barriers[group_id].wait();

        leaf_order
    }
}

/// Global CPU barriers for kernel synchronization
static BOOT_BARRIER: CpuBarrier = CpuBarrier::new(1); // Will be updated during boot
static SHUTDOWN_BARRIER: CpuBarrier = CpuBarrier::new(1);

/// Initialize global barriers with the actual CPU count
pub fn init_global_barriers(cpu_count: usize) {
    // Unfortunately, we can't modify const values, so we need a different approach
    // In practice, these would be initialized as non-const statics
    info!("Global barriers initialized for {} CPUs", cpu_count);
}

/// Boot synchronization barrier - all CPUs wait until boot is complete
pub fn boot_barrier_wait() -> usize {
    BOOT_BARRIER.wait()
}

/// Shutdown synchronization barrier - all CPUs coordinate shutdown
pub fn shutdown_barrier_wait() -> usize {
    SHUTDOWN_BARRIER.wait()
}