/// CPU topology discovery and management
///
/// This module discovers CPU topology from device tree and provides
/// information about CPU hierarchy, NUMA domains, and cache topology.

use alloc::{vec, vec::Vec, sync::Arc};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::{
    smp::{cpu::{CpuInfo, CpuFeatures}, MAX_CPU_NUM, set_cpu_data, CpuData, CpuType},
    board::board_info,
};

/// NUMA topology information
#[derive(Debug, Clone)]
pub struct NumaNode {
    /// NUMA node ID
    pub node_id: usize,
    /// CPUs belonging to this NUMA node
    pub cpu_list: Vec<usize>,
    /// Memory ranges for this NUMA node
    pub memory_ranges: Vec<(usize, usize)>, // (start, size)
    /// Distance to other NUMA nodes
    pub distances: Vec<u8>,
}

/// Cache topology information
#[derive(Debug, Clone)]
pub struct CacheInfo {
    /// Cache level (1, 2, 3, etc.)
    pub level: u8,
    /// Cache type (Instruction, Data, Unified)
    pub cache_type: CacheType,
    /// Cache size in bytes
    pub size: usize,
    /// Cache line size in bytes
    pub line_size: usize,
    /// Cache associativity
    pub associativity: usize,
    /// CPUs sharing this cache
    pub shared_cpus: Vec<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheType {
    Instruction,
    Data,
    Unified,
}

/// System topology information
#[derive(Debug)]
pub struct SystemTopology {
    /// All discovered CPUs
    pub cpus: Vec<CpuInfo>,
    /// NUMA nodes
    pub numa_nodes: Vec<NumaNode>,
    /// Cache hierarchy
    pub caches: Vec<CacheInfo>,
    /// Total number of CPUs
    pub cpu_count: usize,
    /// Maximum CPU ID
    pub max_cpu_id: usize,
}

/// Global system topology
static mut SYSTEM_TOPOLOGY: Option<SystemTopology> = None;
static TOPOLOGY_INITIALIZED: AtomicUsize = AtomicUsize::new(0);

impl SystemTopology {
    pub fn new() -> Self {
        Self {
            cpus: Vec::new(),
            numa_nodes: Vec::new(),
            caches: Vec::new(),
            cpu_count: 0,
            max_cpu_id: 0,
        }
    }

    /// Add a CPU to the topology
    pub fn add_cpu(&mut self, cpu_info: CpuInfo) {
        self.max_cpu_id = self.max_cpu_id.max(cpu_info.cpu_id);
        self.cpus.push(cpu_info);
        self.cpu_count += 1;
    }

    /// Get CPU information by logical CPU ID
    pub fn get_cpu(&self, cpu_id: usize) -> Option<&CpuInfo> {
        self.cpus.iter().find(|cpu| cpu.cpu_id == cpu_id)
    }

    /// Get CPU information by architecture-specific ID
    pub fn get_cpu_by_arch_id(&self, arch_id: usize) -> Option<&CpuInfo> {
        self.cpus.iter().find(|cpu| cpu.arch_id == arch_id)
    }

    /// Get all CPUs in a NUMA node
    pub fn cpus_in_numa_node(&self, node_id: usize) -> Vec<&CpuInfo> {
        self.cpus.iter()
            .filter(|cpu| cpu.numa_node == node_id)
            .collect()
    }

    /// Find the NUMA node for a given CPU
    pub fn numa_node_for_cpu(&self, cpu_id: usize) -> Option<usize> {
        self.get_cpu(cpu_id).map(|cpu| cpu.numa_node)
    }

    /// Get CPUs sharing a cache level
    pub fn cpus_sharing_cache(&self, cpu_id: usize, cache_level: u8) -> Vec<usize> {
        for cache in &self.caches {
            if cache.level == cache_level && cache.shared_cpus.contains(&cpu_id) {
                return cache.shared_cpus.clone();
            }
        }
        vec![cpu_id] // If no cache info, assume CPU has private cache
    }
}

/// Discover CPU topology from device tree
pub fn discover_cpus() {
    if TOPOLOGY_INITIALIZED.compare_exchange(0, 1, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return; // Already initialized
    }

    let mut topology = SystemTopology::new();
    let board_info = board_info();

    // Parse CPUs from device tree or board info
    let cpu_count = board_info.cpu_count.max(1); // At least 1 CPU

    for logical_id in 0..cpu_count {
        let arch_id = if board_info.cpu_count > 1 {
            // Use actual hart IDs from device tree if available
            board_info.cpu_hart_ids[logical_id].unwrap_or(logical_id)
        } else {
            0 // Single CPU system
        };

        let mut cpu_info = CpuInfo::new(logical_id, arch_id);

        // Set CPU frequency if available
        if board_info.cpu_frequency > 0 {
            cpu_info.frequency = board_info.cpu_frequency;
        }

        // Set basic CPU features for RISC-V
        cpu_info.features.has_fpu = true; // Most RISC-V implementations have FPU
        cpu_info.features.cache_sizes = [32768, 32768, 262144, 0]; // Default L1I, L1D, L2, L3
        cpu_info.features.tlb_sizes = [32, 32]; // Default ITLB, DTLB sizes

        // Determine NUMA node (for now, all CPUs in node 0)
        cpu_info.numa_node = 0;

        // Log before moving cpu_info
        info!("Discovered CPU {}: arch_id={} (hart_id), frequency={}Hz",
              logical_id, arch_id, cpu_info.frequency);

        topology.add_cpu(cpu_info);

        // Create per-CPU data structure
        let cpu_type = if logical_id == 0 {
            CpuType::Bootstrap
        } else {
            CpuType::Application
        };

        let cpu_data = Arc::new(CpuData::new(logical_id, cpu_type));
        cpu_data.set_arch_cpu_id(arch_id);
        info!("Set CPU{} arch_cpu_id to {} (hart_id)", logical_id, arch_id);
        set_cpu_data(logical_id, cpu_data);
    }

    // Create default NUMA node
    let numa_node = NumaNode {
        node_id: 0,
        cpu_list: (0..cpu_count).collect(),
        memory_ranges: vec![(board_info.mem.start, board_info.mem.end - board_info.mem.start)],
        distances: vec![10], // Distance to self is typically 10
    };
    topology.numa_nodes.push(numa_node);

    // Create default cache topology
    create_default_cache_topology(&mut topology);

    debug!("CPU topology discovery complete: {} CPUs, {} NUMA nodes",
          topology.cpu_count, topology.numa_nodes.len());

    // Store the topology globally
    unsafe {
        SYSTEM_TOPOLOGY = Some(topology);
    }

    TOPOLOGY_INITIALIZED.store(2, Ordering::Release);
}

/// Create default cache topology for RISC-V systems
fn create_default_cache_topology(topology: &mut SystemTopology) {
    // Create L1 instruction cache (per-CPU)
    for cpu in &topology.cpus {
        let l1i_cache = CacheInfo {
            level: 1,
            cache_type: CacheType::Instruction,
            size: cpu.features.cache_sizes[0],
            line_size: 64,
            associativity: 2,
            shared_cpus: vec![cpu.cpu_id],
        };
        topology.caches.push(l1i_cache);

        // L1 data cache (per-CPU)
        let l1d_cache = CacheInfo {
            level: 1,
            cache_type: CacheType::Data,
            size: cpu.features.cache_sizes[1],
            line_size: 64,
            associativity: 2,
            shared_cpus: vec![cpu.cpu_id],
        };
        topology.caches.push(l1d_cache);
    }

    // L2 cache (shared between pairs of CPUs or all CPUs)
    if topology.cpu_count > 1 {
        let shared_cpus: Vec<usize> = topology.cpus.iter().map(|cpu| cpu.cpu_id).collect();
        let l2_cache = CacheInfo {
            level: 2,
            cache_type: CacheType::Unified,
            size: topology.cpus[0].features.cache_sizes[2],
            line_size: 64,
            associativity: 8,
            shared_cpus,
        };
        topology.caches.push(l2_cache);
    }
}

/// Get the global system topology
pub fn get_topology() -> Option<&'static SystemTopology> {
    if TOPOLOGY_INITIALIZED.load(Ordering::Acquire) == 2 {
        unsafe { core::ptr::addr_of!(SYSTEM_TOPOLOGY).as_ref().unwrap().as_ref() }
    } else {
        None
    }
}

/// Get CPU information for a specific CPU
pub fn get_cpu_info(cpu_id: usize) -> Option<&'static CpuInfo> {
    get_topology()?.get_cpu(cpu_id)
}

/// Get the architecture-specific CPU ID for a logical CPU ID
pub fn logical_to_arch_cpu_id(logical_id: usize) -> Option<usize> {
    get_cpu_info(logical_id).map(|info| info.arch_id)
}

/// Get the logical CPU ID for an architecture-specific CPU ID
pub fn arch_to_logical_cpu_id(arch_id: usize) -> Option<usize> {
    get_topology()?.get_cpu_by_arch_id(arch_id).map(|info| info.cpu_id)
}

/// Get all CPUs in the same NUMA node as the given CPU
pub fn cpus_in_same_numa_node(cpu_id: usize) -> Vec<usize> {
    if let Some(topology) = get_topology() {
        if let Some(numa_node) = topology.numa_node_for_cpu(cpu_id) {
            return topology.numa_nodes[numa_node].cpu_list.clone();
        }
    }
    vec![cpu_id] // Fallback to just the current CPU
}

/// Find the closest CPU to a given CPU (based on NUMA distance)
pub fn find_closest_cpu(cpu_id: usize, exclude: &[usize]) -> Option<usize> {
    let topology = get_topology()?;
    let numa_node = topology.numa_node_for_cpu(cpu_id)?;

    // First try CPUs in the same NUMA node
    for &candidate in &topology.numa_nodes[numa_node].cpu_list {
        if candidate != cpu_id && !exclude.contains(&candidate) {
            return Some(candidate);
        }
    }

    // If no CPU in same NUMA node, try other nodes
    for node in &topology.numa_nodes {
        if node.node_id != numa_node {
            for &candidate in &node.cpu_list {
                if !exclude.contains(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

/// Get CPUs that share a specific cache level with the given CPU
pub fn cpus_sharing_cache_level(cpu_id: usize, cache_level: u8) -> Vec<usize> {
    if let Some(topology) = get_topology() {
        topology.cpus_sharing_cache(cpu_id, cache_level)
    } else {
        vec![cpu_id]
    }
}

/// Calculate CPU affinity mask for optimal performance
pub fn calculate_optimal_affinity(cpu_id: usize) -> u64 {
    let mut mask = 0u64;

    // Include CPUs sharing L2 cache
    for shared_cpu in cpus_sharing_cache_level(cpu_id, 2) {
        if shared_cpu < 64 {
            mask |= 1u64 << shared_cpu;
        }
    }

    // If no L2 sharing info, include NUMA node CPUs
    if mask == 0 {
        for numa_cpu in cpus_in_same_numa_node(cpu_id) {
            if numa_cpu < 64 {
                mask |= 1u64 << numa_cpu;
            }
        }
    }

    // At minimum, include the CPU itself
    if cpu_id < 64 {
        mask |= 1u64 << cpu_id;
    }

    mask
}

/// Get memory bandwidth information for CPU placement decisions
pub fn get_memory_bandwidth_info(cpu_id: usize) -> Option<(f64, f64)> {
    // This would be implemented based on hardware specifications
    // For now, return default values
    let topology = get_topology()?;
    let cpu_info = topology.get_cpu(cpu_id)?;

    // Estimate based on CPU frequency and NUMA node
    let base_bandwidth = 25.6; // GB/s, typical DDR4 bandwidth
    let numa_penalty = if topology.numa_nodes.len() > 1 { 0.8 } else { 1.0 };

    Some((base_bandwidth * numa_penalty, base_bandwidth))
}

/// Display topology information for debugging
pub fn print_topology_info() {
    if let Some(topology) = get_topology() {
        info!("=== CPU Topology Information ===");
        info!("Total CPUs: {}", topology.cpu_count);
        info!("Max CPU ID: {}", topology.max_cpu_id);

        for cpu in &topology.cpus {
            info!("CPU {}: arch_id={}, numa_node={}, freq={}MHz",
                  cpu.cpu_id, cpu.arch_id, cpu.numa_node, cpu.frequency / 1_000_000);
        }

        info!("NUMA Nodes: {}", topology.numa_nodes.len());
        for node in &topology.numa_nodes {
            info!("  Node {}: CPUs={:?}, Memory ranges: {:?}",
                  node.node_id, node.cpu_list, node.memory_ranges);
        }

        info!("Cache Information:");
        for cache in &topology.caches {
            info!("  L{} {:?}: {}KB, line_size={}, shared_by={:?}",
                  cache.level, cache.cache_type, cache.size / 1024,
                  cache.line_size, cache.shared_cpus);
        }
        info!("===============================");
    } else {
        warn!("CPU topology not yet discovered");
    }
}