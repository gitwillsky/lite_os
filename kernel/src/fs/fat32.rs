use alloc::{string::String, sync::Arc, vec, vec::Vec};
use spin::Mutex;

use crate::drivers::{BlockDevice, block::BlockError};

use super::{FileSystem, FileSystemError, FileStat, Inode, InodeType};

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
        println!("[FAT32] Attempting to initialize FAT32 filesystem...");
        println!("[FAT32] Device block size: {}", device.block_size());
        
        // Read full block (4096 bytes) to accommodate VirtIO block size
        let mut block_bytes = vec![0u8; device.block_size()];
        if let Err(e) = device.read_block(0, &mut block_bytes) {
            println!("[FAT32] Failed to read boot sector: {:?}", e);
            return None;
        }
        println!("[FAT32] Successfully read boot sector");
        
        // Extract the 512-byte boot sector from the full block
        let mut bpb_bytes = [0u8; SECTOR_SIZE];
        bpb_bytes.copy_from_slice(&block_bytes[..SECTOR_SIZE]);
        
        // Debug: show first few bytes of boot sector
        println!("[FAT32] Boot sector first 16 bytes: {:02x?}", &bpb_bytes[..16]);
        
        let bpb = unsafe { *(bpb_bytes.as_ptr() as *const BiosParameterBlock) };
        
        // Verify FAT32 filesystem
        let bpb_ptr = bpb_bytes.as_ptr();
        let signature = unsafe { core::ptr::read_unaligned(bpb_ptr.add(510) as *const u16) };
        println!("[FAT32] Boot signature: {:#x}", signature);
        if signature != FAT32_SIGNATURE {
            println!("[FAT32] Invalid boot signature: {:#x} (expected {:#x})", signature, FAT32_SIGNATURE);
            return None;
        }
        
        let sectors_per_fat_32 = unsafe { core::ptr::read_unaligned(bpb_ptr.add(36) as *const u32) };
        println!("[FAT32] Sectors per FAT32: {}", sectors_per_fat_32);
        if sectors_per_fat_32 == 0 {
            println!("[FAT32] Not a FAT32 filesystem (sectors_per_fat_32 is 0)");
            return None;
        }
        
        let reserved_sectors = unsafe { core::ptr::read_unaligned(bpb_ptr.add(14) as *const u16) };
        let num_fats = unsafe { core::ptr::read_unaligned(bpb_ptr.add(16) as *const u8) };
        let sectors_per_cluster = unsafe { core::ptr::read_unaligned(bpb_ptr.add(13) as *const u8) };
        let root_cluster = unsafe { core::ptr::read_unaligned(bpb_ptr.add(44) as *const u32) };
        
        let fat_start_sector = reserved_sectors as u32;
        let cluster_start_sector = fat_start_sector + (num_fats as u32 * sectors_per_fat_32);
        let sectors_per_cluster = sectors_per_cluster as u32;
        let bytes_per_cluster = sectors_per_cluster * SECTOR_SIZE as u32;
        
        println!("[FAT32] Filesystem initialized successfully");
        println!("[FAT32] Reserved sectors: {}", reserved_sectors);
        println!("[FAT32] Number of FATs: {}", num_fats);
        println!("[FAT32] Sectors per FAT: {}", sectors_per_fat_32);
        println!("[FAT32] Sectors per cluster: {}", sectors_per_cluster);
        println!("[FAT32] Bytes per cluster: {}", bytes_per_cluster);
        println!("[FAT32] Root directory cluster: {}", root_cluster);
        println!("[FAT32] Data area start sector: {}", cluster_start_sector);
        
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
            self.device.read_block(block_num as usize, &mut block_data)?;
            
            // Extract the specific sector from the block
            let sector_offset_in_block = sector_in_block as usize * SECTOR_SIZE;
            let sector_offset_in_buf = i as usize * SECTOR_SIZE;
            
            buf[sector_offset_in_buf..sector_offset_in_buf + SECTOR_SIZE]
                .copy_from_slice(&block_data[sector_offset_in_block..sector_offset_in_block + SECTOR_SIZE]);
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
            self.device.read_block(block_num as usize, &mut block_data)?;
            
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
    
    fn get_next_cluster(&self, cluster: u32) -> u32 {
        let fat_cache = self.fat_cache.lock();
        if cluster as usize >= fat_cache.len() {
            return CLUSTER_EOF;
        }
        fat_cache[cluster as usize] & 0x0FFFFFFF
    }
    
    fn read_directory_entries(&self, cluster: u32) -> Result<Vec<DirectoryEntry>, FileSystemError> {
        let mut entries = Vec::new();
        let mut current_cluster = cluster;
        
        loop {
            let mut cluster_data = vec![0u8; self.bytes_per_cluster as usize];
            self.read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;
            
            for chunk in cluster_data.chunks_exact(32) {
                let entry = unsafe { *(chunk.as_ptr() as *const DirectoryEntry) };
                
                if entry.name[0] == 0x00 {
                    // End of directory
                    return Ok(entries);
                }
                
                if entry.name[0] == 0xE5 {
                    // Deleted entry
                    continue;
                }
                
                if entry.attr & ATTR_LONG_NAME == ATTR_LONG_NAME {
                    // Long filename entry, skip for now
                    continue;
                }
                
                entries.push(entry);
            }
            
            current_cluster = self.get_next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                break;
            }
        }
        
        Ok(entries)
    }
}

impl FileSystem for FAT32FileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        Arc::new(FAT32Inode {
            fs: self as *const _ as *const FAT32FileSystem,
            cluster: self.root_cluster,
            size: 0,
            is_dir: true,
        })
    }
    
    fn create_file(&self, _parent: &Arc<dyn Inode>, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn create_directory(&self, _parent: &Arc<dyn Inode>, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn remove(&self, _parent: &Arc<dyn Inode>, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
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
    size: u64,
    is_dir: bool,
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
        self.size
    }
    
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        if self.is_dir {
            return Err(FileSystemError::IsDirectory);
        }
        
        if offset >= self.size {
            return Ok(0);
        }
        
        let read_size = (buf.len() as u64).min(self.size - offset) as usize;
        let mut current_cluster = self.cluster;
        let mut cluster_offset = offset;
        let bytes_per_cluster = self.fs().bytes_per_cluster as u64;
        
        // Skip preceding clusters
        while cluster_offset >= bytes_per_cluster {
            current_cluster = self.fs().get_next_cluster(current_cluster);
            if current_cluster >= CLUSTER_EOF {
                return Ok(0);
            }
            cluster_offset -= bytes_per_cluster;
        }
        
        let mut bytes_read = 0;
        
        while bytes_read < read_size && current_cluster < CLUSTER_EOF {
            let mut cluster_data = vec![0u8; bytes_per_cluster as usize];
            self.fs().read_cluster(current_cluster, &mut cluster_data)
                .map_err(|_| FileSystemError::IoError)?;
            
            let copy_start = cluster_offset as usize;
            let copy_size = ((bytes_per_cluster as usize - copy_start).min(read_size - bytes_read));
            
            buf[bytes_read..bytes_read + copy_size]
                .copy_from_slice(&cluster_data[copy_start..copy_start + copy_size]);
            
            bytes_read += copy_size;
            cluster_offset = 0;
            current_cluster = self.fs().get_next_cluster(current_cluster);
        }
        
        Ok(bytes_read)
    }
    
    fn write_at(&self, _offset: u64, _buf: &[u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }
        
        let entries = self.fs().read_directory_entries(self.cluster)?;
        let mut names = Vec::new();
        
        for entry in entries {
            if entry.attr & ATTR_VOLUME_ID != 0 {
                continue;
            }
            
            let name = Self::entry_name_to_string(&entry);
            if name != "." && name != ".." {
                names.push(name);
            }
        }
        
        Ok(names)
    }
    
    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !self.is_dir {
            return Err(FileSystemError::NotDirectory);
        }
        
        let entries = self.fs().read_directory_entries(self.cluster)?;
        
        for entry in entries {
            if entry.attr & ATTR_VOLUME_ID != 0 {
                continue;
            }
            
            let entry_name = Self::entry_name_to_string(&entry);
            if entry_name == name.to_lowercase() {
                let cluster = (entry.first_cluster_high as u32) << 16 | entry.first_cluster_low as u32;
                let is_dir = entry.attr & ATTR_DIRECTORY != 0;
                let size = if is_dir { 0 } else { entry.file_size as u64 };
                
                return Ok(Arc::new(FAT32Inode {
                    fs: self.fs,
                    cluster,
                    size,
                    is_dir,
                }));
            }
        }
        
        Err(FileSystemError::NotFound)
    }
    
    fn create_file(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn create_directory(&self, _name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn remove(&self, _name: &str) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn truncate(&self, _size: u64) -> Result<(), FileSystemError> {
        Err(FileSystemError::PermissionDenied)
    }
    
    fn sync(&self) -> Result<(), FileSystemError> {
        Ok(())
    }
}