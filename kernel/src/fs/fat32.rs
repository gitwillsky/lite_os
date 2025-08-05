use alloc::{string::String, sync::Arc, vec, vec::Vec};
use spin::Mutex;

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

pub struct FAT32FileSystem {
    device: Arc<dyn BlockDevice>,
    bpb: BiosParameterBlock,
    fat_start_sector: u32,
    cluster_start_sector: u32,
    sectors_per_cluster: u32,
    bytes_per_cluster: u32,
    root_cluster: u32,
    fat_cache: Mutex<Vec<u32>>,
    directory_lock: Mutex<()>,  // Global lock for directory operations
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
            fat_cache: Mutex::new(fat_cache),
            directory_lock: Mutex::new(()),
        }))
    }

    fn cluster_to_sector(&self, cluster: u32) -> u32 {
        self.cluster_start_sector + (cluster - 2) * self.sectors_per_cluster
    }

    fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), BlockError> {
        if buf.len() < self.bytes_per_cluster as usize {
            return Err(BlockError::InvalidBlock);
        }

        let start_sector = self.cluster_to_sector(cluster);
        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;

        for i in 0..self.sectors_per_cluster {
            let sector_num = start_sector + i;
            let block_num = sector_num / sectors_per_block as u32;
            let sector_in_block = sector_num % sectors_per_block as u32;

            // Read the full device block
            let mut block_data = vec![0u8; device_block_size];
            self.device
                .read_block(block_num as usize, &mut block_data)?;

            // Extract the specific sector from the block
            let sector_offset_in_block = sector_in_block as usize * SECTOR_SIZE;
            let sector_offset_in_buf = i as usize * SECTOR_SIZE;

            buf[sector_offset_in_buf..sector_offset_in_buf + SECTOR_SIZE].copy_from_slice(
                &block_data[sector_offset_in_block..sector_offset_in_block + SECTOR_SIZE],
            );
        }

        Ok(())
    }

    fn write_cluster(&self, cluster: u32, buf: &[u8]) -> Result<(), BlockError> {
        if buf.len() < self.bytes_per_cluster as usize {
            return Err(BlockError::InvalidBlock);
        }

        let start_sector = self.cluster_to_sector(cluster);
        let device_block_size = self.device.block_size();
        let sectors_per_block = device_block_size / SECTOR_SIZE;

        for i in 0..self.sectors_per_cluster {
            let sector_num = start_sector + i;
            let block_num = sector_num / sectors_per_block as u32;
            let sector_in_block = sector_num % sectors_per_block as u32;

            // Read the full device block first (read-modify-write)
            let mut block_data = vec![0u8; device_block_size];
            self.device
                .read_block(block_num as usize, &mut block_data)?;

            // Modify the specific sector in the block
            let sector_offset_in_block = sector_in_block as usize * SECTOR_SIZE;
            let sector_offset_in_buf = i as usize * SECTOR_SIZE;

            block_data[sector_offset_in_block..sector_offset_in_block + SECTOR_SIZE]
                .copy_from_slice(&buf[sector_offset_in_buf..sector_offset_in_buf + SECTOR_SIZE]);

            // Write the modified block back
            self.device.write_block(block_num as usize, &block_data)?;
        }

        Ok(())
    }

    fn next_cluster(&self, cluster: u32) -> u32 {
        let fat_cache = self.fat_cache.lock();
        if cluster as usize >= fat_cache.len() {
            return CLUSTER_EOF;
        }
        fat_cache[cluster as usize] & 0x0FFFFFFF
    }

    fn allocate_cluster(&self) -> Option<u32> {
        let mut fat_cache = self.fat_cache.lock();
        // Start from cluster 2 (clusters 0 and 1 are reserved)
        for i in 2..fat_cache.len() {
            if fat_cache[i] == CLUSTER_FREE {
                fat_cache[i] = CLUSTER_EOF; // Mark as end of chain
                return Some(i as u32);
            }
        }
        warn!("No free clusters available");
        None
    }

    fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), BlockError> {
        let mut fat_cache = self.fat_cache.lock();
        if cluster as usize >= fat_cache.len() || cluster < 2 {
            error!("Invalid cluster number for FAT write: {}", cluster);
            return Err(BlockError::InvalidBlock);
        }

        fat_cache[cluster as usize] = value & 0x0FFFFFFF;

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

            let value_bytes = value.to_le_bytes();
            block_data[block_sector_offset..block_sector_offset + 4].copy_from_slice(&value_bytes);

            self.device.write_block(block_num as usize, &block_data)?;
        }

        Ok(())
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
                                let name1 = core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name1));
                                long_name_utf16.extend_from_slice(&name1);
                                let name2 = core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name2));
                                long_name_utf16.extend_from_slice(&name2);
                                let name3 = core::ptr::read_unaligned(core::ptr::addr_of!((*lfn).name3));
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

    /// Update the file size in the directory entry for a given cluster
    /// This searches through all directories to find the entry with the matching cluster
    fn update_directory_entry_size(&self, parent_dir_cluster: u32, target_cluster: u32, new_size: u32) -> Result<(), FileSystemError> {
        let _lock = self.directory_lock.lock();  // Ensure exclusive access to directory operations
        debug!("[update_directory_entry_size] Searching for cluster {} with new size {}", target_cluster, new_size);
        
        // Add memory barrier to ensure directory entry creation is visible across cores
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        
        // Retry logic for multi-core systems to handle timing issues
        for retry in 0..5 {
            if retry > 0 {
                debug!("[update_directory_entry_size] Retry {} for cluster {}", retry, target_cluster);
                // Small delay to allow other cores to complete directory operations
                for _ in 0..1000 { core::hint::spin_loop(); }
            }
            
            // Start search from the specific parent directory cluster
            match self.search_and_update_entry(parent_dir_cluster, target_cluster, new_size) {
                Ok(()) => return Ok(()),
                Err(FileSystemError::NotFound) if retry < 4 => {
                    debug!("[update_directory_entry_size] Entry not found on retry {}, will retry", retry);
                    continue;
                },
                Err(e) => return Err(e),
            }
        }
        
        // If all retries failed, return the error
        warn!("[update_directory_entry_size] Failed to find directory entry for cluster {} after 5 retries", target_cluster);
        Err(FileSystemError::NotFound)
    }

    /// Recursively search for a directory entry with the given cluster and update its size
    /// This function needs to track the actual disk position, not the filtered vector index
    fn search_and_update_entry(&self, dir_cluster: u32, target_cluster: u32, new_size: u32) -> Result<(), FileSystemError> {
        debug!("[search_and_update_entry] Searching in directory cluster {} for target cluster {}", dir_cluster, target_cluster);
        // We need to track the actual disk position, so we read directory data directly
        let mut current_cluster = dir_cluster;
        let mut global_entry_index = 0;
        let mut entries_found = 0;  // Track total entries found for debugging

        loop {
            // Read cluster data directly
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            let entries_per_cluster = self.bytes_per_cluster as usize / 32;
            let cluster_base_index = global_entry_index;

            // Process each 32-byte directory entry in this cluster
            for (local_index, chunk) in cluster_data.chunks_exact(32).enumerate() {
                let entry = unsafe {
                    core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry)
                };

                // End of directory entries
                if entry.name[0] == 0x00 {
                    debug!("[search_and_update_entry] End of directory entries reached (found {} total entries)", entries_found);
                    return Err(FileSystemError::NotFound);
                }

                // Skip deleted entries but still count them for indexing
                if entry.name[0] == 0xE5 {
                    debug!("[search_and_update_entry] Skipping deleted entry at local_index {}", local_index);
                    continue;
                }

                // Skip long filename entries but still count them for indexing
                if entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                    debug!("[search_and_update_entry] Skipping LFN entry at local_index {}", local_index);
                    continue;
                }

                entries_found += 1;
                let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                debug!("[search_and_update_entry] Found entry with cluster {}, target is {} (attr: 0x{:02x}, name: {:?})", 
                    entry_cluster, target_cluster, entry.attr, 
                    core::str::from_utf8(&entry.name[..8]).unwrap_or("invalid"));

                // If this is the target cluster (regardless of file vs directory), update its size
                if entry_cluster == target_cluster {
                    let is_directory = (entry.attr & ATTR_DIRECTORY) != 0;
                    debug!("[search_and_update_entry] Found target cluster {} (is_directory: {})", target_cluster, is_directory);
                    
                    // Only update size for regular files, not directories
                    if !is_directory {
                        let current_file_size = entry.file_size;
                        debug!("[search_and_update_entry] Found target file! Updating size from {} to {}", current_file_size, new_size);
                        // Create updated entry
                        let mut updated_entry = entry;
                        updated_entry.file_size = new_size;

                        // Update the entry directly in the current cluster data
                        let entry_bytes = unsafe {
                            core::slice::from_raw_parts(&updated_entry as *const DirectoryEntry as *const u8, core::mem::size_of::<DirectoryEntry>())
                        };
                        let entry_offset = local_index * 32;
                        cluster_data[entry_offset..entry_offset + 32].copy_from_slice(entry_bytes);

                        // Write the modified cluster back
                        self.write_cluster(current_cluster, &cluster_data)
                            .map_err(|_| FileSystemError::IoError)?;
                        
                        debug!("[search_and_update_entry] Successfully updated directory entry for cluster {}", target_cluster);
                        return Ok(());
                    } else {
                        debug!("[search_and_update_entry] Target cluster {} is a directory, cannot update file size", target_cluster);
                        return Err(FileSystemError::IsDirectory);
                    }
                }

                // If this is a subdirectory, recursively search it (but exclude . and .. directories)
                if (entry.attr & ATTR_DIRECTORY) != 0 && entry_cluster != 0 && entry_cluster != current_cluster {
                    // Skip . and .. entries
                    if !(entry.name[0] == b'.' && (entry.name[1] == b' ' || entry.name[1] == b'.')) {
                        debug!("[search_and_update_entry] Recursively searching subdirectory cluster {}", entry_cluster);
                        if let Ok(()) = self.search_and_update_entry(entry_cluster, target_cluster, new_size) {
                            return Ok(());
                        }
                    }
                }
            }

            // Update global entry index for the next cluster
            global_entry_index += entries_per_cluster;

            // Move to next cluster in the directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        debug!("[search_and_update_entry] Target cluster {} not found in directory cluster {} (searched {} entries)", target_cluster, dir_cluster, entries_found);
        Err(FileSystemError::NotFound)
    }

    /// Search for a directory entry with the given cluster (read-only verification)
    fn verify_directory_entry_exists(&self, dir_cluster: u32, target_cluster: u32) -> Result<bool, FileSystemError> {
        debug!("[verify_directory_entry_exists] Searching for cluster {} in directory cluster {}", target_cluster, dir_cluster);
        let mut current_cluster = dir_cluster;

        loop {
            // Read cluster data directly
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            // Process each 32-byte directory entry in this cluster
            for chunk in cluster_data.chunks_exact(32) {
                let entry = unsafe {
                    core::ptr::read_unaligned(chunk.as_ptr() as *const DirectoryEntry)
                };

                // End of directory entries
                if entry.name[0] == 0x00 {
                    debug!("[verify_directory_entry_exists] End of directory entries reached");
                    return Ok(false);
                }

                // Skip deleted entries
                if entry.name[0] == 0xE5 {
                    continue;
                }

                // Skip long filename entries
                if entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                    continue;
                }

                let entry_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                
                // If this is the target cluster, we found it
                if entry_cluster == target_cluster {
                    debug!("[verify_directory_entry_exists] Found cluster {} in directory", target_cluster);
                    return Ok(true);
                }
            }

            // Move to next cluster in the directory
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }

        debug!("[verify_directory_entry_exists] Cluster {} not found in directory {}", target_cluster, dir_cluster);
        Ok(false)
    }

    /// Write a directory entry back to the specified position
    fn write_directory_entry(&self, dir_cluster: u32, entry_index: usize, entry: &DirectoryEntry) -> Result<(), FileSystemError> {
        debug!("[write_directory_entry] Writing entry at index {} in cluster {}", entry_index, dir_cluster);
        let entries_per_cluster = self.bytes_per_cluster as usize / core::mem::size_of::<DirectoryEntry>();
        let cluster_index = entry_index / entries_per_cluster;
        let entry_in_cluster = entry_index % entries_per_cluster;

        // Navigate to the correct cluster
        let mut current_cluster = dir_cluster;
        for _ in 0..cluster_index {
            current_cluster = self.next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                debug!("[write_directory_entry] Cluster not found during navigation");
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
            core::slice::from_raw_parts(entry as *const DirectoryEntry as *const u8, core::mem::size_of::<DirectoryEntry>())
        };

        cluster_data[entry_offset..entry_offset + core::mem::size_of::<DirectoryEntry>()]
            .copy_from_slice(entry_bytes);

        // Write the modified cluster back
        self.write_cluster(current_cluster, &cluster_data)
            .map_err(|_| FileSystemError::IoError)?;

        debug!("[write_directory_entry] Successfully wrote directory entry");
        Ok(())
    }

    fn validate_filename(name: &str) -> Result<(), FileSystemError> {
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidPath);
        }

        // Check for invalid characters
        for c in name.chars() {
            if c.is_control() || "\\/:*?\"<>|".contains(c) {
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

    fn create_directory_entry(&self, parent_cluster: u32, name: &str, new_cluster: u32, is_dir: bool) -> Result<u32, FileSystemError> {
        let _lock = self.directory_lock.lock();  // Ensure exclusive access to directory operations
        let (sfn, lfn_entries) = self.generate_filename_entries(name);

        let required_slots = lfn_entries.len() + 1;
        let mut current_cluster = parent_cluster;

        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;

            let mut empty_slots = 0;
            let mut start_index = 0;

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
                // Write LFN entries
                for (i, lfn) in lfn_entries.iter().enumerate() {
                    let offset = (start_index + i) * 32;
                    let lfn_bytes = unsafe {
                        core::slice::from_raw_parts(lfn as *const _ as *const u8, 32)
                    };
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
                let offset = (start_index + lfn_entries.len()) * 32;
                let entry_bytes = unsafe {
                    core::slice::from_raw_parts(&entry as *const _ as *const u8, 32)
                };
                cluster_data[offset..offset + 32].copy_from_slice(entry_bytes);

                self.write_cluster(current_cluster, &cluster_data)
                    .map_err(|_| FileSystemError::IoError)?;
                
                // Add memory barrier to ensure directory entry is visible across cores
                core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
                
                // Force a cache flush to ensure directory entry is written to disk
                // This is critical for multi-core systems to avoid race conditions
                debug!("[create_directory_entry] Successfully created directory entry for cluster {} in directory cluster {}", new_cluster, current_cluster);
                return Ok(current_cluster);
            }

            let next_cluster = self.next_cluster(current_cluster);
            if next_cluster >= CLUSTER_EOF {
                // Allocate new cluster if needed
                if let Some(new_cluster) = self.allocate_cluster() {
                    self.write_fat_entry(current_cluster, new_cluster)
                        .map_err(|_| FileSystemError::IoError)?;
                    current_cluster = new_cluster;
                } else {
                    return Err(FileSystemError::NoSpace);
                }
            } else {
                current_cluster = next_cluster;
            }
        }
    }

    fn generate_filename_entries(&self, name: &str) -> (ShortFileNameEntry, Vec<LongFileNameEntry>) {
        // 生成唯一的短文件名占位符（简化版本）
        let mut sfn_name = [0x20u8; 8];  // 用空格填充
        let sfn_ext = [0x20u8; 3];   // 用空格填充

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

        let sfn = ShortFileNameEntry { name: sfn_name, ext: sfn_ext };

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
            parent_dir_cluster: 0,  // Root has no parent
            size: Mutex::new(0),
            is_dir: true,
            mode: Mutex::new(0o755),  // 目录默认权限
            uid: Mutex::new(0),       // root用户
            gid: Mutex::new(0),       // root组
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
    parent_dir_cluster: u32,  // The directory cluster that contains this file's directory entry
    size: Mutex<u64>,
    is_dir: bool,
    mode: Mutex<u32>,   // 文件权限模式
    uid: Mutex<u32>,    // 文件拥有者UID
    gid: Mutex<u32>,    // 文件拥有者GID
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
            debug!("read_at: offset({}) >= file_size({}), returning 0", offset, file_size);
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
                    debug!("read_cluster failed for cluster {}: {:?}", current_cluster, e);
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
            if current_cluster < CLUSTER_EOF {
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
                    // Allocate new cluster
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
            }
        }
        // Update file size if we wrote beyond current size
        let old_size = *self.size.lock();
        let new_size = (offset + bytes_written as u64).max(old_size);
        *self.size.lock() = new_size;

        // Update directory entry with new file size if size changed
        if new_size != old_size {
            debug!("[write_at] File size changed from {} to {}, updating directory entry for cluster {}", old_size, new_size, self.cluster);
            
            // Add memory barrier to ensure all writes are completed before updating directory entry
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            
            if let Err(e) = fs.update_directory_entry_size(self.parent_dir_cluster, self.cluster, new_size as u32) {
                // Log the error but don't fail the write operation
                // The in-memory size is still updated correctly
                error!("Failed to update directory entry size: {:?}", e);
            } else {
                debug!("[write_at] Successfully updated directory entry size for cluster {}", self.cluster);
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
                    parent_dir_cluster: self.cluster,  // Parent directory cluster
                    size: Mutex::new(size),
                    is_dir,
                    mode: Mutex::new(if is_dir { 0o755 } else { 0o644 }),  // 默认权限
                    uid: Mutex::new(0),       // root用户
                    gid: Mutex::new(0),       // root组
                }));
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        // Validate filename
        FAT32FileSystem::validate_filename(name)?;

        // Check if file already exists
        if let Ok(_) = self.find_child(name) {
            return Err(FileSystemError::AlreadyExists);
        }

        let fs = self.fs();

        // Allocate a new cluster for the file
        let new_cluster = fs.allocate_cluster().ok_or(FileSystemError::NoSpace)?;
        debug!("[create_file] Allocated cluster {} for new file '{}'", new_cluster, name);

        // Create file entry in parent directory
        let actual_dir_cluster = fs.create_directory_entry(self.cluster, name, new_cluster, false)?;
        debug!("[create_file] Created directory entry for file '{}' with cluster {} in directory cluster {}", name, new_cluster, actual_dir_cluster);

        // Verify that the directory entry was created successfully by searching for it
        // This ensures the entry is visible before we return the inode
        debug!("[create_file] Verifying directory entry exists for cluster {} in directory cluster {}", new_cluster, actual_dir_cluster);
        let verification_result = fs.verify_directory_entry_exists(actual_dir_cluster, new_cluster)?;
        if !verification_result {
            error!("[create_file] Directory entry not found after creation for cluster {} in directory cluster {}", new_cluster, actual_dir_cluster);
            // Try to clean up the allocated cluster
            let _ = fs.write_fat_entry(new_cluster, CLUSTER_FREE);
            return Err(FileSystemError::IoError);
        }
        debug!("[create_file] Directory entry verification successful for cluster {} in directory cluster {}", new_cluster, actual_dir_cluster);

        // Return the new file inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
            parent_dir_cluster: actual_dir_cluster,  // Directory cluster where entry was created
            size: Mutex::new(0),
            is_dir: false,
            mode: Mutex::new(0o644),  // 文件默认权限
            uid: Mutex::new(0),       // root用户
            gid: Mutex::new(0),       // root组
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
        let actual_dir_cluster = fs.create_directory_entry(self.cluster, name, new_cluster, true)?;

        // Return the new directory inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
            parent_dir_cluster: actual_dir_cluster,  // Directory cluster where entry was created
            size: Mutex::new(0),
            is_dir: true,
            mode: Mutex::new(0o755),  // 目录默认权限
            uid: Mutex::new(0),       // root用户
            gid: Mutex::new(0),       // root组
        }))
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }

        let fs = self.fs();
        let mut child_cluster = 0u32;
        let mut found = false;

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
                                core::ptr::read_unaligned(lfn_chunk.as_ptr() as *const LongFileNameEntry)
                            };
                            unsafe {
                                let name1 = core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name1));
                                long_name_utf16.extend_from_slice(&name1);
                                let name2 = core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name2));
                                long_name_utf16.extend_from_slice(&name2);
                                let name3 = core::ptr::read_unaligned(core::ptr::addr_of!(lfn_entry.name3));
                                long_name_utf16.extend_from_slice(&name3);
                            }
                        }
                        let null_pos = long_name_utf16.iter().position(|&c| c == 0).unwrap_or(long_name_utf16.len());
                        long_name_utf16.truncate(null_pos);
                        String::from_utf16_lossy(&long_name_utf16)
                    } else {
                        FAT32Inode::entry_name_to_string(&entry)
                    };

                    if entry_name.to_lowercase() == name.to_lowercase() {
                        child_cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                        found = true;

                        // Mark all related entries as deleted (LFN entries + SFN entry)
                        for &lfn_idx in &lfn_cache {
                            cluster_data[lfn_idx * 32] = 0xE5;
                        }
                        cluster_data[chunk_idx * 32] = 0xE5; // Mark SFN entry as deleted

                        fs.write_cluster(current_cluster, &cluster_data)
                            .map_err(|_| FileSystemError::IoError)?;
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

        // Free the clusters used by the file/directory
        let mut current_cluster = child_cluster;
        while current_cluster < CLUSTER_EOF && current_cluster != 0 {
            let next_cluster = fs.next_cluster(current_cluster);
            fs.write_fat_entry(current_cluster, CLUSTER_FREE)
                .map_err(|_| FileSystemError::IoError)?;
            current_cluster = next_cluster;
        }

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
