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
}

impl FAT32FileSystem {
    pub fn new(device: Arc<dyn BlockDevice>) -> Option<Arc<Self>> {
        debug!("[FAT32] Attempting to initialize FAT32 filesystem...");
        debug!("[FAT32] Device block size: {}", device.block_size());

        // Read full block (4096 bytes) to accommodate VirtIO block size
        let mut block_bytes = vec![0u8; device.block_size()];
        if let Err(e) = device.read_block(0, &mut block_bytes) {
            error!("[FAT32] Failed to read boot sector: {:?}", e);
            return None;
        }
        debug!("[FAT32] Successfully read boot sector");

        // Extract the 512-byte boot sector from the full block
        let mut bpb_bytes = [0u8; SECTOR_SIZE];
        bpb_bytes.copy_from_slice(&block_bytes[..SECTOR_SIZE]);

        // Debug: show first few bytes of boot sector
        debug!(
            "[FAT32] Boot sector first 16 bytes: {:02x?}",
            &bpb_bytes[..16]
        );

        let bpb =
            unsafe { core::ptr::read_unaligned(bpb_bytes.as_ptr() as *const BiosParameterBlock) };

        // Verify FAT32 filesystem
        let bpb_ptr = bpb_bytes.as_ptr();
        let signature = unsafe { core::ptr::read_unaligned(bpb_ptr.add(510) as *const u16) };
        debug!("[FAT32] Boot signature: {:#x}", signature);
        if signature != FAT32_SIGNATURE {
            error!(
                "[FAT32] Invalid boot signature: {:#x} (expected {:#x})",
                signature, FAT32_SIGNATURE
            );
            return None;
        }

        let sectors_per_fat_32 =
            unsafe { core::ptr::read_unaligned(bpb_ptr.add(36) as *const u32) };
        debug!("[FAT32] Sectors per FAT32: {}", sectors_per_fat_32);
        if sectors_per_fat_32 == 0 {
            error!("[FAT32] Not a FAT32 filesystem (sectors_per_fat_32 is 0)");
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

        info!("[FAT32] Filesystem initialized successfully");
        info!("[FAT32] Reserved sectors: {}", reserved_sectors);
        info!("[FAT32] Number of FATs: {}", num_fats);
        info!("[FAT32] Sectors per FAT: {}", sectors_per_fat_32);
        info!("[FAT32] Sectors per cluster: {}", sectors_per_cluster);
        info!("[FAT32] Bytes per cluster: {}", bytes_per_cluster);
        info!("[FAT32] Root directory cluster: {}", root_cluster);
        info!("[FAT32] Data area start sector: {}", cluster_start_sector);

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

        // Debug: Show first few FAT entries
        info!("[FAT32] FAT table loaded with {} entries", fat_cache.len());

        Some(Arc::new(FAT32FileSystem {
            device,
            bpb,
            fat_start_sector,
            cluster_start_sector,
            sectors_per_cluster,
            bytes_per_cluster,
            root_cluster,
            fat_cache: Mutex::new(fat_cache),
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
        debug!(
            "[FAT32] Looking for free cluster. FAT cache size: {}",
            fat_cache.len()
        );

        // Start from cluster 2 (clusters 0 and 1 are reserved)
        for i in 2..fat_cache.len() {
            debug!(
                "[FAT32] Checking cluster {}: FAT[{}] = {:#08x}",
                i, i, fat_cache[i]
            );
            if fat_cache[i] == CLUSTER_FREE {
                fat_cache[i] = CLUSTER_EOF; // Mark as end of chain
                debug!("[FAT32] Allocated cluster {}", i);
                return Some(i as u32);
            }
        }
        warn!("[FAT32] No free clusters available");
        None
    }

    fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), BlockError> {
        let mut fat_cache = self.fat_cache.lock();
        if cluster as usize >= fat_cache.len() || cluster < 2 {
            error!("[FAT32] Invalid cluster number for FAT write: {}", cluster);
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
                error!("[FAT32] FAT entry would exceed block boundary");
                return Err(BlockError::InvalidBlock);
            }

            let value_bytes = value.to_le_bytes();
            block_data[block_sector_offset..block_sector_offset + 4].copy_from_slice(&value_bytes);

            self.device.write_block(block_num as usize, &block_data)?;
            debug!(
                "[FAT32] Updated FAT {} entry for cluster {} with value {:#x}",
                fat_num, cluster, value
            );
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

    fn update_file_size(&self, _cluster: u32, _new_size: u32) -> Result<(), BlockError> {
        // Find the directory entry for this cluster and update its size
        // This is a simplified implementation - in a real system you'd want to track parent directory
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

    fn create_directory_entry(&self, parent_cluster: u32, name: &str, new_cluster: u32, is_dir: bool) -> Result<(), FileSystemError> {
        debug!("[FAT32] Creating directory entry: {} (cluster: {}, is_dir: {})", name, new_cluster, is_dir);

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
                return Ok(());
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
                    debug!("[FAT32] read_cluster failed for cluster {}: {:?}", current_cluster, e);
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
        let new_size = (offset + bytes_written as u64).max(*self.size.lock());
        *self.size.lock() = new_size;

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

        debug!("[FAT32] Looking for child: {}", name);
        let entries = self.fs().read_directory_entries(self.cluster)?;
        debug!("[FAT32] Found {} directory entries", entries.len());

        for info in entries {
            if info.entry.attr & ATTR_VOLUME_ID != 0 {
                continue;
            }

            if info.name.to_lowercase() == name.to_lowercase() {
                let entry = info.entry;
                let cluster =
                    (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                let is_dir = entry.attr & ATTR_DIRECTORY != 0;
                let size = if is_dir { 0 } else { entry.file_size as u64 };

                debug!(
                    "[FAT32] Found match: {} (cluster: {}, is_dir: {})",
                    info.name, cluster, is_dir
                );
                return Ok(Arc::new(FAT32Inode {
                    fs: self.fs,
                    cluster,
                    size: Mutex::new(size),
                    is_dir,
                    mode: Mutex::new(if is_dir { 0o755 } else { 0o644 }),  // 默认权限
                    uid: Mutex::new(0),       // root用户
                    gid: Mutex::new(0),       // root组
                }));
            }
        }

        debug!("[FAT32] Child {} not found", name);
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

        // Create file entry in parent directory
        fs.create_directory_entry(self.cluster, name, new_cluster, false)?;

        // Return the new file inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
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
        fs.create_directory_entry(self.cluster, name, new_cluster, true)?;

        // Return the new directory inode
        Ok(Arc::new(FAT32Inode {
            fs: self.fs,
            cluster: new_cluster,
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

            let entries = fs.read_directory_entries(current_cluster)?;

            for i in 0..entries.len() {
                if entries[i].name.to_lowercase() == name.to_lowercase() {
                    child_cluster = (entries[i].entry.first_cluster_high as u32) << 16
                        | entries[i].entry.first_cluster_low as u32;
                    found = true;

                    // Find the range of entries to delete
                    let mut start_index = i;
                    while start_index > 0 {
                        let order = cluster_data[(start_index - 1) * 32];
                        if order & 0x40 != 0 {
                            start_index -= 1;
                            break;
                        }
                        if cluster_data[(start_index - 1) * 32 + 11] & ATTR_LONG_NAME
                            != ATTR_LONG_NAME
                        {
                            break;
                        }
                        start_index -= 1;
                    }

                    for j in start_index..=i {
                        cluster_data[j * 32] = 0xE5;
                    }

                    fs.write_cluster(current_cluster, &cluster_data)
                        .map_err(|_| FileSystemError::IoError)?;
                    break;
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
