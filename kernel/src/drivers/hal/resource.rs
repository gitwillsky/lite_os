use core::fmt;
use alloc::sync::Arc;
use alloc::vec::Vec;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use spin::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceType {
    Memory,
    IoPort,
    Interrupt,
    Dma,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceError {
    AlreadyInUse,
    InvalidRange,
    NotFound,
    ConflictDetected,
    AccessDenied,
    OutOfResources,
}

impl fmt::Display for ResourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResourceError::AlreadyInUse => write!(f, "Resource already in use"),
            ResourceError::InvalidRange => write!(f, "Invalid resource range"),
            ResourceError::NotFound => write!(f, "Resource not found"),
            ResourceError::ConflictDetected => write!(f, "Resource conflict detected"),
            ResourceError::AccessDenied => write!(f, "Resource access denied"),
            ResourceError::OutOfResources => write!(f, "Out of resources"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryRange {
    pub start: usize,
    pub size: usize,
    pub cached: bool,
    pub writable: bool,
    pub executable: bool,
}

impl MemoryRange {
    pub fn new(start: usize, size: usize) -> Self {
        Self {
            start,
            size,
            cached: true,
            writable: true,
            executable: false,
        }
    }

    pub fn with_attributes(
        start: usize,
        size: usize,
        cached: bool,
        writable: bool,
        executable: bool,
    ) -> Self {
        Self {
            start,
            size,
            cached,
            writable,
            executable,
        }
    }

    pub fn end(&self) -> usize {
        self.start + self.size
    }

    pub fn contains(&self, addr: usize) -> bool {
        addr >= self.start && addr < self.end()
    }

    pub fn overlaps(&self, other: &MemoryRange) -> bool {
        !(self.end() <= other.start || other.end() <= self.start)
    }
}

#[derive(Debug, Clone)]
pub struct IoPortRange {
    pub start: u16,
    pub size: u16,
}

impl IoPortRange {
    pub fn new(start: u16, size: u16) -> Self {
        Self { start, size }
    }

    pub fn end(&self) -> u16 {
        self.start + self.size
    }

    pub fn contains(&self, port: u16) -> bool {
        port >= self.start && port < self.end()
    }

    pub fn overlaps(&self, other: &IoPortRange) -> bool {
        !(self.end() <= other.start || other.end() <= self.start)
    }
}

#[derive(Debug, Clone)]
pub struct IrqResource {
    pub irq_num: u32,
    pub shared: bool,
    pub level_triggered: bool,
    pub active_high: bool,
}

impl IrqResource {
    pub fn new(irq_num: u32) -> Self {
        Self {
            irq_num,
            shared: false,
            level_triggered: true,
            active_high: true,
        }
    }

    pub fn shared(mut self) -> Self {
        self.shared = true;
        self
    }

    pub fn edge_triggered(mut self) -> Self {
        self.level_triggered = false;
        self
    }

    pub fn active_low(mut self) -> Self {
        self.active_high = false;
        self
    }
}

#[derive(Debug, Clone)]
pub enum Resource {
    Memory(MemoryRange),
    IoPort(IoPortRange),
    Interrupt(IrqResource),
    Dma { channel: u32, width: u8 },
}

impl Resource {
    pub fn resource_type(&self) -> ResourceType {
        match self {
            Resource::Memory(_) => ResourceType::Memory,
            Resource::IoPort(_) => ResourceType::IoPort,
            Resource::Interrupt(_) => ResourceType::Interrupt,
            Resource::Dma { .. } => ResourceType::Dma,
        }
    }

    pub fn conflicts_with(&self, other: &Resource) -> bool {
        match (self, other) {
            (Resource::Memory(a), Resource::Memory(b)) => a.overlaps(b),
            (Resource::IoPort(a), Resource::IoPort(b)) => a.overlaps(b),
            (Resource::Interrupt(a), Resource::Interrupt(b)) => {
                a.irq_num == b.irq_num && (!a.shared || !b.shared)
            }
            (Resource::Dma { channel: a, .. }, Resource::Dma { channel: b, .. }) => a == b,
            _ => false,
        }
    }
}

#[derive(Debug)]
struct ResourceAllocation {
    resource: Resource,
    owner: String,
    exclusive: bool,
}

pub trait ResourceManager: Send + Sync {
    fn request_resource(
        &mut self,
        resource: Resource,
        owner: &str,
    ) -> Result<(), ResourceError>;

    fn release_resource(
        &mut self,
        resource: &Resource,
        owner: &str,
    ) -> Result<(), ResourceError>;

    fn find_free_memory(
        &self,
        size: usize,
        alignment: usize,
        start: usize,
        end: usize,
    ) -> Option<usize>;

    fn is_available(&self, resource: &Resource) -> bool;

    fn get_conflicts(&self, resource: &Resource) -> Vec<String>;
}

pub struct SystemResourceManager {
    allocations: Mutex<Vec<ResourceAllocation>>,
    memory_map: Mutex<BTreeMap<usize, MemoryRange>>,
    io_ports: Mutex<BTreeMap<u16, IoPortRange>>,
    interrupts: Mutex<BTreeMap<u32, Vec<String>>>, // IRQ -> owners
    dma_channels: Mutex<BTreeMap<u32, Vec<String>>>, // DMA channel -> owners
}

impl SystemResourceManager {
    pub fn new() -> Self {
        Self {
            allocations: Mutex::new(Vec::new()),
            memory_map: Mutex::new(BTreeMap::new()),
            io_ports: Mutex::new(BTreeMap::new()),
            interrupts: Mutex::new(BTreeMap::new()),
            dma_channels: Mutex::new(BTreeMap::new()),
        }
    }

    fn check_memory_conflict(&self, range: &MemoryRange) -> Vec<String> {
        let allocations = self.allocations.lock();
        let mut conflicts = Vec::new();

        for allocation in allocations.iter() {
            if let Resource::Memory(existing) = &allocation.resource {
                if range.overlaps(existing) {
                    conflicts.push(allocation.owner.clone());
                }
            }
        }

        conflicts
    }

    fn check_io_conflict(&self, range: &IoPortRange) -> Vec<String> {
        let allocations = self.allocations.lock();
        let mut conflicts = Vec::new();

        for allocation in allocations.iter() {
            if let Resource::IoPort(existing) = &allocation.resource {
                if range.overlaps(existing) {
                    conflicts.push(allocation.owner.clone());
                }
            }
        }

        conflicts
    }

    fn check_irq_conflict(&self, irq: &IrqResource) -> Vec<String> {
        let interrupts = self.interrupts.lock();

        if let Some(owners) = interrupts.get(&irq.irq_num) {
            if !irq.shared || owners.iter().any(|_| !irq.shared) {
                return owners.clone();
            }
        }

        Vec::new()
    }

    fn check_dma_conflict(&self, channel: u32) -> Vec<String> {
        let dmas = self.dma_channels.lock();
        if let Some(owners) = dmas.get(&channel) {
            return owners.clone();
        }
        Vec::new()
    }
}

impl ResourceManager for SystemResourceManager {
    fn request_resource(
        &mut self,
        resource: Resource,
        owner: &str,
    ) -> Result<(), ResourceError> {
        let conflicts = self.get_conflicts(&resource);
        if !conflicts.is_empty() {
            return Err(ResourceError::ConflictDetected);
        }

        let allocation = ResourceAllocation {
            resource: resource.clone(),
            owner: owner.to_string(),
            exclusive: true,
        };

        // Update internal tracking structures
        match &resource {
            Resource::Memory(range) => {
                let mut memory_map = self.memory_map.lock();
                memory_map.insert(range.start, range.clone());
            }
            Resource::IoPort(range) => {
                let mut io_ports = self.io_ports.lock();
                io_ports.insert(range.start, range.clone());
            }
            Resource::Interrupt(irq) => {
                let mut interrupts = self.interrupts.lock();
                interrupts.entry(irq.irq_num)
                    .or_insert_with(Vec::new)
                    .push(owner.to_string());
            }
            Resource::Dma { channel, .. } => {
                let mut dma = self.dma_channels.lock();
                dma.entry(*channel)
                    .or_insert_with(Vec::new)
                    .push(owner.to_string());
            }
        }

        let mut allocations = self.allocations.lock();
        allocations.push(allocation);

        Ok(())
    }

    fn release_resource(
        &mut self,
        resource: &Resource,
        owner: &str,
    ) -> Result<(), ResourceError> {
        let mut allocations = self.allocations.lock();

        let initial_len = allocations.len();
        allocations.retain(|allocation| {
            !(allocation.owner == owner && allocation.resource.resource_type() == resource.resource_type())
        });

        if allocations.len() == initial_len {
            return Err(ResourceError::NotFound);
        }

        // Update internal tracking structures
        match resource {
            Resource::Memory(range) => {
                let mut memory_map = self.memory_map.lock();
                memory_map.remove(&range.start);
            }
            Resource::IoPort(range) => {
                let mut io_ports = self.io_ports.lock();
                io_ports.remove(&range.start);
            }
            Resource::Interrupt(irq) => {
                let mut interrupts = self.interrupts.lock();
                if let Some(owners) = interrupts.get_mut(&irq.irq_num) {
                    owners.retain(|o| o != owner);
                    if owners.is_empty() {
                        interrupts.remove(&irq.irq_num);
                    }
                }
            }
            Resource::Dma { channel, .. } => {
                let mut dma = self.dma_channels.lock();
                if let Some(owners) = dma.get_mut(channel) {
                    owners.retain(|o| o != owner);
                    if owners.is_empty() {
                        dma.remove(channel);
                    }
                }
            }
        }

        Ok(())
    }

    fn find_free_memory(
        &self,
        size: usize,
        alignment: usize,
        start: usize,
        end: usize,
    ) -> Option<usize> {
        let memory_map = self.memory_map.lock();

        let mut current = (start + alignment - 1) & !(alignment - 1); // Align start

        while current + size <= end {
            let test_range = MemoryRange::new(current, size);

            let mut conflicts = false;
            for (_, existing) in memory_map.iter() {
                if test_range.overlaps(existing) {
                    conflicts = true;
                    current = existing.end();
                    current = (current + alignment - 1) & !(alignment - 1); // Re-align
                    break;
                }
            }

            if !conflicts {
                return Some(current);
            }
        }

        None
    }

    fn is_available(&self, resource: &Resource) -> bool {
        self.get_conflicts(resource).is_empty()
    }

    fn get_conflicts(&self, resource: &Resource) -> Vec<String> {
        match resource {
            Resource::Memory(range) => self.check_memory_conflict(range),
            Resource::IoPort(range) => self.check_io_conflict(range),
            Resource::Interrupt(irq) => self.check_irq_conflict(irq),
            Resource::Dma { channel, .. } => self.check_dma_conflict(*channel),
        }
    }
}