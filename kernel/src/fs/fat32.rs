use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::{mem, slice};
use spin::{Mutex, RwLock};

use crate::drivers::{BlockDevice, block::BlockError};
use super::{FileSystem, FileSystemError, Inode, InodeType, FileStat};

const SECTOR_SIZE: usize = 512;
const FAT32_SIGNATURE: u16 = 0xAA55;
const FAT32_END_OF_CHAIN: u32 = 0x0FFFFFFF;
const FAT32_BAD_CLUSTER: u32 = 0x0FFFFFF7;
const FAT32_FREE_CLUSTER: u32 = 0x00000000;

// FSInfo 扇区签名
const FSINFO_SIGNATURE1: u32 = 0x41615252;
const FSINFO_SIGNATURE2: u32 = 0x61417272;
const FSINFO_SIGNATURE3: u32 = 0xAA550000;

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct Fat32BootSector {
    pub jmp_boot: [u8; 3],
    pub oem_name: [u8; 8],
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    pub reserved_sector_count: u16,
    pub num_fats: u8,
    pub root_entry_count: u16,
    pub total_sectors_16: u16,
    pub media: u8,
    pub fat_size_16: u16,
    pub sectors_per_track: u16,
    pub num_heads: u16,
    pub hidden_sectors: u32,
    pub total_sectors_32: u32,
    pub fat_size_32: u32,
    pub ext_flags: u16,
    pub fs_ver: u16,
    pub root_cluster: u32,
    pub fs_info: u16,
    pub backup_boot_sector: u16,
    pub reserved: [u8; 12],
    pub drive_number: u8,
    pub reserved1: u8,
    pub boot_signature: u8,
    pub volume_id: u32,
    pub volume_label: [u8; 11],
    pub fs_type: [u8; 8],
    pub boot_code: [u8; 420],
    pub signature: u16,
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DirectoryEntry {
    pub name: [u8; 11],
    pub attributes: u8,
    pub nt_reserved: u8,
    pub creation_time_tenth: u8,
    pub creation_time: u16,
    pub creation_date: u16,
    pub last_access_date: u16,
    pub first_cluster_high: u16,
    pub write_time: u16,
    pub write_date: u16,
    pub first_cluster_low: u16,
    pub file_size: u32,
}

impl DirectoryEntry {
    pub const ATTR_READ_ONLY: u8 = 0x01;
    pub const ATTR_HIDDEN: u8 = 0x02;
    pub const ATTR_SYSTEM: u8 = 0x04;
    pub const ATTR_VOLUME_ID: u8 = 0x08;
    pub const ATTR_DIRECTORY: u8 = 0x10;
    pub const ATTR_ARCHIVE: u8 = 0x20;
    pub const ATTR_LONG_NAME: u8 = Self::ATTR_READ_ONLY
        | Self::ATTR_HIDDEN
        | Self::ATTR_SYSTEM
        | Self::ATTR_VOLUME_ID;

    pub fn is_valid(&self) -> bool {
        self.name[0] != 0x00 && self.name[0] != 0xE5
    }

    pub fn is_directory(&self) -> bool {
        self.attributes & Self::ATTR_DIRECTORY != 0
    }

    pub fn is_long_name(&self) -> bool {
        self.attributes == Self::ATTR_LONG_NAME
    }

    pub fn first_cluster(&self) -> u32 {
        ((self.first_cluster_high as u32) << 16) | (self.first_cluster_low as u32)
    }

    pub fn set_first_cluster(&mut self, cluster: u32) {
        self.first_cluster_high = (cluster >> 16) as u16;
        self.first_cluster_low = (cluster & 0xFFFF) as u16;
    }

    pub fn short_name(&self) -> String {
        let mut name = String::new();

        for i in 0..8 {
            if self.name[i] == b' ' { break; }
            name.push(self.name[i] as char);
        }

        let mut ext = String::new();
        for i in 8..11 {
            if self.name[i] == b' ' { break; }
            ext.push(self.name[i] as char);
        }

        if !ext.is_empty() {
            name.push('.');
            name.push_str(&ext);
        }

        name.to_uppercase()
    }

    pub fn set_short_name(&mut self, name: &str) {
        self.name.fill(b' ');

        // 分离文件名和扩展名
        let (base_name, extension) = if let Some(dot_pos) = name.rfind('.') {
            (&name[..dot_pos], Some(&name[dot_pos + 1..]))
        } else {
            (name, None)
        };

        // FAT32短文件名生成规则
        let clean_base = Self::clean_filename_part(base_name).to_uppercase();

        // 生成基础名（最多8字符）
        if clean_base.len() > 8 {
            // 长文件名处理：前6字符 + ~1
            let mut short_base = String::new();
            let mut char_count = 0;

            for ch in clean_base.chars() {
                if char_count >= 6 {
                    break;
                }
                short_base.push(ch);
                char_count += 1;
            }

            short_base.push_str("~1");

            // 写入短文件名基础部分
            for (i, byte) in short_base.bytes().enumerate() {
                if i < 8 {
                    self.name[i] = byte;
                }
            }
        } else {
            // 短文件名直接使用
            for (i, byte) in clean_base.bytes().take(8).enumerate() {
                self.name[i] = byte;
            }
        }

        // 处理扩展名（最多3字符）
        if let Some(ext) = extension {
            let clean_ext = Self::clean_filename_part(ext).to_uppercase();
            for (i, byte) in clean_ext.bytes().take(3).enumerate() {
                self.name[8 + i] = byte;
            }
        }
    }

    /// 清理文件名部分，移除非法字符并转换合法字符
    fn clean_filename_part(part: &str) -> String {
        let mut result = String::new();

        for ch in part.chars() {
            match ch {
                // 合法的ASCII字符
                'A'..='Z' | 'a'..='z' | '0'..='9' => {
                    result.push(ch);
                }
                // FAT32允许的特殊字符
                '!' | '#' | '$' | '%' | '&' | '\'' | '(' | ')' | '-' | '@' | '^' | '_' | '`' | '{' | '}' | '~' => {
                    result.push(ch);
                }
                // 空格在短文件名中不允许，忽略
                ' ' => {
                    // 跳过空格
                }
                // 其他字符转换为下划线
                _ => {
                    result.push('_');
                }
            }
        }

        result
    }

    /// 根据长文件名生成短文件名字符串（用于查找）
    pub fn generate_short_name(long_name: &str) -> String {
        let mut temp_entry = DirectoryEntry {
            name: [b' '; 11],
            attributes: 0,
            nt_reserved: 0,
            creation_time_tenth: 0,
            creation_time: 0,
            creation_date: 0,
            last_access_date: 0,
            first_cluster_high: 0,
            write_time: 0,
            write_date: 0,
            first_cluster_low: 0,
            file_size: 0,
        };

        temp_entry.set_short_name(long_name);
        temp_entry.short_name()
    }
}

#[repr(C, packed)]
#[derive(Debug, Clone, Copy)]
pub struct FSInfo {
    pub signature1: u32,        // 0x41615252
    pub reserved1: [u8; 480],
    pub signature2: u32,        // 0x61417272
    pub free_count: u32,        // 空闲簇数量，0xFFFFFFFF表示未知
    pub next_free: u32,         // 下次分配搜索起始位置
    pub reserved2: [u8; 12],
    pub signature3: u32,        // 0xAA550000
}

impl FSInfo {
    pub fn is_valid(&self) -> bool {
        self.signature1 == FSINFO_SIGNATURE1 &&
        self.signature2 == FSINFO_SIGNATURE2 &&
        (self.signature3 & 0xFFFF0000) == (FSINFO_SIGNATURE3 & 0xFFFF0000)
    }
}

pub struct ClusterManager {
    fat_start_sector: u32,
    sectors_per_fat: u32,
    sectors_per_cluster: u32,
    first_data_sector: u32,
    total_clusters: u32,
    block_device: Arc<dyn BlockDevice>,
    fat_cache: RwLock<BTreeMap<u32, u32>>,
    // 简化同步机制，使用单一的allocation_lock来保护关键数据
    allocation_data: Mutex<AllocationData>,
    fsinfo_sector: u32,
}

#[derive(Debug)]
struct AllocationData {
    next_free_cluster: u32,
    free_cluster_count: u32,
}

impl core::fmt::Debug for ClusterManager {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ClusterManager")
            .field("fat_start_sector", &self.fat_start_sector)
            .field("sectors_per_fat", &self.sectors_per_fat)
            .field("sectors_per_cluster", &self.sectors_per_cluster)
            .field("first_data_sector", &self.first_data_sector)
            .field("total_clusters", &self.total_clusters)
            .field("block_device", &"<BlockDevice>")
            .field("fat_cache", &self.fat_cache)
            .field("allocation_data", &self.allocation_data)
            .field("fsinfo_sector", &self.fsinfo_sector)
            .finish()
    }
}

impl ClusterManager {
    pub fn new(
        boot_sector: &Fat32BootSector,
        block_device: Arc<dyn BlockDevice>,
    ) -> Self {
        let fat_start_sector = boot_sector.reserved_sector_count as u32;
        let root_dir_sectors = 0;
        let first_data_sector = fat_start_sector
            + (boot_sector.num_fats as u32 * boot_sector.fat_size_32)
            + root_dir_sectors;

        let total_sectors = if boot_sector.total_sectors_16 != 0 {
            boot_sector.total_sectors_16 as u32
        } else {
            boot_sector.total_sectors_32
        };

        let data_sectors = total_sectors - first_data_sector;
        let total_clusters = data_sectors / boot_sector.sectors_per_cluster as u32;

        let fsinfo_sector = boot_sector.fs_info as u32;
        let (next_free, free_count) = Self::load_fsinfo(&block_device, fsinfo_sector, total_clusters);

        Self {
            fat_start_sector,
            sectors_per_fat: boot_sector.fat_size_32,
            sectors_per_cluster: boot_sector.sectors_per_cluster as u32,
            first_data_sector,
            total_clusters,
            block_device,
            fat_cache: RwLock::new(BTreeMap::new()),
            allocation_data: Mutex::new(AllocationData {
                next_free_cluster: next_free,
                free_cluster_count: free_count,
            }),
            fsinfo_sector,
        }
    }

    fn load_fsinfo(block_device: &Arc<dyn BlockDevice>, fsinfo_sector: u32, total_clusters: u32) -> (u32, u32) {
        let block_size = block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;
        let block_id = fsinfo_sector as usize / sectors_per_block;
        let sector_in_block = fsinfo_sector as usize % sectors_per_block;
        let sector_offset = sector_in_block * SECTOR_SIZE;

        let mut block_buf = vec![0u8; block_size];
        if block_device.read_block(block_id, &mut block_buf).is_ok() {
            let fsinfo_data = &block_buf[sector_offset..sector_offset + SECTOR_SIZE];
            if fsinfo_data.len() >= core::mem::size_of::<FSInfo>() {
                let fsinfo = unsafe {
                    *(fsinfo_data.as_ptr() as *const FSInfo)
                };

                if fsinfo.is_valid() {
                    let next_free = if fsinfo.next_free < 2 || fsinfo.next_free >= total_clusters + 2 {
                        2  // 默认从簇2开始
                    } else {
                        fsinfo.next_free
                    };

                    let free_count = if fsinfo.free_count == 0xFFFFFFFF {
                        total_clusters  // 未知，估计全部为空闲
                    } else {
                        fsinfo.free_count
                    };

                    return (next_free, free_count);
                }
            }
        }

        warn!("Failed to load FSInfo, using defaults");
        (2, total_clusters) // 默认值
    }

    fn update_fsinfo(&self, next_free: u32, free_count: u32) {
        let block_size = self.block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;
        let block_id = self.fsinfo_sector as usize / sectors_per_block;
        let sector_in_block = self.fsinfo_sector as usize % sectors_per_block;
        let sector_offset = sector_in_block * SECTOR_SIZE;

        let mut block_buf = vec![0u8; block_size];
        if self.block_device.read_block(block_id, &mut block_buf).is_ok() {
            let fsinfo_data = &mut block_buf[sector_offset..sector_offset + SECTOR_SIZE];
            if fsinfo_data.len() >= core::mem::size_of::<FSInfo>() {
                let fsinfo = unsafe {
                    &mut *(fsinfo_data.as_mut_ptr() as *mut FSInfo)
                };

                if fsinfo.is_valid() {
                    fsinfo.next_free = next_free;
                    fsinfo.free_count = free_count;

                    if self.block_device.write_block(block_id, &block_buf).is_ok() {
                        debug!("Updated FSInfo: next_free={}, free_count={}", next_free, free_count);
                    }
                }
            }
        }
    }

    pub fn cluster_to_sector(&self, cluster: u32) -> u32 {
        if cluster < 2 {
            return 0;
        }
        self.first_data_sector + (cluster - 2) * self.sectors_per_cluster
    }

    pub fn read_fat_entry(&self, cluster: u32) -> Result<u32, FileSystemError> {
        {
            let cache = self.fat_cache.read();
            if let Some(&entry) = cache.get(&cluster) {
                return Ok(entry);
            }
        }

        let fat_offset = cluster * 4;
        let fat_sector = self.fat_start_sector + (fat_offset / SECTOR_SIZE as u32);
        let entry_offset = (fat_offset % SECTOR_SIZE as u32) as usize;

        // 计算块号和块内偏移
        let block_size = self.block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;
        let block_id = fat_sector as usize / sectors_per_block;
        let sector_in_block = fat_sector as usize % sectors_per_block;
        let sector_offset = sector_in_block * SECTOR_SIZE;

        let mut block_buf = vec![0u8; block_size];
        self.block_device
            .read_block(block_id, &mut block_buf)
            .map_err(|_| FileSystemError::IoError)?;

        let sector_buf = &block_buf[sector_offset..sector_offset + SECTOR_SIZE];
        let entry = u32::from_le_bytes([
            sector_buf[entry_offset],
            sector_buf[entry_offset + 1],
            sector_buf[entry_offset + 2],
            sector_buf[entry_offset + 3],
        ]) & 0x0FFFFFFF;

        {
            let mut cache = self.fat_cache.write();
            cache.insert(cluster, entry);
        }

        Ok(entry)
    }

    pub fn write_fat_entry(&self, cluster: u32, value: u32) -> Result<(), FileSystemError> {
        let fat_offset = cluster * 4;
        let fat_sector = self.fat_start_sector + (fat_offset / SECTOR_SIZE as u32);
        let entry_offset = (fat_offset % SECTOR_SIZE as u32) as usize;

        // 计算块号和块内偏移
        let block_size = self.block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;
        let block_id = fat_sector as usize / sectors_per_block;
        let sector_in_block = fat_sector as usize % sectors_per_block;
        let sector_offset = sector_in_block * SECTOR_SIZE;

        let mut block_buf = vec![0u8; block_size];
        self.block_device
            .read_block(block_id, &mut block_buf)
            .map_err(|_| FileSystemError::IoError)?;

        let masked_value = value & 0x0FFFFFFF;
        let bytes = masked_value.to_le_bytes();
        let sector_buf = &mut block_buf[sector_offset..sector_offset + SECTOR_SIZE];
        sector_buf[entry_offset..entry_offset + 4].copy_from_slice(&bytes);

        self.block_device
            .write_block(block_id, &block_buf)
            .map_err(|_| FileSystemError::IoError)?;

        {
            let mut cache = self.fat_cache.write();
            cache.insert(cluster, masked_value);
        }

        Ok(())
    }

    pub fn allocate_cluster(&self) -> Result<u32, FileSystemError> {
        // 限制搜索范围，避免长时间锁定
        const MAX_SEARCH_ATTEMPTS: u32 = 256;

        let mut alloc_data = self.allocation_data.lock();

        // 快速检查是否还有空闲簇
        if alloc_data.free_cluster_count == 0 {
            warn!("No free clusters available");
            return Err(FileSystemError::NoSpace);
        }

        let start_cluster = alloc_data.next_free_cluster;
        let mut current = start_cluster;

        debug!("Starting cluster allocation from cluster {} (free_count={})",
               start_cluster, alloc_data.free_cluster_count);

        for attempts in 1..=MAX_SEARCH_ATTEMPTS {
            // 确保簇号在有效范围内
            if current < 2 {
                current = 2;
            }
            if current >= self.total_clusters + 2 {
                current = 2; // 回绕到开头
            }

            // 释放锁来读取FAT表项，避免死锁
            drop(alloc_data);
            let fat_entry = match self.read_fat_entry(current) {
                Ok(entry) => entry,
                Err(e) => {
                    warn!("Error reading FAT entry for cluster {}: {:?}", current, e);
                    current += 1;
                    // 重新获取锁
                    alloc_data = self.allocation_data.lock();
                    continue;
                }
            };

            if fat_entry == FAT32_FREE_CLUSTER {
                debug!("Found free cluster {} after {} attempts", current, attempts);

                // 分配簇
                if let Err(e) = self.write_fat_entry(current, FAT32_END_OF_CHAIN) {
                    warn!("Failed to write FAT entry for cluster {}: {:?}", current, e);
                    current += 1;
                    // 重新获取锁
                    alloc_data = self.allocation_data.lock();
                    continue;
                }

                // 重新获取锁更新分配数据
                alloc_data = self.allocation_data.lock();

                // 更新下一个搜索起始位置
                let new_next_free = if current + 1 >= self.total_clusters + 2 {
                    2
                } else {
                    current + 1
                };

                alloc_data.next_free_cluster = new_next_free;
                alloc_data.free_cluster_count = alloc_data.free_cluster_count.saturating_sub(1);

                debug!("Successfully allocated cluster {}, next_free={}, remaining={}",
                       current, new_next_free, alloc_data.free_cluster_count);

                // 异步更新FSInfo扇区（在锁外进行）
                let next_free = alloc_data.next_free_cluster;
                let free_count = alloc_data.free_cluster_count;
                drop(alloc_data);
                self.update_fsinfo(next_free, free_count);

                return Ok(current);
            } else {
                // 簇已被占用，继续搜索
                if attempts <= 5 {
                    debug!("Cluster {} is occupied (entry=0x{:08x})", current, fat_entry);
                }
                current += 1;
                if current >= self.total_clusters + 2 {
                    current = 2; // 回绕
                }

                // 如果回到了起始位置，说明搜索了一圈
                if current == start_cluster {
                    warn!("Wrapped around to starting cluster, stopping search");
                    return Err(FileSystemError::NoSpace);
                }

                // 重新获取锁
                alloc_data = self.allocation_data.lock();
            }
        }

        warn!("No free clusters found after {} attempts", MAX_SEARCH_ATTEMPTS);
        Err(FileSystemError::NoSpace)
    }

    pub fn free_cluster_chain(&self, start_cluster: u32) -> Result<(), FileSystemError> {
        let mut current = start_cluster;
        let mut freed_count = 0u32;

        while current < FAT32_END_OF_CHAIN && current >= 2 {
            let next = self.read_fat_entry(current)?;
            self.write_fat_entry(current, FAT32_FREE_CLUSTER)?;
            freed_count += 1;
            current = next;
        }

        // 更新空闲簇计数
        if freed_count > 0 {
            let mut alloc_data = self.allocation_data.lock();
            alloc_data.free_cluster_count = alloc_data.free_cluster_count.saturating_add(freed_count);
            // 如果释放的簇比当前下一个空闲簇位置更靠前，更新搜索起始位置
            if start_cluster < alloc_data.next_free_cluster {
                alloc_data.next_free_cluster = start_cluster;
            }
            debug!("Freed {} clusters starting from {}, free_count now {}",
                   freed_count, start_cluster, alloc_data.free_cluster_count);
        }

        Ok(())
    }

    pub fn get_cluster_chain(&self, start_cluster: u32) -> Result<Vec<u32>, FileSystemError> {
        let mut chain = Vec::new();
        let mut current = start_cluster;
        let mut seen = BTreeSet::new();

        // 限制最大簇链长度防止无限循环
        const MAX_CHAIN_LENGTH: usize = 65536;

        while current >= 2 && current < FAT32_END_OF_CHAIN && chain.len() < MAX_CHAIN_LENGTH {
            // 检测循环引用
            if seen.contains(&current) {
                warn!("Detected circular reference in FAT chain at cluster {}", current);
                break;
            }
            seen.insert(current);

            // 检查簇号是否在有效范围内
            if current >= self.total_clusters + 2 {
                warn!("Invalid cluster number {} in FAT chain", current);
                break;
            }

            chain.push(current);

            match self.read_fat_entry(current) {
                Ok(next) => {
                    current = next;
                    // 检查特殊值
                    if next == FAT32_BAD_CLUSTER {
                        warn!("Encountered bad cluster {} in FAT chain", next);
                        break;
                    }
                    if next == FAT32_FREE_CLUSTER && !chain.is_empty() {
                        warn!("Encountered free cluster {} in FAT chain", next);
                        break;
                    }
                }
                Err(e) => {
                    warn!("Failed to read FAT entry for cluster {}: {:?}", current, e);
                    break;
                }
            }
        }

        if chain.len() >= MAX_CHAIN_LENGTH {
            warn!("FAT chain exceeded maximum length, truncating");
        }

        Ok(chain)
    }

    pub fn extend_cluster_chain(&self, last_cluster: u32) -> Result<u32, FileSystemError> {
        let new_cluster = self.allocate_cluster()?;
        self.write_fat_entry(last_cluster, new_cluster)?;
        Ok(new_cluster)
    }

    pub fn read_cluster(&self, cluster: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
        let start_sector = self.cluster_to_sector(cluster);
        let cluster_size = self.sectors_per_cluster as usize * SECTOR_SIZE;

        if buf.len() != cluster_size {
            return Err(FileSystemError::IoError);
        }

        let block_size = self.block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;

        // 读取簇涉及的所有块
        for i in 0..self.sectors_per_cluster {
            let sector = start_sector + i;
            let block_id = sector as usize / sectors_per_block;
            let sector_in_block = sector as usize % sectors_per_block;
            let sector_offset_in_block = sector_in_block * SECTOR_SIZE;

            let mut block_buf = vec![0u8; block_size];
            self.block_device
                .read_block(block_id, &mut block_buf)
                .map_err(|_| FileSystemError::IoError)?;

            let buf_offset = i as usize * SECTOR_SIZE;
            buf[buf_offset..buf_offset + SECTOR_SIZE]
                .copy_from_slice(&block_buf[sector_offset_in_block..sector_offset_in_block + SECTOR_SIZE]);
        }

        Ok(())
    }

    pub fn write_cluster(&self, cluster: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        let start_sector = self.cluster_to_sector(cluster);
        let cluster_size = self.sectors_per_cluster as usize * SECTOR_SIZE;

        if buf.len() != cluster_size {
            return Err(FileSystemError::IoError);
        }

        let block_size = self.block_device.block_size();
        let sectors_per_block = block_size / SECTOR_SIZE;

        // 写入簇涉及的所有块
        for i in 0..self.sectors_per_cluster {
            let sector = start_sector + i;
            let block_id = sector as usize / sectors_per_block;
            let sector_in_block = sector as usize % sectors_per_block;
            let sector_offset_in_block = sector_in_block * SECTOR_SIZE;

            let mut block_buf = vec![0u8; block_size];

            // 如果不是写整个块，需要先读取
            if sectors_per_block > 1 {
                self.block_device
                    .read_block(block_id, &mut block_buf)
                    .map_err(|_| FileSystemError::IoError)?;
            }

            let buf_offset = i as usize * SECTOR_SIZE;
            block_buf[sector_offset_in_block..sector_offset_in_block + SECTOR_SIZE]
                .copy_from_slice(&buf[buf_offset..buf_offset + SECTOR_SIZE]);

            self.block_device
                .write_block(block_id, &block_buf)
                .map_err(|_| FileSystemError::IoError)?;
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct FAT32FileSystem {
    boot_sector: Fat32BootSector,
    cluster_manager: Arc<ClusterManager>,
    root_cluster: u32,
}

impl FAT32FileSystem {
    pub fn new(block_device: Arc<dyn BlockDevice>) -> Result<Arc<Self>, FileSystemError> {
        let block_size = block_device.block_size();
        let mut boot_buf = vec![0u8; block_size];
        block_device
            .read_block(0, &mut boot_buf)
            .map_err(|_| FileSystemError::IoError)?;

        // FAT32引导扇区在第一个扇区（块的前512字节）
        let boot_sector = unsafe { *(boot_buf.as_ptr() as *const Fat32BootSector) };

        if boot_sector.signature != FAT32_SIGNATURE {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let cluster_manager = Arc::new(ClusterManager::new(&boot_sector, block_device));

        let fs = Arc::new(Self {
            root_cluster: boot_sector.root_cluster,
            boot_sector,
            cluster_manager,
        });

        Ok(fs)
    }

    pub fn cluster_size(&self) -> usize {
        self.boot_sector.sectors_per_cluster as usize * SECTOR_SIZE
    }
}

#[derive(Debug)]
pub struct Fat32RootInode {
    root_cluster: u32,
    cluster_size: usize,
    cluster_manager: Arc<ClusterManager>,
}

impl Fat32RootInode {
    pub fn new(root_cluster: u32, cluster_size: usize, cluster_manager: Arc<ClusterManager>) -> Self {
        Self {
            root_cluster,
            cluster_size,
            cluster_manager,
        }
    }
}

impl Inode for Fat32RootInode {
    fn inode_type(&self) -> InodeType {
        InodeType::Directory
    }

    fn size(&self) -> u64 {
        0
    }

    fn read_at(&self, _offset: u64, _buf: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }

    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        if self.root_cluster == 0 {
            return Ok(Vec::new());
        }

        let cluster_chain = self.cluster_manager.get_cluster_chain(self.root_cluster)?;
        let mut entries = Vec::new();

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    *(cluster_buf.as_ptr().add(offset) as *const DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                let name = dir_entry.short_name();
                entries.push(name);
            }
        }

        Ok(entries)
    }

    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.root_cluster == 0 {
            return Err(FileSystemError::NotFound);
        }

        let upper_name = name.to_uppercase();
        let short_name = DirectoryEntry::generate_short_name(name);
        let cluster_chain = self.cluster_manager.get_cluster_chain(self.root_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    *(cluster_buf.as_ptr().add(offset) as *const DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                let entry_name = dir_entry.short_name();
                // 检查是否匹配：支持原名、大写名或生成的短文件名
                if entry_name == upper_name || entry_name == short_name {
                    return Ok(Arc::new(Fat32SimpleInode::new(
                        dir_entry,
                        cluster,
                        offset,
                        self.cluster_size,
                        Arc::clone(&self.cluster_manager),
                    )));
                }
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_entry(name, false)
    }

    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_entry(name, true)
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        if self.root_cluster == 0 {
            return Err(FileSystemError::NotFound);
        }

        let upper_name = name.to_uppercase();
        let cluster_chain = self.cluster_manager.get_cluster_chain(self.root_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    &mut *(cluster_buf.as_mut_ptr().add(offset) as *mut DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                if dir_entry.short_name() == upper_name {
                    let first_cluster = dir_entry.first_cluster();

                    if first_cluster != 0 {
                        self.cluster_manager.free_cluster_chain(first_cluster)?;
                    }

                    dir_entry.name[0] = 0xE5;

                    self.cluster_manager.write_cluster(cluster, &cluster_buf)?;

                    return Ok(());
                }
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn truncate(&self, _new_size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::IsDirectory)
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}

impl Fat32RootInode {
    fn create_entry(&self, name: &str, is_directory: bool) -> Result<Arc<dyn Inode>, FileSystemError> {
        if self.root_cluster == 0 {
            return Err(FileSystemError::IoError);
        }

        if self.find_child(name).is_ok() {
            return Err(FileSystemError::AlreadyExists);
        }

        let new_cluster = self.cluster_manager.allocate_cluster()?;

        let mut new_entry = DirectoryEntry {
            name: [b' '; 11],
            attributes: if is_directory { DirectoryEntry::ATTR_DIRECTORY } else { 0 },
            nt_reserved: 0,
            creation_time_tenth: 0,
            creation_time: 0,
            creation_date: 0,
            last_access_date: 0,
            first_cluster_high: 0,
            write_time: 0,
            write_date: 0,
            first_cluster_low: 0,
            file_size: 0,
        };

        new_entry.set_short_name(name);
        new_entry.set_first_cluster(new_cluster);

        if is_directory {
            let mut dir_buf = vec![0u8; self.cluster_size];

            let mut dot_entry = new_entry;
            dot_entry.set_short_name(".");

            let mut dotdot_entry = new_entry;
            dotdot_entry.set_short_name("..");
            dotdot_entry.set_first_cluster(self.root_cluster);

            let dot_bytes = unsafe {
                slice::from_raw_parts(
                    &dot_entry as *const DirectoryEntry as *const u8,
                    mem::size_of::<DirectoryEntry>(),
                )
            };

            let dotdot_bytes = unsafe {
                slice::from_raw_parts(
                    &dotdot_entry as *const DirectoryEntry as *const u8,
                    mem::size_of::<DirectoryEntry>(),
                )
            };

            dir_buf[0..mem::size_of::<DirectoryEntry>()].copy_from_slice(dot_bytes);
            dir_buf[mem::size_of::<DirectoryEntry>()..2 * mem::size_of::<DirectoryEntry>()]
                .copy_from_slice(dotdot_bytes);

            self.cluster_manager.write_cluster(new_cluster, &dir_buf)?;
        } else {
            let mut file_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.write_cluster(new_cluster, &file_buf)?;
        }

        let cluster_chain = self.cluster_manager.get_cluster_chain(self.root_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    &mut *(cluster_buf.as_mut_ptr().add(offset) as *mut DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    *dir_entry = new_entry;

                    self.cluster_manager.write_cluster(cluster, &cluster_buf)?;

                    return Ok(Arc::new(Fat32SimpleInode::new(
                        new_entry,
                        cluster,
                        offset,
                        self.cluster_size,
                        Arc::clone(&self.cluster_manager),
                    )));
                }
            }
        }

        Err(FileSystemError::NoSpace)
    }
}

#[derive(Debug)]
pub struct Fat32SimpleInode {
    entry: Mutex<DirectoryEntry>,
    parent_cluster: u32,
    entry_offset: usize,
    cluster_size: usize,
    cluster_manager: Arc<ClusterManager>,
}

impl Fat32SimpleInode {
    pub fn new(
        entry: DirectoryEntry,
        parent_cluster: u32,
        entry_offset: usize,
        cluster_size: usize,
        cluster_manager: Arc<ClusterManager>,
    ) -> Self {
        Self {
            entry: Mutex::new(entry),
            parent_cluster,
            entry_offset,
            cluster_size,
            cluster_manager,
        }
    }

    fn update_entry_on_disk(&self) -> Result<(), FileSystemError> {
        let entry = self.entry.lock();

        let mut cluster_buf = vec![0u8; self.cluster_size];
        self.cluster_manager.read_cluster(self.parent_cluster, &mut cluster_buf)?;

        let entry_bytes = unsafe {
            slice::from_raw_parts(
                &*entry as *const DirectoryEntry as *const u8,
                mem::size_of::<DirectoryEntry>(),
            )
        };

        let start = self.entry_offset;
        let end = start + mem::size_of::<DirectoryEntry>();
        cluster_buf[start..end].copy_from_slice(entry_bytes);

        self.cluster_manager.write_cluster(self.parent_cluster, &cluster_buf)?;

        Ok(())
    }
}

impl Inode for Fat32SimpleInode {
    fn inode_type(&self) -> InodeType {
        let entry = self.entry.lock();
        if entry.is_directory() {
            InodeType::Directory
        } else {
            InodeType::File
        }
    }

    fn size(&self) -> u64 {
        let entry = self.entry.lock();
        entry.file_size as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let entry = self.entry.lock();
        let start_cluster = entry.first_cluster();

        if start_cluster == 0 || offset >= entry.file_size as u64 {
            return Ok(0);
        }

        drop(entry);

        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        let mut bytes_read = 0;
        let mut file_offset = offset as usize;
        let mut buf_offset = 0;

        for &cluster in &cluster_chain {
            if buf_offset >= buf.len() {
                break;
            }

            if file_offset >= self.cluster_size {
                file_offset -= self.cluster_size;
                continue;
            }

            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let read_start = file_offset;
            let read_end = self.cluster_size.min(read_start + buf.len() - buf_offset);
            let read_len = read_end - read_start;

            if read_len > 0 {
                buf[buf_offset..buf_offset + read_len]
                    .copy_from_slice(&cluster_buf[read_start..read_end]);
                bytes_read += read_len;
                buf_offset += read_len;
            }

            file_offset = 0;
        }

        Ok(bytes_read)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        let mut entry = self.entry.lock();
        let start_cluster = entry.first_cluster();

        if start_cluster == 0 {
            return Err(FileSystemError::IoError);
        }

        let needed_size = offset as usize + buf.len();
        let needed_clusters = (needed_size + self.cluster_size - 1) / self.cluster_size;

        let mut cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        while cluster_chain.len() < needed_clusters {
            let last_cluster = *cluster_chain.last().unwrap();
            let new_cluster = self.cluster_manager.extend_cluster_chain(last_cluster)?;
            cluster_chain.push(new_cluster);
        }

        let mut bytes_written = 0;
        let mut file_offset = offset as usize;
        let mut buf_offset = 0;

        for &cluster in &cluster_chain {
            if buf_offset >= buf.len() {
                break;
            }

            if file_offset >= self.cluster_size {
                file_offset -= self.cluster_size;
                continue;
            }

            let mut cluster_buf = vec![0u8; self.cluster_size];

            if file_offset > 0 || buf.len() - buf_offset < self.cluster_size - file_offset {
                self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;
            }

            let write_start = file_offset;
            let write_end = self.cluster_size.min(write_start + buf.len() - buf_offset);
            let write_len = write_end - write_start;

            if write_len > 0 {
                cluster_buf[write_start..write_end]
                    .copy_from_slice(&buf[buf_offset..buf_offset + write_len]);
                bytes_written += write_len;
                buf_offset += write_len;
            }

            self.cluster_manager.write_cluster(cluster, &cluster_buf)?;
            file_offset = 0;
        }

        let new_size = (offset as usize + bytes_written).max(entry.file_size as usize);
        entry.file_size = new_size as u32;
        drop(entry);

        self.update_entry_on_disk()?;

        Ok(bytes_written)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        let entry = self.entry.lock();
        if !entry.is_directory() {
            return Err(FileSystemError::NotDirectory);
        }

        let start_cluster = entry.first_cluster();
        drop(entry);

        if start_cluster == 0 {
            return Ok(Vec::new());
        }

        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;
        let mut entries = Vec::new();

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    *(cluster_buf.as_ptr().add(offset) as *const DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                let name = dir_entry.short_name();
                if name != "." && name != ".." {
                    entries.push(name);
                }
            }
        }

        Ok(entries)
    }

    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let entry = self.entry.lock();
        if !entry.is_directory() {
            return Err(FileSystemError::NotDirectory);
        }

        let start_cluster = entry.first_cluster();
        drop(entry);

        if start_cluster == 0 {
            return Err(FileSystemError::NotFound);
        }

        let upper_name = name.to_uppercase();
        let short_name = DirectoryEntry::generate_short_name(name);
        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    *(cluster_buf.as_ptr().add(offset) as *const DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                let entry_name = dir_entry.short_name();
                // 检查是否匹配：支持原名、大写名或生成的短文件名
                if entry_name == upper_name || entry_name == short_name {
                    return Ok(Arc::new(Fat32SimpleInode::new(
                        dir_entry,
                        cluster,
                        offset,
                        self.cluster_size,
                        Arc::clone(&self.cluster_manager),
                    )));
                }
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_entry(name, false)
    }

    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.create_entry(name, true)
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        let entry = self.entry.lock();
        if !entry.is_directory() {
            return Err(FileSystemError::NotDirectory);
        }

        let start_cluster = entry.first_cluster();
        drop(entry);

        if start_cluster == 0 {
            return Err(FileSystemError::NotFound);
        }

        let upper_name = name.to_uppercase();
        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    &mut *(cluster_buf.as_mut_ptr().add(offset) as *mut DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    if dir_entry.name[0] == 0x00 {
                        break;
                    }
                    continue;
                }

                if dir_entry.is_long_name() {
                    continue;
                }

                if dir_entry.short_name() == upper_name {
                    let first_cluster = dir_entry.first_cluster();

                    if first_cluster != 0 {
                        self.cluster_manager.free_cluster_chain(first_cluster)?;
                    }

                    dir_entry.name[0] = 0xE5;

                    self.cluster_manager.write_cluster(cluster, &cluster_buf)?;

                    return Ok(());
                }
            }
        }

        Err(FileSystemError::NotFound)
    }

    fn truncate(&self, new_size: u64) -> Result<(), FileSystemError> {
        let mut entry = self.entry.lock();
        let current_size = entry.file_size as u64;

        if new_size >= current_size {
            return Ok(());
        }

        let start_cluster = entry.first_cluster();
        if start_cluster == 0 {
            return Ok(());
        }

        let cluster_size = self.cluster_size as u64;
        let needed_clusters = if new_size == 0 { 0 } else { (new_size + cluster_size - 1) / cluster_size } as usize;

        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        if needed_clusters == 0 {
            self.cluster_manager.free_cluster_chain(start_cluster)?;
            entry.set_first_cluster(0);
        } else if needed_clusters < cluster_chain.len() {
            let truncate_start = cluster_chain[needed_clusters];
            self.cluster_manager.write_fat_entry(cluster_chain[needed_clusters - 1], FAT32_END_OF_CHAIN)?;
            self.cluster_manager.free_cluster_chain(truncate_start)?;
        }

        entry.file_size = new_size as u32;
        drop(entry);

        self.update_entry_on_disk()?;

        Ok(())
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        self.update_entry_on_disk()
    }
}

impl Fat32SimpleInode {
    fn create_entry(&self, name: &str, is_directory: bool) -> Result<Arc<dyn Inode>, FileSystemError> {
        let entry = self.entry.lock();
        if !entry.is_directory() {
            return Err(FileSystemError::NotDirectory);
        }

        let start_cluster = entry.first_cluster();
        drop(entry);

        if start_cluster == 0 {
            return Err(FileSystemError::IoError);
        }

        if self.find_child(name).is_ok() {
            return Err(FileSystemError::AlreadyExists);
        }

        let new_cluster = self.cluster_manager.allocate_cluster()?;

        let mut new_entry = DirectoryEntry {
            name: [b' '; 11],
            attributes: if is_directory { DirectoryEntry::ATTR_DIRECTORY } else { 0 },
            nt_reserved: 0,
            creation_time_tenth: 0,
            creation_time: 0,
            creation_date: 0,
            last_access_date: 0,
            first_cluster_high: 0,
            write_time: 0,
            write_date: 0,
            first_cluster_low: 0,
            file_size: 0,
        };

        new_entry.set_short_name(name);
        new_entry.set_first_cluster(new_cluster);

        if is_directory {
            let mut dir_buf = vec![0u8; self.cluster_size];

            let mut dot_entry = new_entry;
            dot_entry.set_short_name(".");

            let mut dotdot_entry = new_entry;
            dotdot_entry.set_short_name("..");
            dotdot_entry.set_first_cluster(start_cluster);

            let dot_bytes = unsafe {
                slice::from_raw_parts(
                    &dot_entry as *const DirectoryEntry as *const u8,
                    mem::size_of::<DirectoryEntry>(),
                )
            };

            let dotdot_bytes = unsafe {
                slice::from_raw_parts(
                    &dotdot_entry as *const DirectoryEntry as *const u8,
                    mem::size_of::<DirectoryEntry>(),
                )
            };

            dir_buf[0..mem::size_of::<DirectoryEntry>()].copy_from_slice(dot_bytes);
            dir_buf[mem::size_of::<DirectoryEntry>()..2 * mem::size_of::<DirectoryEntry>()]
                .copy_from_slice(dotdot_bytes);

            self.cluster_manager.write_cluster(new_cluster, &dir_buf)?;
        } else {
            let mut file_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.write_cluster(new_cluster, &file_buf)?;
        }

        let cluster_chain = self.cluster_manager.get_cluster_chain(start_cluster)?;

        for &cluster in &cluster_chain {
            let mut cluster_buf = vec![0u8; self.cluster_size];
            self.cluster_manager.read_cluster(cluster, &mut cluster_buf)?;

            let entries_per_cluster = self.cluster_size / mem::size_of::<DirectoryEntry>();
            for i in 0..entries_per_cluster {
                let offset = i * mem::size_of::<DirectoryEntry>();
                let dir_entry = unsafe {
                    &mut *(cluster_buf.as_mut_ptr().add(offset) as *mut DirectoryEntry)
                };

                if !dir_entry.is_valid() {
                    *dir_entry = new_entry;

                    self.cluster_manager.write_cluster(cluster, &cluster_buf)?;

                    return Ok(Arc::new(Fat32SimpleInode::new(
                        new_entry,
                        cluster,
                        offset,
                        self.cluster_size,
                        Arc::clone(&self.cluster_manager),
                    )));
                }
            }
        }

        Err(FileSystemError::NoSpace)
    }
}

impl FileSystem for FAT32FileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        Arc::new(Fat32RootInode::new(
            self.root_cluster,
            self.cluster_size(),
            Arc::clone(&self.cluster_manager),
        ))
    }

    fn create_file(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_file(name)
    }

    fn create_directory(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        parent.create_directory(name)
    }

    fn remove(&self, parent: &Arc<dyn Inode>, name: &str) -> Result<(), FileSystemError> {
        parent.remove(name)
    }

    fn stat(&self, inode: &Arc<dyn Inode>) -> Result<FileStat, FileSystemError> {
        Ok(FileStat {
            size: inode.size(),
            file_type: inode.inode_type(),
            mode: inode.mode(),
            nlink: 1,
            uid: inode.uid(),
            gid: inode.gid(),
            atime: 0,
            mtime: 0,
            ctime: 0,
        })
    }

    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}