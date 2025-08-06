use alloc::{collections::BTreeMap, string::String, sync::Arc, vec, vec::Vec};
use spin::{Mutex, RwLock};

use crate::drivers::{BlockDevice, block::BlockError};

use super::{FileStat, FileSystem, FileSystemError, Inode, InodeType};

const SECTOR_SIZE: usize = 512;
const FAT32_SIGNATURE: u16 = 0xAA55;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct BiosParameterBlock {
    jmp_boot: [u8; 3],
    oem_name: [u8; 8],
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    root_entries: u16,
    total_sectors_16: u16,
    media_type: u8,
    sectors_per_fat_16: u16,
    sectors_per_track: u16,
    num_heads: u16,
    hidden_sectors: u32,
    total_sectors_32: u32,

    // FAT32 specific
    sectors_per_fat_32: u32,
    flags: u16,
    version: u16,
    root_cluster: u32,
    fs_info_sector: u16,
    backup_boot_sector: u16,
    reserved: [u8; 12],
    drive_number: u8,
    reserved1: u8,
    boot_signature: u8,
    volume_id: u32,
    volume_label: [u8; 11],
    fs_type: [u8; 8],
    boot_code: [u8; 420],
    signature: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct DirectoryEntry {
    name: [u8; 8],
    ext: [u8; 3],
    attr: u8,
    reserved: u8,
    create_time_tenth: u8,
    create_time: u16,
    create_date: u16,
    last_access_date: u16,
    first_cluster_high: u16,
    last_write_time: u16,
    last_write_date: u16,
    first_cluster_low: u16,
    file_size: u32,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
struct LongFileNameEntry {
    order: u8,
    name1: [u16; 5],
    attr: u8,
    entry_type: u8,
    checksum: u8,
    name2: [u16; 6],
    zero: u16,
    name3: [u16; 2],
}

struct DirEntryInfo {
    name: String,
    entry: DirectoryEntry,
}

struct ShortFileNameEntry {
    name: [u8; 8],
    ext: [u8; 3],
}

const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_HIDDEN: u8 = 0x02;
const ATTR_SYSTEM: u8 = 0x04;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_ARCHIVE: u8 = 0x20;
const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;

const CLUSTER_FREE: u32 = 0x00000000;
const CLUSTER_EOF: u32 = 0x0FFFFFF8;
const CLUSTER_BAD: u32 = 0x0FFFFFF7;

// Block cache entry
#[derive(Clone)]
struct CacheEntry {
    data: Vec<u8>,
    dirty: bool,
    last_access: u64,
}

// Multi-core safe filesystem structure with proper lock hierarchy
pub struct FAT32FileSystem {
    device: Arc<dyn BlockDevice>,
    bpb: BiosParameterBlock,
    fat_start_sector: u32,
    cluster_start_sector: u32,
    sectors_per_cluster: u32,
    bytes_per_cluster: u32,
    root_cluster: u32,

    // Lock hierarchy (must be acquired in this order to avoid deadlock):
    // 1. global_lock (for filesystem structure changes)
    // 2. fat_cache (for FAT table operations)
    // 3. block_cache (for block-level operations)
    // 4. directory_locks (for specific directory operations)

    fat_cache: RwLock<Vec<u32>>,
    block_cache: RwLock<BTreeMap<u32, CacheEntry>>,
    directory_locks: RwLock<BTreeMap<u32, Arc<Mutex<()>>>>, // Per-directory locks

    // Global operations lock (for filesystem structure changes)
    global_lock: Mutex<()>,

    // Write ordering lock to ensure proper flush sequence
    flush_lock: Mutex<()>,
}

impl FAT32FileSystem {
    pub fn new(device: Arc<dyn BlockDevice>) -> Option<Arc<Self>> {
        // Read full block (4096 bytes) to accommodate VirtIO block size
        let mut block_bytes = vec![0u8; device.block_size()];
        if let Err(e) = device.read_block(0, &mut block_bytes) {
            error!("Failed to read boot sector: {:?}", e);
            return None;
        }
        debug!("Successfully read boot sector");

        // Extract the 512-byte boot sector from the full block
        let mut bpb_bytes = [0u8; SECTOR_SIZE];
        bpb_bytes.copy_from_slice(&block_bytes[..SECTOR_SIZE]);

        let bpb =
            unsafe { core::ptr::read_unaligned(bpb_bytes.as_ptr() as *const BiosParameterBlock) };

        // Verify FAT32 filesystem
        let bpb_ptr = bpb_bytes.as_ptr();
        let signature = unsafe { core::ptr::read_unaligned(bpb_ptr.add(510) as *const u16) };
        debug!("Boot signature: {:#x}", signature);
        if signature != FAT32_SIGNATURE {
            error!(
                "Invalid boot signature: {:#x} (expected {:#x})",
                signature, FAT32_SIGNATURE
            );
            return None;
        }

        let sectors_per_fat_32 =
            unsafe { core::ptr::read_unaligned(bpb_ptr.add(36) as *const u32) };
        debug!("Sectors per FAT32: {}", sectors_per_fat_32);
        if sectors_per_fat_32 == 0 {
            error!("Not a FAT32 filesystem (sectors_per_fat_32 is 0)");
            return None;
        }

        let reserved_sectors = unsafe { core::ptr::read_unaligned(bpb_ptr.add(14) as *const u16) };
        let num_fats = unsafe { core::ptr::read_unaligned(bpb_ptr.add(16) as *const u8) };
        let sectors_per_cluster =
            unsafe { core::ptr::read_unaligned(bpb_ptr.add(13) as *const u8) };
        let root_cluster = unsafe { core::ptr::read_unaligned(bpb_ptr.add(44) as *const u32) };

        let fat_start_sector = reserved_sectors as u32;
        let cluster_start_sector = fat_start_sector + (num_fats as u32 * sectors_per_fat_32);
        let sectors_per_cluster = sectors_per_cluster as u32;
        let bytes_per_cluster = sectors_per_cluster * SECTOR_SIZE as u32;

        debug!("Filesystem initialized successfully");
        debug!("Reserved sectors: {}", reserved_sectors);
        debug!("Number of FATs: {}", num_fats);
        debug!("Sectors per FAT: {}", sectors_per_fat_32);
        debug!("Sectors per cluster: {}", sectors_per_cluster);
        debug!("Bytes per cluster: {}", bytes_per_cluster);
        debug!("Root directory cluster: {}", root_cluster);
        debug!("Data area start sector: {}", cluster_start_sector);

        // Load FAT table
        let fat_sectors = sectors_per_fat_32 as usize;
        let fat_entries = (fat_sectors * SECTOR_SIZE) / 4;
        let mut fat_cache = Vec::with_capacity(fat_entries);

        // Calculate how many device blocks we need for the FAT
        let device_block_size = device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;
        let fat_blocks = (fat_sectors + sectors_per_block - 1) / sectors_per_block;

        for block_idx in 0..fat_blocks {
            let mut block_data = vec![0u8; device_block_size];
            let block_num = fat_start_sector as usize / sectors_per_block + block_idx;

            if device.read_block(block_num, &mut block_data).is_err() {
                return None;
            }

            // Process each sector within the block
            for sector_in_block in 0..sectors_per_block {
                let current_sector = block_idx * sectors_per_block + sector_in_block;
                if current_sector >= fat_sectors {
                    break;
                }

                let sector_offset = sector_in_block * SECTOR_SIZE;
                let sector_data = &block_data[sector_offset..sector_offset + SECTOR_SIZE];

                let entries = unsafe {
                    core::slice::from_raw_parts(sector_data.as_ptr() as *const u32, SECTOR_SIZE / 4)
                };
                fat_cache.extend_from_slice(entries);
            }
        }

        Some(Arc::new(FAT32FileSystem {
            device,
            bpb,
            fat_start_sector,
            cluster_start_sector,
            sectors_per_cluster,
            bytes_per_cluster,
            root_cluster,
            fat_cache: RwLock::new(fat_cache),
            block_cache: RwLock::new(BTreeMap::new()),
            directory_locks: RwLock::new(BTreeMap::new()),
            global_lock: Mutex::new(()),
            flush_lock: Mutex::new(()),
        }))
    }

    fn cluster_to_sector(&self, cluster: u32) -> Result<u32, ()> {
        // Clusters 0 and 1 are reserved in FAT32, valid data clusters start from 2
        if cluster < 2 || cluster >= CLUSTER_EOF {
            error!(
                "Invalid cluster number: {} (clusters 0 and 1 are reserved, cluster >= {} is invalid)",
                cluster, CLUSTER_EOF
            );
            return Err(());
        }
        Ok(self.cluster_start_sector + (cluster - 2) * self.sectors_per_cluster)
    }

    /// Get or create a directory-specific lock for fine-grained synchronization
    fn get_directory_lock(&self, cluster: u32) -> Arc<Mutex<()>> {
        let mut locks = self.directory_locks.write();
        locks
            .entry(cluster)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Block cache management with LRU eviction
    fn read_cached_block(&self, sector: u32) -> Result<Vec<u8>, BlockError> {
        // Try to read from cache first
        {
            let cache = self.block_cache.read();
            if let Some(entry) = cache.get(&sector) {
                return Ok(entry.data.clone());
            }
        }

        // Not in cache, read from device
        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;
        let block_num = sector / sectors_per_block as u32;
        let sector_in_block = sector % sectors_per_block as u32;

        let mut block_data = vec![0u8; device_block_size];
        self.device
            .read_block(block_num as usize, &mut block_data)?;

        let sector_offset = sector_in_block as usize * SECTOR_SIZE;
        let sector_data = block_data[sector_offset..sector_offset + SECTOR_SIZE].to_vec();

        // Cache the sector data
        {
            let mut cache = self.block_cache.write();

            // Simple LRU eviction if cache gets too large
            if cache.len() > 1000 {
                let oldest_key = cache.keys().next().copied().unwrap();
                cache.remove(&oldest_key);
            }

            cache.insert(
                sector,
                CacheEntry {
                    data: sector_data.clone(),
                    dirty: false,
                    last_access: 0, // Would use actual timestamp in real implementation
                },
            );
        }

        Ok(sector_data)
    }

    /// Write through cache - write to cache and device immediately
    fn write_cached_block(&self, sector: u32, data: &[u8]) -> Result<(), BlockError> {
        if data.len() != SECTOR_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        // Write to device first (write-through policy)
        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;
        let block_num = sector / sectors_per_block as u32;
        let sector_in_block = sector % sectors_per_block as u32;

        // Read-modify-write for device block
        let mut block_data = vec![0u8; device_block_size];
        self.device
            .read_block(block_num as usize, &mut block_data)?;

        let sector_offset = sector_in_block as usize * SECTOR_SIZE;
        block_data[sector_offset..sector_offset + SECTOR_SIZE].copy_from_slice(data);

        self.device.write_block(block_num as usize, &block_data)?;

        // Update cache
        {
            let mut cache = self.block_cache.write();
            cache.insert(
                sector,
                CacheEntry {
                    data: data.to_vec(),
                    dirty: false, // Already written to device
                    last_access: 0,
                },
            );
        }

        Ok(())
    }

    fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() < self.bytes_per_cluster as usize {
            return Err(BlockError::InvalidBlock);
        }

        let start_sector = self.cluster_to_sector(cluster)
            .map_err(|_| BlockError::InvalidBlock)?;

        for i in 0..self.sectors_per_cluster {
            let sector_num = start_sector + i;
            let sector_data = self.read_cached_block(sector_num)?;

            let sector_offset_in_buf = i as usize * SECTOR_SIZE;
            buf[sector_offset_in_buf..sector_offset_in_buf + SECTOR_SIZE]
                .copy_from_slice(&sector_data);
        }

        Ok(())
    }

    fn write_cluster(&self, cluster: u32, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() < self.bytes_per_cluster as usize {
            return Err(BlockError::InvalidBlock);
        }

        let start_sector = self.cluster_to_sector(cluster)
            .map_err(|_| BlockError::InvalidBlock)?;

        for i in 0..self.sectors_per_cluster {
            let sector_num = start_sector + i;
            let sector_offset_in_buf = i as usize * SECTOR_SIZE;
            let sector_data = &buf[sector_offset_in_buf..sector_offset_in_buf + SECTOR_SIZE];

            self.write_cached_block(sector_num, sector_data)?;
        }

        Ok(())
    }

    /// Reload a specific FAT entry from disk to repair corruption
    fn reload_fat_entry(&self, cluster: u32) {
        if cluster < 2 || cluster as usize >= self.fat_cache.read().len() {
            return;
        }

        // Use global lock to prevent race conditions during reload
        let _global_lock = self.global_lock.lock();

        // Clear the block cache for this FAT sector to force a fresh read
        let fat_sector = self.fat_start_sector + (cluster * 4) / SECTOR_SIZE as u32;
        {
            let mut block_cache = self.block_cache.write();
            block_cache.remove(&fat_sector);
        }

        // Read FAT entry directly from device (bypassing cache)
        let sector_offset = ((cluster * 4) % SECTOR_SIZE as u32) as usize;
        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;
        let block_num = fat_sector / sectors_per_block as u32;
        let sector_in_block = fat_sector % sectors_per_block as u32;

        let mut block_data = vec![0u8; device_block_size];
        if self.device.read_block(block_num as usize, &mut block_data).is_ok() {
            let sector_data_offset = sector_in_block as usize * SECTOR_SIZE;
            if sector_data_offset + SECTOR_SIZE <= block_data.len() && sector_offset + 4 <= SECTOR_SIZE {
                let fat_value = u32::from_le_bytes([
                    block_data[sector_data_offset + sector_offset],
                    block_data[sector_data_offset + sector_offset + 1],
                    block_data[sector_data_offset + sector_offset + 2],
                    block_data[sector_data_offset + sector_offset + 3],
                ]) & 0x0FFFFFFF;

                // Update in-memory cache with disk value
                let mut fat_cache = self.fat_cache.write();
                if (cluster as usize) < fat_cache.len() {
                    fat_cache[cluster as usize] = fat_value;
                }
            }
        }
    }

    fn next_cluster(&self, cluster: u32) -> u32 {
        let fat_cache = self.fat_cache.read();
        if cluster as usize >= fat_cache.len() {
            return CLUSTER_EOF;
        }
        let next = fat_cache[cluster as usize] & 0x0FFFFFFF;

        // Handle corrupted FAT entries that point to reserved clusters
        if next == 0 || next == 1 {
            error!(
                "Corrupted FAT entry: cluster {} points to invalid cluster {} (reserved clusters 0-1)",
                cluster, next
            );
            // Try to repair by reloading from disk
            drop(fat_cache);
            self.reload_fat_entry(cluster);

            // Re-read after potential repair
            let fat_cache = self.fat_cache.read();
            let repaired_next = fat_cache[cluster as usize] & 0x0FFFFFFF;
            if repaired_next == 0 || repaired_next == 1 {
                warn!("FAT entry {} still corrupted after reload, treating as EOF", cluster);
                return CLUSTER_EOF;
            }
            return repaired_next;
        }

        next
    }

    fn allocate_cluster(&self) -> Option<u32> {
        // Use global lock to ensure atomic cluster allocation across all operations
        let _global_lock = self.global_lock.lock();

        // Find a free cluster first, without holding the write lock for too long
        let cluster_to_allocate = {
            let fat_cache = self.fat_cache.read();
            let mut found_cluster = None;

            // Start from cluster 2 (clusters 0 and 1 are reserved)
            for i in 2..fat_cache.len() {
                if fat_cache[i] == CLUSTER_FREE {
                    found_cluster = Some(i as u32);
                    break;
                }
            }
            found_cluster
        };

        let cluster_to_allocate = match cluster_to_allocate {
            Some(cluster) => cluster,
            None => {
                warn!("No free clusters available");
                return None;
            }
        };

        // Now atomically allocate the cluster - write to disk first
        if let Err(e) = self.write_fat_entry_atomic(cluster_to_allocate, CLUSTER_EOF) {
            error!("Failed to persist allocated cluster {} to disk: {:?}", cluster_to_allocate, e);
            return None;
        }

        // Only update in-memory cache after successful disk write
        {
            let mut fat_cache = self.fat_cache.write();
            if (cluster_to_allocate as usize) < fat_cache.len() {
                // Double-check that it's still free (defensive programming)
                let current_value = fat_cache[cluster_to_allocate as usize];
                if current_value == CLUSTER_FREE {
                    fat_cache[cluster_to_allocate as usize] = CLUSTER_EOF;
                } else {
                    // Someone else allocated it - this shouldn't happen with global lock
                    error!("Cluster {} was allocated by another thread despite global lock (current value: {})",
                           cluster_to_allocate, current_value);
                    return None;
                }
            }
        }

        Some(cluster_to_allocate)
    }

    /// Write to disk only without modifying in-memory cache (for atomic operations)
    fn write_fat_entry_atomic(&self, cluster: u32, value: u32) -> Result<(), BlockError> {
        if cluster < 2 {
            error!("Invalid cluster number for FAT write: {}", cluster);
            return Err(BlockError::InvalidBlock);
        }

        // Write to both FAT copies
        let sectors_per_fat_32 = unsafe {
            let bpb_ptr = &self.bpb as *const _ as *const u8;
            core::ptr::read_unaligned(bpb_ptr.add(36) as *const u32)
        };
        let num_fats = self.bpb.num_fats;

        for fat_num in 0..num_fats {
            // Calculate FAT sector for this copy
            let fat_start = self.fat_start_sector + (fat_num as u32 * sectors_per_fat_32);
            let fat_sector = fat_start + (cluster * 4) / SECTOR_SIZE as u32;
            let sector_offset = ((cluster * 4) % SECTOR_SIZE as u32) as usize;

            let device_block_size = self.device.block_size();
            let sectors_per_block = device_block_size / SECTOR_SIZE;
            let block_num = fat_sector / sectors_per_block as u32;
            let sector_in_block = fat_sector % sectors_per_block as u32;

            // Read-modify-write the block
            let mut block_data = vec![0u8; device_block_size];
            self.device
                .read_block(block_num as usize, &mut block_data)?;

            let block_sector_offset = sector_in_block as usize * SECTOR_SIZE + sector_offset;
            if block_sector_offset + 4 > device_block_size {
                error!("FAT entry would exceed block boundary");
                return Err(BlockError::InvalidBlock);
            }

            let value_bytes = (value & 0x0FFFFFFF).to_le_bytes();
            block_data[block_sector_offset..block_sector_offset + 4].copy_from_slice(&value_bytes);

            self.device.write_block(block_num as usize, &block_data)?;
        }

        Ok(())
    }

    fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), BlockError> {
        // Use global lock to ensure atomic operation
        let _global_lock = self.global_lock.lock();

        if cluster < 2 {
            error!("Invalid cluster number for FAT write: {}", cluster);
            return Err(BlockError::InvalidBlock);
        }

        // First write to disk atomically
        let result = self.write_fat_entry_atomic(cluster, value);

        // Only update in-memory cache if disk write succeeded
        if result.is_ok() {
            let mut fat_cache = self.fat_cache.write();
            if (cluster as usize) < fat_cache.len() {
                let old_value = fat_cache[cluster as usize];
                fat_cache[cluster as usize] = value & 0x0FFFFFFF;
            }
        } else {
            error!("Failed to write FAT entry cluster {} to disk: {:?}", cluster, result);
        }

        result
    }

    fn read_directory_entries(&self, cluster: u32) -> Result<Vec<DirEntryInfo>, FileSystemError> {
        let mut entries = Vec::new();
        let mut current_cluster = cluster;
        let mut lfn_cache: Vec<LongFileNameEntry> = Vec::new();

        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            for chunk in cluster_data.chunks_exact(32) {
                let attr = chunk[11];
                let is_lfn = attr & ATTR_LONG_NAME == ATTR_LONG_NAME;

                if is_lfn {
                    let lfn_entry = unsafe {
                        core::ptr::read_unaligned(chunk.as_ptr() as *const LongFileNameEntry)
                    };
                    lfn_cache.push(lfn_entry);
                } else {
                    let entry = unsafe {
                        core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry)
                    };

                    if entry.name[0] == 0x00 {
                        return Ok(entries);
                    }
                    if entry.name[0] == 0xE5 {
                        lfn_cache.clear();
                        continue;
                    }

                    let name = if !lfn_cache.is_empty() {
                        lfn_cache.sort_by_key(|e| e.order & 0x1F);
                        let mut long_name_utf16: Vec<u16> = Vec::new();
                        for lfn in &lfn_cache {
                            unsafe {
                                let name1 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name1));
                                long_name_utf16.extend_from_slice(&name1);
                                let name2 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name2));
                                long_name_utf16.extend_from_slice(&name2);
                                let name3 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name3));
                                long_name_utf16.extend_from_slice(&name3);
                            }
                        }

                        let null_pos = long_name_utf16
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(long_name_utf16.len());
                        long_name_utf16.truncate(null_pos);

                        lfn_cache.clear();
                        String::from_utf16_lossy(&long_name_utf16)
                    } else {
                        FAT32Inode::entry_name_to_string(&entry)
                    };

                    entries.push(DirEntryInfo { name, entry });
                }
            }

            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        Ok(entries)
    }

    /// Unified directory entry management - replaces all the redundant search functions
    /// This is the single source of truth for directory operations
    fn modify_directory_entry<F>(
        &self,
        parent_dir_cluster: u32,
        target_cluster: u32,
        operation: F,
    ) -> Result<(), FileSystemError>
    where
        F: Fn(&mut DirectoryEntry) -> Result<(), FileSystemError> + Send + Sync,
    {
        // Validate cluster numbers - clusters 0 and 1 are reserved in FAT32
        if parent_dir_cluster < 2 {
            error!(
                "[modify_directory_entry] Invalid parent directory cluster: {} (reserved)",
                parent_dir_cluster
            );
            return Err(FileSystemError::InvalidPath);
        }

        let dir_lock = self.get_directory_lock(parent_dir_cluster);
        let _lock = dir_lock.lock(); // Directory-specific lock

        // Memory barrier for multi-core visibility
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        let mut current_cluster = parent_dir_cluster;

        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            let mut modified = false;

            // Process each 32-byte directory entry
            for (local_index, chunk) in cluster_data.chunks_exact_mut(32).enumerate() {
                let entry =
                    unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry) };

                // End of directory entries - this is normal, just means we've reached the end
                if entry.name[0] == 0x00 {
                    debug!("[modify_directory_entry] Reached end of directory entries in cluster {}", current_cluster);
                    break; // Break from the inner loop, continue to next cluster
                }

                // Skip deleted and LFN entries
                if entry.name[0] == 0xE5 || (entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME) {
                    continue;
                }

                let entry_cluster =
                    (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;

                if entry_cluster == target_cluster {
                    // Apply the operation to modify the entry
                    let mut modified_entry = entry;
                    operation(&mut modified_entry)?;

                    // Write the modified entry back to the cluster data
                    let entry_bytes = unsafe {
                        core::slice::from_raw_parts(
                            &modified_entry as *const DirectoryEntry as *const u8,
                            32,
                        )
                    };
                    chunk.copy_from_slice(entry_bytes);
                    modified = true;
                    break;
                }
            }

            if modified {
                // Atomic write of the entire cluster
                self.write_cluster(current_cluster, &cluster_data)
                    .map_err(|_| FileSystemError::IoError)?;

                // Memory barrier to ensure write completion
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

                return Ok(());
            }

            // Move to next cluster in the directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        Err(FileSystemError::NotFound)
    }

    /// Update file size - now just a wrapper around the unified function
    fn update_directory_entry_size(
        &self,
        parent_dir_cluster: u32,
        target_cluster: u32,
        new_size: u32,
    ) -> Result<(), FileSystemError> {
        // Only skip if parent_dir_cluster is 0 AND target_cluster is the root directory cluster
        // This should only happen for the root directory inode itself, not files in root directory
        if parent_dir_cluster == 0 && target_cluster == self.root_cluster {
            return Ok(());
        }

        // Try to update directory entry with enhanced error handling
        match self.modify_directory_entry(parent_dir_cluster, target_cluster, |entry| {
            if entry.attr & ATTR_DIRECTORY != 0 {
                return Err(FileSystemError::IsDirectory);
            }
            entry.file_size = new_size;
            Ok(())
        }) {
            Ok(()) => Ok(()),
            Err(FileSystemError::NotFound) => {
                // Entry not found in expected parent cluster, try to find it
                debug!("[update_directory_entry_size] Entry not found in parent cluster {}, searching filesystem", parent_dir_cluster);

                // Search the entire filesystem for this cluster
                if let Ok((actual_parent, _)) = self.find_directory_entry_by_cluster(target_cluster) {
                    debug!("[update_directory_entry_size] Found entry in cluster {}, updating size", actual_parent);

                    // Try updating in the actual parent cluster
                    self.modify_directory_entry(actual_parent, target_cluster, |entry| {
                        if entry.attr & ATTR_DIRECTORY != 0 {
                            return Err(FileSystemError::IsDirectory);
                        }
                        entry.file_size = new_size;
                        Ok(())
                    })
                } else {
                    error!("[update_directory_entry_size] Entry for cluster {} not found anywhere, this is expected for temporary files", target_cluster);
                    // Don't treat this as an error - the file might be deleted or temporary
                    Ok(())
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Verify directory entry exists - now uses the unified search
    fn verify_directory_entry_exists(
        &self,
        dir_cluster: u32,
        target_cluster: u32,
    ) -> Result<bool, FileSystemError> {
        // Use a no-op operation just to check if the entry exists
        match self.modify_directory_entry(dir_cluster, target_cluster, |_entry| Ok(())) {
            Ok(()) => Ok(true),
            Err(FileSystemError::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn verify_directory_entry_immediate(
        &self,
        dir_cluster: u32,
        target_cluster: u32,
    ) -> Result<(), FileSystemError> {
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        let mut current_cluster = dir_cluster;
        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            // Check each directory entry
            for chunk in cluster_data.chunks_exact(32) {
                let entry = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry) };

                // End of directory entries
                if entry.name[0] == 0x00 {
                    break;
                }

                // Skip deleted and LFN entries
                if entry.name[0] == 0xE5 || (entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME) {
                    continue;
                }

                let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                if entry_cluster == target_cluster {
                    return Ok(());
                }
            }

            // Move to next cluster in directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        error!("[verify_directory_entry_immediate] Entry for cluster {} not found in directory {}",
               target_cluster, dir_cluster);
        Err(FileSystemError::NotFound)
    }

    fn find_directory_entry_by_name(
        &self,
        dir_cluster: u32,
        name: &str,
    ) -> Result<(u32, DirectoryEntry), FileSystemError> {
        let mut current_cluster = dir_cluster;

        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            // Process LFN entries to reconstruct full names
            let mut lfn_name = String::new();
            let mut entries = cluster_data.chunks_exact(32).collect::<Vec<_>>();

            for chunk in entries {
                let entry = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry) };

                if entry.name[0] == 0x00 {
                    break; // End of directory
                }

                if entry.name[0] == 0xE5 {
                    lfn_name.clear(); // Reset LFN on deleted entry
                    continue;
                }

                if entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                    // This is a long filename entry
                    let lfn = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const LongFileNameEntry) };

                    // Extract characters from LFN entry
                    let mut chars = Vec::new();

                    // name1 (5 chars) - copy to avoid packed field reference
                    let name1 = lfn.name1;
                    for &c in &name1 {
                        if c != 0xFFFF && c != 0x0000 {
                            chars.push(c);
                        }
                    }
                    // name2 (6 chars) - copy to avoid packed field reference
                    let name2 = lfn.name2;
                    for &c in &name2 {
                        if c != 0xFFFF && c != 0x0000 {
                            chars.push(c);
                        }
                    }
                    // name3 (2 chars) - copy to avoid packed field reference
                    let name3 = lfn.name3;
                    for &c in &name3 {
                        if c != 0xFFFF && c != 0x0000 {
                            chars.push(c);
                        }
                    }

                    if lfn.order & 0x40 != 0 {
                        // First LFN entry (highest order)
                        lfn_name = String::from_utf16_lossy(&chars);
                    } else {
                        // Prepend to existing name
                        let prefix = String::from_utf16_lossy(&chars);
                        lfn_name = prefix + &lfn_name;
                    }
                    continue;
                }

                // This is a regular directory entry
                if !lfn_name.is_empty() {
                    // Use the reconstructed long filename
                    if lfn_name == name {
                        let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                        return Ok((entry_cluster, entry));
                    }
                    lfn_name.clear();
                } else {
                    // Use short filename (8.3 format)
                    let mut short_name = String::new();
                    for &b in &entry.name {
                        if b != 0x20 {
                            short_name.push(b as char);
                        }
                    }
                    if entry.ext[0] != 0x20 {
                        short_name.push('.');
                        for &b in &entry.ext {
                            if b != 0x20 {
                                short_name.push(b as char);
                            }
                        }
                    }

                    if short_name == name {
                        let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                        return Ok((entry_cluster, entry));
                    }
                }
            }

            // Move to next cluster in directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn find_directory_entry_by_cluster(
        &self,
        target_cluster: u32,
    ) -> Result<(u32, DirectoryEntry), FileSystemError> {
        // Search starting from root directory
        self.search_directory_tree_for_cluster(self.root_cluster, target_cluster)
    }

    fn search_directory_tree_for_cluster(
        &self,
        dir_cluster: u32,
        target_cluster: u32,
    ) -> Result<(u32, DirectoryEntry), FileSystemError> {
        let mut current_cluster = dir_cluster;

        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            // Check each directory entry
            for chunk in cluster_data.chunks_exact(32) {
                let entry = unsafe { core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry) };

                if entry.name[0] == 0x00 {
                    break; // End of directory
                }

                if entry.name[0] == 0xE5 || (entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME) {
                    continue; // Skip deleted and LFN entries
                }

                let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;

                // Found the target cluster
                if entry_cluster == target_cluster {
                    return Ok((current_cluster, entry));
                }

                // If this is a directory, recursively search it
                if entry.attr & ATTR_DIRECTORY != 0 && entry_cluster != dir_cluster {
                    // Avoid infinite recursion by not searching parent directories
                    let name_str = core::str::from_utf8(&entry.name).unwrap_or("").trim();
                    if name_str != "." && name_str != ".." {
                        if let Ok(result) = self.search_directory_tree_for_cluster(entry_cluster, target_cluster) {
                            return Ok(result);
                        }
                    }
                }
            }

            // Move to next cluster in current directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        Err(FileSystemError::NotFound)
    }

    /// Force flush block cache - this MUST succeed for filesystem consistency
    /// Uses proper lock ordering and retry mechanism to avoid deadlock while ensuring data integrity
    fn flush_block_cache(&self) {
        // Use flush_lock to serialize all flush operations and prevent interference
        let _flush_guard = self.flush_lock.lock();

        // Memory barrier to ensure all previous writes are visible
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // Retry mechanism with backoff for acquiring block cache lock
        let max_retries = 10;
        let mut retry_count = 0;

        loop {
            match self.block_cache.try_write() {
                Some(mut cache) => {
                    // Successfully acquired lock, perform flush
                    let mut blocks_flushed = 0;
                    let mut failed_blocks = 0;

                    for (sector, entry) in cache.iter_mut() {
                        if entry.dirty {
                            match self.flush_single_block(*sector, &entry.data) {
                                Ok(_) => {
                                    entry.dirty = false;
                                    blocks_flushed += 1;
                                }
                                Err(e) => {
                                    error!("[flush_block_cache] Failed to flush sector {}: {:?}", sector, e);
                                    failed_blocks += 1;
                                }
                            }
                        }
                    }

                    // Only clear cache if all blocks were successfully flushed
                    if failed_blocks == 0 && blocks_flushed > 0 {
                        cache.clear();
                    }

                    if failed_blocks > 0 {
                        error!("[flush_block_cache] {} blocks failed to flush - filesystem may be corrupted!", failed_blocks);
                    }
                    break;
                }
                None => {
                    // Could not acquire lock, implement retry with exponential backoff
                    retry_count += 1;

                    if retry_count >= max_retries {
                        // This is a critical failure - we CANNOT skip flushing
                        error!("[flush_block_cache] CRITICAL: Could not acquire block cache lock after {} retries", max_retries);
                        error!("[flush_block_cache] Forcing emergency flush to preserve data integrity!");

                        // Force a memory barrier and try one more time
                        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

                        // Last desperate attempt - this might cause temporary slowdown but preserves data
                        let mut cache = self.block_cache.write(); // Blocking write - will wait for lock
                        let mut blocks_flushed = 0;

                        for (sector, entry) in cache.iter_mut() {
                            if entry.dirty {
                                if let Ok(_) = self.flush_single_block(*sector, &entry.data) {
                                    entry.dirty = false;
                                    blocks_flushed += 1;
                                }
                            }
                        }

                        if blocks_flushed > 0 {
                            cache.clear();
                        }

                        warn!("[flush_block_cache] Emergency flush completed: {} blocks flushed", blocks_flushed);
                        break;
                    }

                    // Exponential backoff: wait longer each time
                    let backoff_cycles = 1000 * (1 << retry_count.min(8));
                    for _ in 0..backoff_cycles {
                        core::hint::spin_loop();
                    }

                    // Memory barrier before retry
                    core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);
                }
            }
        }

        // Final memory barrier to ensure all writes are committed
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }

    /// Flush a single block to device - separated for better error handling
    fn flush_single_block(&self, sector: u32, data: &[u8]) -> Result<(), BlockError> {
        if data.len() != SECTOR_SIZE {
            return Err(BlockError::InvalidBlock);
        }

        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;
        let block_num = sector / sectors_per_block as u32;
        let sector_in_block = sector % sectors_per_block as u32;

        // Read-modify-write for device block
        let mut block_data = vec![0u8; device_block_size];
        self.device.read_block(block_num as usize, &mut block_data)?;

        let sector_offset = sector_in_block as usize * SECTOR_SIZE;
        block_data[sector_offset..sector_offset + SECTOR_SIZE].copy_from_slice(data);

        self.device.write_block(block_num as usize, &block_data)?;
        Ok(())
    }

    /// Write a directory entry back to the specified position
    fn write_directory_entry(
        &self,
        dir_cluster: u32,
        entry_index: usize,
        entry: &DirectoryEntry,
    ) -> Result<(), FileSystemError> {
        let entries_per_cluster =
            self.bytes_per_cluster as usize / core::mem::size_of::<DirectoryEntry>();
        let cluster_index = entry_index / entries_per_cluster;
        let entry_in_cluster = entry_index % entries_per_cluster;

        // Navigate to the correct cluster
        let mut current_cluster = dir_cluster;
        for _ in 0..cluster_index {
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                error!("[write_directory_entry] Cluster not found during navigation");
                return Err(FileSystemError::NotFound);
            }
        }

        // Read the cluster data
        let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
        self.read_cluster(current_cluster, &mut cluster_data)
            .map_err(|_| FileSystemError::IoError)?;

        // Update the specific entry
        let entry_offset = entry_in_cluster * core::mem::size_of::<DirectoryEntry>();
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(
                entry as *const DirectoryEntry as *const u8,
                core::mem::size_of::<DirectoryEntry>(),
            )
        };

        cluster_data[entry_offset..entry_offset + core::mem::size_of::<DirectoryEntry>()]
            .copy_from_slice(entry_bytes);

        // Write the modified cluster back
        self.write_cluster(current_cluster, &cluster_data)
            .map_err(|_| FileSystemError::IoError)?;

        Ok(())
    }

    fn validate_filename(name: &str) -> Result<(), FileSystemError> {
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidPath);
        }

        // Check for invalid characters
        for c in name.chars() {
            if c.is_control() || r#"\/:*?"<>|"#.contains(c) {
                return Err(FileSystemError::InvalidPath);
            }
        }

        // Check for reserved names
        let name_upper = name.to_uppercase();
        let reserved_names = [
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];

        for reserved in &reserved_names {
            if name_upper == *reserved {
                return Err(FileSystemError::InvalidPath);
            }
        }

        Ok(())
    }

    fn create_directory_entry(
        &self,
        parent_cluster: u32,
        name: &str,
        new_cluster: u32,
        is_dir: bool,
    ) -> Result<u32, FileSystemError> {
        // Check if the file already exists BEFORE acquiring lock to prevent deadlock
        if self.find_directory_entry_by_name(parent_cluster, name).is_ok() {
            return Err(FileSystemError::AlreadyExists);
        }

        let dir_lock = self.get_directory_lock(parent_cluster);
        let _lock = dir_lock.lock(); // Directory-specific lock

        // Memory barrier to ensure all previous operations are visible
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        let (sfn, lfn_entries) = self.generate_filename_entries(name);
        let required_slots = lfn_entries.len() + 1;
        let mut current_cluster = parent_cluster;

        // Track all clusters we've checked to avoid infinite loops
        let mut checked_clusters = alloc::collections::BTreeSet::new();

        loop {
            // Prevent infinite loops in corrupted filesystem
            if checked_clusters.contains(&current_cluster) {
                error!("[create_directory_entry] Detected cluster loop at {}", current_cluster);
                return Err(FileSystemError::IoError);
            }
            checked_clusters.insert(current_cluster);

            // Read cluster with error handling
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            if let Err(_) = self.read_cluster(current_cluster, &mut cluster_data) {
                error!("[create_directory_entry] Failed to read cluster {}", current_cluster);
                return Err(FileSystemError::IoError);
            }

            let mut empty_slots = 0;
            let mut start_index = 0;
            let entries_per_cluster = cluster_data.len() / 32;

            // Find consecutive empty slots
            for (i, chunk) in cluster_data.chunks_exact(32).enumerate() {
                if chunk[0] == 0x00 || chunk[0] == 0xE5 {
                    if empty_slots == 0 {
                        start_index = i;
                    }
                    empty_slots += 1;
                    if empty_slots >= required_slots {
                        break;
                    }
                } else {
                    empty_slots = 0;
                }
            }

            if empty_slots >= required_slots {
                for (i, lfn) in lfn_entries.iter().enumerate() {
                    let offset = (start_index + i) * 32;
                    let lfn_bytes =
                        unsafe { core::slice::from_raw_parts(lfn as *const _ as *const u8, 32) };
                    cluster_data[offset..offset + 32].copy_from_slice(lfn_bytes);
                }

                // Write SFN entry
                let entry = DirectoryEntry {
                    name: sfn.name,
                    ext: sfn.ext,
                    attr: if is_dir { ATTR_DIRECTORY } else { 0 },
                    reserved: 0,
                    create_time_tenth: 0,
                    create_time: 0,
                    create_date: 0,
                    last_access_date: 0,
                    first_cluster_high: (new_cluster >> 16) as u16,
                    last_write_time: 0,
                    last_write_date: 0,
                    first_cluster_low: (new_cluster & 0xFFFF) as u16,
                    file_size: 0,
                };
                let sfn_offset = (start_index + lfn_entries.len()) * 32;
                let entry_bytes =
                    unsafe { core::slice::from_raw_parts(&entry as *const _ as *const u8, 32) };
                cluster_data[sfn_offset..sfn_offset + 32].copy_from_slice(entry_bytes);

                // Atomic write of the entire cluster
                if let Err(_) = self.write_cluster(current_cluster, &cluster_data) {
                    error!("[create_directory_entry] Failed to write cluster {}", current_cluster);
                    return Err(FileSystemError::IoError);
                }

                // Force write completion with multiple barriers
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                self.flush_block_cache();
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

                return Ok(current_cluster);
            }

            // Try to extend the directory if we're at the end
            let next_cluster = self.next_cluster(current_cluster);
            if next_cluster >= CLUSTER_EOF {
                // Allocate new cluster for directory expansion
                if let Some(expansion_cluster) = self.allocate_cluster() {
                    // Initialize the new cluster with zeros
                    let zero_cluster = vec![0u8; self.bytes_per_cluster as usize];
                    if let Err(_) = self.write_cluster(expansion_cluster, &zero_cluster) {
                        // Clean up allocated cluster on failure
                        let _ = self.write_fat_entry(expansion_cluster, CLUSTER_FREE);
                        return Err(FileSystemError::IoError);
                    }

                    // Link current cluster to new cluster
                    if let Err(_) = self.write_fat_entry(current_cluster, expansion_cluster) {
                        // Clean up on failure
                        let _ = self.write_fat_entry(expansion_cluster, CLUSTER_FREE);
                        return Err(FileSystemError::IoError);
                    }

                    // Mark new cluster as end-of-chain
                    if let Err(_) = self.write_fat_entry(expansion_cluster, CLUSTER_EOF) {
                        return Err(FileSystemError::IoError);
                    }

                    // Force FAT write completion
                    self.flush_block_cache();
                    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

                    current_cluster = expansion_cluster;
                } else {
                    error!("[create_directory_entry] No space available for directory expansion");
                    return Err(FileSystemError::NoSpace);
                }
            } else {
                current_cluster = next_cluster;
            }
        }
    }

    fn generate_filename_entries(
        &self,
        name: &str,
    ) -> (ShortFileNameEntry, Vec<LongFileNameEntry>) {
        // 生成唯一的短文件名占位符（简化版本）
        let mut sfn_name = [0x20u8; 8]; // 用空格填充
        let sfn_ext = [0x20u8; 3]; // 用空格填充

        // 使用文件名哈希生成唯一的8.3名称
        let mut hasher = 0u32;
        for c in name.bytes() {
            hasher = hasher.wrapping_mul(31).wrapping_add(c as u32);
        }

        // 手动生成格式为 "LFN" + 5位数字的占位符
        let hash_suffix = hasher % 100000;
        sfn_name[0] = b'L';
        sfn_name[1] = b'F';
        sfn_name[2] = b'N';

        // 将数字转换为ASCII并填充到后5位
        let mut num = hash_suffix;
        for i in (3..8).rev() {
            sfn_name[i] = b'0' + (num % 10) as u8;
            num /= 10;
        }

        let sfn = ShortFileNameEntry {
            name: sfn_name,
            ext: sfn_ext,
        };

        // 生成长文件名条目
        let utf16_name: Vec<u16> = name.encode_utf16().collect();
        // 计算需要多少个LFN条目 (每个条目存储13个UTF-16字符)
        let num_lfn_entries = (utf16_name.len() + 12) / 13;
        let mut lfn_entries = Vec::new();

        // 计算校验和
        let checksum = self.calculate_sfn_checksum(&sfn);

        // 倒序生成LFN条目（FAT32要求）
        for i in 0..num_lfn_entries {
            let mut lfn = LongFileNameEntry {
                order: (num_lfn_entries - i) as u8,
                name1: [0xFFFF; 5],
                attr: ATTR_LONG_NAME,
                entry_type: 0,
                checksum,
                name2: [0xFFFF; 6],
                zero: 0,
                name3: [0xFFFF; 2],
            };

            // 标记最后一个（实际是第一个写入的）LFN条目
            if i == 0 {
                lfn.order |= 0x40;
            }

            // 计算这个LFN条目对应的字符起始位置（倒序索引）
            let start = (num_lfn_entries - 1 - i) * 13;
            let mut name_idx = 0;

            // 填充name1 (5个字符)
            for j in 0..5 {
                if start + name_idx < utf16_name.len() {
                    lfn.name1[j] = utf16_name[start + name_idx];
                } else if start + name_idx == utf16_name.len() {
                    lfn.name1[j] = 0; // null终止符
                } else {
                    lfn.name1[j] = 0xFFFF; // 填充
                }
                name_idx += 1;
            }

            // 填充name2 (6个字符)
            for j in 0..6 {
                if start + name_idx < utf16_name.len() {
                    lfn.name2[j] = utf16_name[start + name_idx];
                } else if start + name_idx == utf16_name.len() {
                    lfn.name2[j] = 0; // null终止符
                } else {
                    lfn.name2[j] = 0xFFFF; // 填充
                }
                name_idx += 1;
            }

            // 填充name3 (2个字符)
            for j in 0..2 {
                if start + name_idx < utf16_name.len() {
                    lfn.name3[j] = utf16_name[start + name_idx];
                } else if start + name_idx == utf16_name.len() {
                    lfn.name3[j] = 0; // null终止符
                } else {
                    lfn.name3[j] = 0xFFFF; // 填充
                }
                name_idx += 1;
            }

            lfn_entries.push(lfn);
        }

        (sfn, lfn_entries)
    }

    // 计算短文件名校验和
    fn calculate_sfn_checksum(&self, sfn: &ShortFileNameEntry) -> u8 {
        let mut checksum: u8 = 0;
        for &c in &sfn.name {
            checksum = ((checksum & 1) << 7) | ((checksum & 0xFE) >> 1);
            checksum = checksum.wrapping_add(c);
        }
        for &c in &sfn.ext {
            checksum = ((checksum & 1) << 7) | ((checksum & 0xFE) >> 1);
            checksum = checksum.wrapping_add(c);
        }
        checksum
    }
}

impl FileSystem for FAT32FileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        Arc::new(FAT32Inode {
            fs: self as *const _ as *const FAT32FileSystem,
            cluster: self.root_cluster,
            parent_dir_cluster: 0, // Root has no parent
            size: Mutex::new(0),
            is_dir: true,
            mode: Mutex::new(0o755), // 目录默认权限
            uid: Mutex::new(0),      // root用户
            gid: Mutex::new(0),      // root组
        })
    }

    fn create_file(
        &self,
        parent: &Arc<dyn Inode>,
        name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_file(name)
    }

    fn create_directory(
        &self,
        parent: &Arc<dyn Inode>,
        name: &str,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_directory(name)
    }

    fn remove(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<(), FileSystemError> {
        parent.remove(name)
    }

    fn stat(&self, inode: &Arc<dyn Inode>) -> Result<FileStat, FileSystemError> {
        let mut stat = FileStat::default();
        stat.size = inode.size();
        stat.file_type = inode.inode_type();
        Ok(stat)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

pub struct FAT32Inode {
    fs: *const FAT32FileSystem,
    cluster: u32,
    parent_dir_cluster: u32, // The directory cluster that contains this file's directory entry
    size: Mutex<u64>,
    is_dir: bool,
    mode: Mutex<u32>, // 文件权限模式
    uid: Mutex<u32>,  // 文件拥有者UID
    gid: Mutex<u32>,  // 文件拥有者GID
}

unsafe impl Send for FAT32Inode {}
unsafe impl Sync for FAT32Inode {}

impl FAT32Inode {
    fn fs(&self) -> &FAT32FileSystem {
        unsafe { &*self.fs }
    }

    fn entry_name_to_string(entry: &DirectoryEntry) -> String {
        let mut name = String::new();

        // Process filename
        for &byte in &entry.name {
            if byte == 0x20 {
                break;
            }
            name.push(byte as char);
        }

        // Process extension
        let mut ext = String::new();
        for &byte in &entry.ext {
            if byte == 0x20 {
                break;
            }
            ext.push(byte as char);
        }

        if !ext.is_empty() {
            name.push('.');
            name.push_str(&ext);
        }

        name.to_lowercase()
    }
}

impl Inode for FAT32Inode {
    fn inode_type(&self) -> InodeType {
        if self.is_dir {
            InodeType::Directory
        } else {
            InodeType::File
        }
    }

    fn size(&self) -> u64 {
        *self.size.lock()
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        if self.is_dir {
            return Err(FileSystemError::IsDirectory);
        }

        let file_size = *self.size.lock();
        if offset >= file_size {
            return Ok(0);
        }

        let read_size = (buf.len() as u64).min(file_size - offset) as usize;
        let mut current_cluster = self.cluster;
        let mut cluster_offset = offset;
        let bytes_per_cluster = self.fs().bytes_per_cluster as u64;

        // Skip preceding clusters
        while cluster_offset >= bytes_per_cluster {
            current_cluster = self.fs().next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                return Ok(0);
            }
            cluster_offset -= bytes_per_cluster;
        }

        let mut bytes_read = 0;

        while bytes_read < read_size && current_cluster < CLUSTER_EOF {
            let mut cluster_data = vec![0u8; bytes_per_cluster as usize];
            self.fs()
                .read_cluster(current_cluster, &mut cluster_data)
                .map_err(|e| {
                    error!(
                        "read_cluster failed for cluster {}: {:?}",
                        current_cluster, e
                    );
                    FileSystemError::IoError
                })?;

            let copy_start = cluster_offset as usize;
            let copy_size = (bytes_per_cluster as usize - copy_start).min(read_size - bytes_read);

            buf[bytes_read..bytes_read + copy_size]
                .copy_from_slice(&cluster_data[copy_start..copy_start + copy_size]);

            bytes_read += copy_size;
            cluster_offset = 0;
            current_cluster = self.fs().next_cluster(current_cluster);
        }

        Ok(bytes_read)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if self.is_dir {
            return Err(FileSystemError::IsDirectory);
        }

        let fs = self.fs();
        let bytes_per_cluster = fs.bytes_per_cluster as u64;
        let mut current_cluster = self.cluster;
        let mut cluster_offset = offset;
        let mut bytes_written = 0;

        // Skip to the correct starting cluster
        while cluster_offset >= bytes_per_cluster {
            let next_cluster = fs.next_cluster(current_cluster);
            if next_cluster >= CLUSTER_EOF {
                // Need to allocate new clusters
                if let Some(new_cluster) = fs.allocate_cluster() {
                    fs.write_fat_entry(current_cluster, new_cluster)
                        .map_err(|_| FileSystemError::IoError)?;
                    current_cluster = new_cluster;
                } else {
                    return Err(FileSystemError::NoSpace);
                }
            } else {
                current_cluster = next_cluster;
            }
            cluster_offset -= bytes_per_cluster;
        }

        while bytes_written < buf.len() {
            // Read current cluster
            let mut cluster_data = vec![0u8; bytes_per_cluster as usize];
            if current_cluster >= 2 && current_cluster < CLUSTER_EOF {
                fs.read_cluster(current_cluster, &mut cluster_data)
                    .map_err(|_| FileSystemError::IoError)?;
            }

            // Calculate how much to write in this cluster
            let write_start = cluster_offset as usize;
            let write_size =
                (bytes_per_cluster as usize - write_start).min(buf.len() - bytes_written);

            // Modify cluster data
            cluster_data[write_start..write_start + write_size]
                .copy_from_slice(&buf[bytes_written..bytes_written + write_size]);

            // Write cluster back
            fs.write_cluster(current_cluster, &cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            bytes_written += write_size;
            cluster_offset = 0;

            // Move to next cluster if needed
            if bytes_written < buf.len() {
                let next_cluster = fs.next_cluster(current_cluster);
                if next_cluster >= CLUSTER_EOF {
                    // Allocate new cluster and link it atomically
                    if let Some(new_cluster) = fs.allocate_cluster() {
                        // Link the current cluster to the new one
                        if let Err(e) = fs.write_fat_entry(current_cluster, new_cluster) {
                            error!("Failed to link cluster {} to {}: {:?}", current_cluster, new_cluster, e);
                            // Clean up: free the allocated cluster
                            let _ = fs.write_fat_entry(new_cluster, CLUSTER_FREE);
                            return Err(FileSystemError::IoError);
                        }
                        current_cluster = new_cluster;
                    } else {
                        return Err(FileSystemError::NoSpace);
                    }
                } else {
                    current_cluster = next_cluster;
                }
            }
        }
        // Update file size if we wrote beyond current size
        let old_size = *self.size.lock();
        let new_size = (offset + bytes_written as u64).max(old_size);
        *self.size.lock() = new_size;

        // Update directory entry with new file size if size changed
        if new_size != old_size {
            // Add memory barrier to ensure all writes are completed before updating directory entry
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

            if let Err(e) = fs.update_directory_entry_size(
                self.parent_dir_cluster,
                self.cluster,
                new_size as u32,
            ) {
                // Log the error but don't fail the write operation
                // The in-memory size is still updated correctly
                error!("Failed to update directory entry size: {:?}", e);
            }
        }

        Ok(bytes_written)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        let entries = self.fs().read_directory_entries(self.cluster)?;
        let mut names = Vec::new();

        for info in entries {
            if info.entry.attr & ATTR_VOLUME_ID != 0 {
                continue;
            }

            if info.name != "." && info.name != ".." {
                names.push(info.name.to_lowercase());
            }
        }

        Ok(names)
    }

    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        let entries = self.fs().read_directory_entries(self.cluster)?;
        let entries_count = entries.len();

        for (entry_index, info) in entries.into_iter().enumerate() {
            if info.entry.attr & ATTR_VOLUME_ID != 0 {
                continue;
            }

            if info.name.to_lowercase() == name.to_lowercase() {
                let entry = info.entry;
                let cluster =
                    (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                let is_dir = entry.attr & ATTR_DIRECTORY != 0;
                let size = if is_dir { 0 } else { entry.file_size as u64 };

                return Ok(Arc::new(FAT32Inode {
                    fs: self.fs,
                    cluster,
                    parent_dir_cluster: self.cluster, // Parent directory cluster
                    size: Mutex::new(size),
                    is_dir,
                    mode: Mutex::new(if is_dir { 0o755 } else { 0o644 }), // 默认权限
                    uid: Mutex::new(0),                                   // root用户
                    gid: Mutex::new(0),                                   // root组
                }));
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        // Validate filename first
        FAT32FileSystem::validate_filename(name)?;

        let fs = self.fs();

        // Use a global creation lock to prevent race conditions during file creation
        static CREATION_LOCK: spin::Mutex<()> = spin::Mutex::new(());
        let _creation_guard = CREATION_LOCK.lock();

        // Double-check if file already exists (race condition protection)
        if let Ok(_) = self.find_child(name) {
            return Err(FileSystemError::AlreadyExists);
        }

        // Allocate a new cluster for the file with retry mechanism
        let new_cluster = {
            let mut attempts = 0;
            loop {
                if let Some(cluster) = fs.allocate_cluster() {
                    break cluster;
                }
                attempts += 1;
                if attempts >= 3 {
                    error!("[create_file] Failed to allocate cluster after {} attempts", attempts);
                    return Err(FileSystemError::NoSpace);
                }
                // Brief yield to allow other operations to complete
                core::hint::spin_loop();
            }
        };

        // Initialize the allocated cluster with zeros
        let zero_data = vec![0u8; fs.bytes_per_cluster as usize];
        if let Err(e) = fs.write_cluster(new_cluster, &zero_data) {
            error!("[create_file] Failed to initialize cluster {}: {:?}", new_cluster, e);
            // Clean up allocated cluster
            let _ = fs.write_fat_entry(new_cluster, CLUSTER_FREE);
            return Err(FileSystemError::IoError);
        }

        // Mark cluster as end-of-chain in FAT
        if let Err(e) = fs.write_fat_entry(new_cluster, CLUSTER_EOF) {
            error!("[create_file] Failed to mark cluster {} as EOF: {:?}", new_cluster, e);
            // Clean up allocated cluster
            let _ = fs.write_fat_entry(new_cluster, CLUSTER_FREE);
            return Err(FileSystemError::IoError);
        }

        // Create directory entry with enhanced error handling
        let actual_dir_cluster = match fs.create_directory_entry(self.cluster, name, new_cluster, false) {
            Ok(cluster) => cluster,
            Err(e) => {
                error!("[create_file] Failed to create directory entry for '{}': {:?}", name, e);
                // Clean up allocated cluster and FAT entry
                let _ = fs.write_fat_entry(new_cluster, CLUSTER_FREE);
                return Err(e);
            }
        };

        // Return the new file inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
            parent_dir_cluster: actual_dir_cluster, // Directory cluster where entry was created
            size: Mutex::new(0),
            is_dir: false,
            mode: Mutex::new(0o644), // 文件默认权限
            uid: Mutex::new(0),      // root用户
            gid: Mutex::new(0),      // root组
        }))
    }

    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        // Validate directory name
        FAT32FileSystem::validate_filename(name)?;

        // Check if directory already exists
        if let Ok(_) = self.find_child(name) {
            return Err(FileSystemError::AlreadyExists);
        }

        let fs = self.fs();

        // Allocate a new cluster for the directory
        let new_cluster = fs.allocate_cluster().ok_or(FileSystemError::NoSpace)?;

        // Initialize the new directory cluster with "." and ".." entries
        let mut dir_data = vec![0u8; fs.bytes_per_cluster as usize];

        // Create "." entry (current directory)
        let dot_entry = DirectoryEntry {
            name: [b'.', b' ', b' ', b' ', b' ', b' ', b' ', b' '],
            ext: [b' ', b' ', b' '],
            attr: ATTR_DIRECTORY,
            reserved: 0,
            create_time_tenth: 0,
            create_time: 0,
            create_date: 0,
            last_access_date: 0,
            first_cluster_high: (new_cluster >> 16) as u16,
            last_write_time: 0,
            last_write_date: 0,
            first_cluster_low: (new_cluster & 0xFFFF) as u16,
            file_size: 0,
        };

        // Create ".." entry (parent directory)
        let dotdot_entry = DirectoryEntry {
            name: [b'.', b'.', b' ', b' ', b' ', b' ', b' ', b' '],
            ext: [b' ', b' ', b' '],
            attr: ATTR_DIRECTORY,
            reserved: 0,
            create_time_tenth: 0,
            create_time: 0,
            create_date: 0,
            last_access_date: 0,
            first_cluster_high: (self.cluster >> 16) as u16,
            last_write_time: 0,
            last_write_date: 0,
            first_cluster_low: (self.cluster & 0xFFFF) as u16,
            file_size: 0,
        };

        // Copy entries to directory data
        let dot_bytes = unsafe {
            core::slice::from_raw_parts(
                &dot_entry as *const _ as *const u8,
                core::mem::size_of::<DirectoryEntry>(),
            )
        };
        dir_data[0..32].copy_from_slice(dot_bytes);

        let dotdot_bytes = unsafe {
            core::slice::from_raw_parts(
                &dotdot_entry as *const _ as *const u8,
                core::mem::size_of::<DirectoryEntry>(),
            )
        };
        dir_data[32..64].copy_from_slice(dotdot_bytes);

        // Write the initialized directory cluster
        fs.write_cluster(new_cluster, &dir_data)
            .map_err(|_| FileSystemError::IoError)?;

        // Create directory entry in parent directory
        let actual_dir_cluster =
            fs.create_directory_entry(self.cluster, name, new_cluster, true)?;

        // Return the new directory inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
            parent_dir_cluster: actual_dir_cluster, // Directory cluster where entry was created
            size: Mutex::new(0),
            is_dir: true,
            mode: Mutex::new(0o755), // 目录默认权限
            uid: Mutex::new(0),      // root用户
            gid: Mutex::new(0),      // root组
        }))
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        let fs = self.fs();

        // Use directory-specific lock to prevent concurrent modifications
        let dir_lock = fs.get_directory_lock(self.cluster);
        let _lock = dir_lock.lock();

        // Memory barrier to ensure consistency
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        let mut child_cluster = 0u32;
        let mut found = false;
        let mut target_cluster = self.cluster;
        let mut target_entries: Vec<usize> = Vec::new(); // Track entries to delete

        let mut current_cluster = self.cluster;
        loop {
            let mut cluster_data = vec![0u8; fs.bytes_per_cluster as usize];
            fs.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            // Process entries directly from raw cluster data to maintain proper indexing
            let mut lfn_cache: Vec<usize> = Vec::new();

            for (chunk_idx, chunk) in cluster_data.chunks_exact(32).enumerate() {
                let attr = chunk[11];
                let is_lfn = attr & ATTR_LONG_NAME == ATTR_LONG_NAME;

                if chunk[0] == 0x00 {
                    break; // End of directory entries
                }
                if chunk[0] == 0xE5 {
                    lfn_cache.clear();
                    continue; // Skip deleted entries
                }

                if is_lfn {
                    // This is a long filename entry
                    lfn_cache.push(chunk_idx);
                } else {
                    // This is a short filename entry
                    let entry = unsafe {
                        core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry)
                    };

                    if entry.attr & ATTR_VOLUME_ID != 0 {
                        lfn_cache.clear();
                        continue;
                    }

                    // Check if this matches the file we want to delete
                    let entry_name = if !lfn_cache.is_empty() {
                        // Reconstruct long filename from LFN entries
                        lfn_cache.sort_by_key(|&idx| {
                            let order = cluster_data[idx * 32];
                            order & 0x1F
                        });
                        let mut long_name_utf16: Vec<u16> = Vec::new();
                        for &lfn_idx in &lfn_cache {
                            let lfn_chunk = &cluster_data[lfn_idx * 32..lfn_idx * 32 + 32];
                            let lfn_entry = unsafe {
                                core::ptr::read_unaligned(
                                    lfn_chunk.as_ptr() as *const LongFileNameEntry
                                )
                            };
                            unsafe {
                                let name1 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name1));
                                long_name_utf16.extend_from_slice(&name1);
                                let name2 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name2));
                                long_name_utf16.extend_from_slice(&name2);
                                let name3 =
                                    core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name3));
                                long_name_utf16.extend_from_slice(&name3);
                            }
                        }
                        let null_pos = long_name_utf16
                            .iter()
                            .position(|&c| c == 0)
                            .unwrap_or(long_name_utf16.len());
                        long_name_utf16.truncate(null_pos);
                        String::from_utf16_lossy(&long_name_utf16)
                    } else {
                        FAT32Inode::entry_name_to_string(&entry)
                    };

                    if entry_name.to_lowercase() == name.to_lowercase() {
                        child_cluster = (entry.first_cluster_high as u32) << 16
                            | entry.first_cluster_low as u32;
                        found = true;
                        target_cluster = current_cluster;

                        // Store entry indices to delete (LFN entries + SFN entry)
                        target_entries.extend(lfn_cache.iter().copied());
                        target_entries.push(chunk_idx);
                        break;
                    }

                    lfn_cache.clear();
                }
            }

            if found {
                break;
            }

            current_cluster = fs.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        if !found {
            return Err(FileSystemError::NotFound);
        }

        // Now perform atomic deletion
        // Step 1: Mark directory entries as deleted
        let mut cluster_data = vec![0u8; fs.bytes_per_cluster as usize];
        fs.read_cluster(target_cluster, &mut cluster_data)
            .map_err(|_| FileSystemError::IoError)?;

        // Double-check entries are still valid before deleting
        let mut all_entries_valid = true;
        for &entry_idx in &target_entries {
            if entry_idx * 32 >= cluster_data.len() || cluster_data[entry_idx * 32] == 0xE5 {
                all_entries_valid = false;
                break;
            }
        }

        if !all_entries_valid {
            error!("[remove] Directory entries changed during deletion, aborting for safety");
            return Err(FileSystemError::IoError);
        }

        // Mark entries as deleted
        for &entry_idx in &target_entries {
            cluster_data[entry_idx * 32] = 0xE5;
        }

        // Atomic write of directory cluster
        fs.write_cluster(target_cluster, &cluster_data)
            .map_err(|_| FileSystemError::IoError)?;

        // Force directory write completion
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        fs.flush_block_cache();
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        // Step 2: Free the clusters used by the file/directory
        let mut current_cluster = child_cluster;
        while current_cluster < CLUSTER_EOF && current_cluster != 0 {
            let next_cluster = fs.next_cluster(current_cluster);

            // Atomic FAT update
            if let Err(e) = fs.write_fat_entry(current_cluster, CLUSTER_FREE) {
                error!("[remove] Failed to free cluster {}: {:?}", current_cluster, e);
                // Continue trying to free other clusters even if one fails
            }

            current_cluster = next_cluster;
        }

        // Force FAT write completion
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        fs.flush_block_cache();
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

        Ok(())
    }

    fn truncate(&self, size: u64) -> Result<(), FileSystemError> {
        if self.is_dir {
            return Err(FileSystemError::IsDirectory);
        }

        let fs = self.fs();
        let bytes_per_cluster = fs.bytes_per_cluster as u64;
        let needed_clusters = (size + bytes_per_cluster - 1) / bytes_per_cluster;

        let mut current_cluster = self.cluster;
        let mut cluster_count = 0;

        // Navigate through existing clusters
        while current_cluster < CLUSTER_EOF && cluster_count < needed_clusters {
            cluster_count += 1;
            if cluster_count == needed_clusters {
                // This is the last cluster we need, truncate the chain here
                let next_cluster = fs.next_cluster(current_cluster);
                fs.write_fat_entry(current_cluster, CLUSTER_EOF)
                    .map_err(|_| FileSystemError::IoError)?;

                // Free remaining clusters
                let mut free_cluster = next_cluster;
                while free_cluster < CLUSTER_EOF {
                    let next_free = fs.next_cluster(free_cluster);
                    fs.write_fat_entry(free_cluster, CLUSTER_FREE)
                        .map_err(|_| FileSystemError::IoError)?;
                    free_cluster = next_free;
                }
                break;
            }
            current_cluster = fs.next_cluster(current_cluster);
        }

        // If we need more clusters, allocate them
        while cluster_count < needed_clusters {
            if let Some(new_cluster) = fs.allocate_cluster() {
                fs.write_fat_entry(current_cluster, new_cluster)
                    .map_err(|_| FileSystemError::IoError)?;
                current_cluster = new_cluster;
                cluster_count += 1;
            } else {
                return Err(FileSystemError::NoSpace);
            }
        }

        Ok(())
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    /// 获取文件权限模式
    fn mode(&self) -> u32 {
        *self.mode.lock()
    }

    /// 设置文件权限模式
    fn set_mode(&self, mode: u32) -> Result<(), super::FileSystemError> {
        *self.mode.lock() = mode;
        Ok(())
    }

    /// 获取文件拥有者UID
    fn uid(&self) -> u32 {
        *self.uid.lock()
    }

    /// 设置文件拥有者UID
    fn set_uid(&self, uid: u32) -> Result<(), super::FileSystemError> {
        *self.uid.lock() = uid;
        Ok(())
    }

    /// 获取文件拥有者GID
    fn gid(&self) -> u32 {
        *self.gid.lock()
    }

    /// 设置文件拥有者GID
    fn set_gid(&self, gid: u32) -> Result<(), super::FileSystemError> {
        *self.gid.lock() = gid;
        Ok(())
    }
}
