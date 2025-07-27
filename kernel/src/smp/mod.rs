/// Symmetric Multi-Processing (SMP) support for LiteOS
///
/// This module provides the core infrastructure for multi-core CPU management,
/// including CPU discovery, initialization, and per-CPU data structures.

pub mod cpu;
pub mod ipi;
pub mod topology;
pub mod boot;

pub use cpu::*;
pub use ipi::*;
pub use topology::*;
pub use boot::*;

use crate::sync::spinlock::SpinLock;
use core::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use alloc::{vec::Vec, sync::Arc, boxed::Box};

/// Maximum number of CPUs supported by the system
pub const MAX_CPU_NUM: usize = 64;

/// Global CPU count
static CPU_COUNT: AtomicUsize = AtomicUsize::new(1);

/// CPU online status bitmap
static CPU_ONLINE_MASK: AtomicUsize = AtomicUsize::new(1); // CPU 0 is always online initially

/// Per-CPU data array
static mut PER_CPU_DATA: [Option<Arc<CpuData>>; MAX_CPU_NUM] = [const { None }; MAX_CPU_NUM];
static PER_CPU_DATA_INIT: AtomicBool = AtomicBool::new(false);

/// Initialize SMP subsystem
pub fn init() {
    debug!("Initializing SMP subsystem");
    // Initialize per-CPU data for BSP (Boot Strap Processor)
    let bsp_data = Arc::new(CpuData::new(0, CpuType::Bootstrap));
    unsafe {
        PER_CPU_DATA[0] = Some(bsp_data);
    }
    PER_CPU_DATA_INIT.store(true, Ordering::Release);

    // Discover CPUs from device tree
    topology::discover_cpus();

    debug!("SMP initialization complete, {} CPUs discovered", cpu_count());
}

/// Get the current CPU ID
#[inline]
pub fn current_cpu_id() -> usize {
    // For RISC-V, we can use the hart ID from CSR
    #[cfg(target_arch = "riscv64")]
    {
        use riscv::register::mhartid;
        // In supervisor mode, we need to use tp register which should be set during boot
        let mut cpu_id: usize;
        unsafe {
            core::arch::asm!("mv {}, tp", out(reg) cpu_id);
        }
        cpu_id
    }

    #[cfg(not(target_arch = "riscv64"))]
    {
        // Fallback for other architectures
        0
    }
}

/// Get the number of online CPUs
pub fn cpu_count() -> usize {
    CPU_COUNT.load(Ordering::Acquire)
}

/// Check if a CPU is online
pub fn cpu_is_online(cpu_id: usize) -> bool {
    if cpu_id >= MAX_CPU_NUM {
        return false;
    }
    (CPU_ONLINE_MASK.load(Ordering::Acquire) & (1 << cpu_id)) != 0
}

/// Mark a CPU as online
pub fn cpu_set_online(cpu_id: usize) {
    if cpu_id < MAX_CPU_NUM {
        CPU_ONLINE_MASK.fetch_or(1 << cpu_id, Ordering::AcqRel);

        // Update CPU count
        let mut count = 0;
        let mask = CPU_ONLINE_MASK.load(Ordering::Acquire);
        for i in 0..MAX_CPU_NUM {
            if (mask & (1 << i)) != 0 {
                count += 1;
            }
        }
        CPU_COUNT.store(count, Ordering::Release);
    }
}

/// Mark a CPU as offline
pub fn cpu_set_offline(cpu_id: usize) {
    if cpu_id < MAX_CPU_NUM && cpu_id != 0 { // Never offline CPU 0
        CPU_ONLINE_MASK.fetch_and(!(1 << cpu_id), Ordering::AcqRel);

        // Update CPU count
        let mut count = 0;
        let mask = CPU_ONLINE_MASK.load(Ordering::Acquire);
        for i in 0..MAX_CPU_NUM {
            if (mask & (1 << i)) != 0 {
                count += 1;
            }
        }
        CPU_COUNT.store(count, Ordering::Release);
    }
}

/// Get per-CPU data for the current CPU
pub fn current_cpu_data() -> Option<Arc<CpuData>> {
    if !PER_CPU_DATA_INIT.load(Ordering::Acquire) {
        return None;
    }

    let cpu_id = current_cpu_id();
    if cpu_id >= MAX_CPU_NUM {
        return None;
    }

    unsafe {
        PER_CPU_DATA[cpu_id].clone()
    }
}

/// Get per-CPU data for a specific CPU
pub fn cpu_data(cpu_id: usize) -> Option<Arc<CpuData>> {
    if !PER_CPU_DATA_INIT.load(Ordering::Acquire) || cpu_id >= MAX_CPU_NUM {
        return None;
    }

    unsafe {
        PER_CPU_DATA[cpu_id].clone()
    }
}

/// Set per-CPU data for a specific CPU
pub fn set_cpu_data(cpu_id: usize, data: Arc<CpuData>) {
    if cpu_id < MAX_CPU_NUM {
        unsafe {
            PER_CPU_DATA[cpu_id] = Some(data);
        }
    }
}

/// Get all online CPU IDs
pub fn online_cpu_ids() -> Vec<usize> {
    let mut cpus = Vec::new();
    let mask = CPU_ONLINE_MASK.load(Ordering::Acquire);

    for i in 0..MAX_CPU_NUM {
        if (mask & (1 << i)) != 0 {
            cpus.push(i);
        }
    }

    cpus
}

/// Execute a function on all online CPUs
pub fn for_each_online_cpu<F>(mut func: F)
where
    F: FnMut(usize) + Send + Sync,
{
    let current_cpu = current_cpu_id();
    let online_cpus = online_cpu_ids();

    // Execute on other CPUs via IPI
    for &cpu_id in &online_cpus {
        if cpu_id != current_cpu {
            // TODO: Send IPI to execute function on remote CPU
            // For now, we'll just call it locally (not SMP-correct but functional)
            func(cpu_id);
        }
    }

    // Execute on current CPU
    func(current_cpu);
}

/// Execute a function on a specific CPU
pub fn execute_on_cpu<F>(cpu_id: usize, func: F) -> Result<(), &'static str>
where
    F: FnOnce() + Send + 'static,
{
    if !cpu_is_online(cpu_id) {
        return Err("CPU is not online");
    }

    if cpu_id == current_cpu_id() {
        // Execute locally
        func();
        Ok(())
    } else {
        // Send IPI to execute on remote CPU
        ipi::send_function_call_ipi(cpu_id, || {
            func();
            ipi::IpiResponse::Success
        })
    }
}

/// Initialize the TP register for the current CPU (RISC-V specific)
#[cfg(target_arch = "riscv64")]
pub fn init_cpu_id_register(cpu_id: usize) {
    unsafe {
        core::arch::asm!("mv tp, {}", in(reg) cpu_id);
    }
}