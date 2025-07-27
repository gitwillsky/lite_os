use core::sync::atomic::{AtomicU64, Ordering};

/// Global time synchronization
///
/// This structure manages time synchronization across all CPUs.
pub struct GlobalTimer {
    /// Global time base in microseconds (monotonic)
    global_time_base: AtomicU64,
    /// Boot time in microseconds
    boot_time: AtomicU64,
    /// Per-CPU time offsets for synchronization
    cpu_offsets: [AtomicU64; crate::smp::MAX_CPU_NUM],
}

/// Global timer instance
pub static GLOBAL_TIMER: GlobalTimer = GlobalTimer::new();

impl GlobalTimer {
    const fn new() -> Self {
        const INIT_OFFSET: AtomicU64 = AtomicU64::new(0);
        Self {
            global_time_base: AtomicU64::new(0),
            boot_time: AtomicU64::new(0),
            cpu_offsets: [INIT_OFFSET; crate::smp::MAX_CPU_NUM],
        }
    }

    /// Initialize global timer
    pub fn init(&self) {
        let boot_time = self.get_hardware_time_us();
        self.boot_time.store(boot_time, Ordering::Relaxed);
        self.global_time_base.store(0, Ordering::Relaxed);

        info!("Global timer initialized at boot time {}μs", boot_time);
    }

    /// Get hardware time in microseconds
    fn get_hardware_time_us(&self) -> u64 {
        #[cfg(target_arch = "riscv64")]
        {
            use riscv::register::time;

            use crate::timer::config::TIMER_FREQ;
            let freq = TIMER_FREQ.load(Ordering::Relaxed);
            if freq == 0 {
                // Safety fallback: if TIMER_FREQ not initialized, return 0
                // This should not happen with proper initialization
                warn!("TIMER_FREQ not initialized, returning 0");
                return 0;
            }
            time::read64() / (freq / 1_000_000)
        }

        #[cfg(not(target_arch = "riscv64"))]
        {
            // Fallback for other architectures
            static mut FAKE_TIME: u64 = 0;
            unsafe {
                FAKE_TIME += 1000; // 1ms increment
                FAKE_TIME
            }
        }
    }

    /// Get current monotonic time in microseconds
    pub fn get_time_us(&self) -> u64 {
        let hardware_time = self.get_hardware_time_us();
        let boot_time = self.boot_time.load(Ordering::Relaxed);
        hardware_time.saturating_sub(boot_time)
    }

    /// Synchronize time on a CPU
    pub fn sync_cpu_time(&self, cpu_id: usize) {
        if cpu_id < crate::smp::MAX_CPU_NUM {
            let global_time = self.get_time_us();
            let local_time = self.get_hardware_time_us();
            let offset = global_time.saturating_sub(local_time);
            self.cpu_offsets[cpu_id].store(offset, Ordering::Relaxed);

            debug!("Synchronized CPU {} time, offset: {}μs", cpu_id, offset);
        }
    }
}
