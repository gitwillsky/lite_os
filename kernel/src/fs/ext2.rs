use alloc::{string::String, string::ToString, sync::{Arc, Weak}, vec, vec::Vec};
use core::{cmp, mem, ptr};
use spin::Mutex;

use crate::drivers::block::{BlockDevice, BlockError, BLOCK_SIZE};
use super::{FileStat, FileSystem, FileSystemError, Inode, InodeType};

// ===== Ext2 on-disk structures =====

const EXT2_SUPER_MAGIC: u16 = 0xEF53;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
struct Ext2SuperBlock {
    s_inodes_count: u32,
    s_blocks_count: u32,
    s_r_blocks_count: u32,
    s_free_blocks_count: u32,
    s_free_inodes_count: u32,
    s_first_data_block: u32,
    s_log_block_size: u32,
    s_log_frag_size: i32,
    s_blocks_per_group: u32,
    s_frags_per_group: u32,
    s_inodes_per_group: u32,
    s_mtime: u32,
    s_wtime: u32,
    s_mnt_count: u16,
    s_max_mnt_count: i16,
    s_magic: u16,
    s_state: u16,
    s_errors: u16,
    s_minor_rev_level: u16,
    s_lastcheck: u32,
    s_checkinterval: u32,
    s_creator_os: u32,
    s_rev_level: u32,
    s_def_resuid: u16,
    s_def_resgid: u16,
    // ext2 revision 1 fields
    s_first_ino: u32,
    s_inode_size: u16,
    s_block_group_nr: u16,
    s_feature_compat: u32,
    s_feature_incompat: u32,
    s_feature_ro_compat: u32,
    s_uuid: [u8; 16],
    s_volume_name: [u8; 16],
    s_last_mounted: [u8; 64],
    s_algorithm_usage_bitmap: u32,
    // Performance hints
    s_prealloc_blocks: u8,
    s_prealloc_dir_blocks: u8,
    s_reserved_gdt_blocks: u16,
    // Journaling support valid if EXT3_FEATURE_COMPAT_HAS_JOURNAL set
    s_journal_uuid: [u8; 16],
    s_journal_inum: u32,
    s_journal_dev: u32,
    s_last_orphan: u32,
    // Directory indexing support
    s_hash_seed: [u32; 4],
    s_def_hash_version: u8,
    s_jnl_backup_type: u8,
    s_desc_size: u16,
    s_default_mount_opts: u32,
    s_first_meta_bg: u32,
    s_mkfs_time: u32,
    s_jnl_blocks: [u32; 17],
    // 64bit support valid if EXT4_FEATURE_COMPAT_64BIT
    s_blocks_count_hi: u32,
    s_r_blocks_count_hi: u32,
    s_free_blocks_count_hi: u32,
    s_min_extra_isize: u16,
    s_want_extra_isize: u16,
    s_flags: u32,
    s_raid_stride: u16,
    s_mmp_update_interval: u16,
    s_mmp_block: u64,
    s_raid_stripe_width: u32,
    s_log_groups_per_flex: u8,
    s_checksum_type: u8,
    s_reserved_pad: u16,
    s_kbytes_written: u64,
    s_snapshot_inum: u32,
    s_snapshot_id: u32,
    s_snapshot_r_blocks_count: u64,
    s_snapshot_list: u32,
    s_error_count: u32,
    s_first_error_time: u32,
    s_first_error_ino: u32,
    s_first_error_block: u64,
    s_first_error_func: [u8; 32],
    s_first_error_line: u32,
    s_last_error_time: u32,
    s_last_error_ino: u32,
    s_last_error_line: u32,
    s_last_error_block: u64,
    s_last_error_func: [u8; 32],
    s_mount_opts: [u8; 64],
    s_usr_quota_inum: u32,
    s_grp_quota_inum: u32,
    s_overhead_clusters: u32,
    s_backup_bgs: [u32; 2],
    s_encrypt_algos: [u8; 4],
    s_encrypt_pw_salt: [u8; 16],
    s_lpf_ino: u32,
    s_prj_quota_inum: u32,
    s_checksum_seed: u32,
    s_reserved: [u32; 98],
    s_checksum: u32,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2GroupDesc {
    bg_block_bitmap: u32,
    bg_inode_bitmap: u32,
    bg_inode_table: u32,
    bg_free_blocks_count: u16,
    bg_free_inodes_count: u16,
    bg_used_dirs_count: u16,
    bg_pad: u16,
    bg_reserved: [u32; 3],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2InodeDisk {
    i_mode: u16,
    i_uid: u16,
    i_size_lo: u32,
    i_atime: u32,
    i_ctime: u32,
    i_mtime: u32,
    i_dtime: u32,
    i_gid: u16,
    i_links_count: u16,
    i_blocks_lo: u32,
    i_flags: u32,
    i_osd1: u32,
    i_block: [u32; 15],
    i_generation: u32,
    i_file_acl: u32,
    i_dir_acl_or_size_high: u32,
    i_faddr: u32,
    i_osd2: [u8; 12],
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
struct Ext2DirEntry2Header {
    inode: u32,
    rec_len: u16,
    name_len: u8,
    file_type: u8,
    // followed by name[name_len]
}

// ===== Helpers =====

fn align_up(x: usize, a: usize) -> usize { (x + a - 1) & !(a - 1) }

fn ceil_div(a: usize, b: usize) -> usize { (a + b - 1) / b }

// ===== Core Ext2 FS =====

pub struct Ext2FileSystem {
    device: Arc<dyn BlockDevice>,
    superblock: Ext2SuperBlock,
    block_size: usize,
    inode_size: usize,
    inodes_per_group: usize,
    blocks_per_group: usize,
    first_data_block: u32,
    groups: Mutex<Vec<Ext2GroupDesc>>,
    self_ref: spin::Mutex<Weak<Ext2FileSystem>>, // to upgrade &self to Arc
}

impl core::fmt::Debug for Ext2FileSystem {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ext2FileSystem")
            .field("block_size", &self.block_size)
            .field("inodes_per_group", &self.inodes_per_group)
            .field("blocks_per_group", &self.blocks_per_group)
            .field("first_data_block", &self.first_data_block)
            .finish()
    }
}

impl Ext2FileSystem {
    pub fn new(device: Arc<dyn BlockDevice>) -> Result<Arc<Self>, FileSystemError> {
        let dev_block_size = device.block_size();
        if dev_block_size != BLOCK_SIZE {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Read first device block to access superblock at offset 1024
        let mut block0 = vec![0u8; dev_block_size];
        device.read_block(0, &mut block0).map_err(|_| FileSystemError::IoError)?;
        if block0.len() < 2048 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // Superblock occupies 1024 bytes starting at 1024
        let sb_ptr = unsafe { block0.as_ptr().add(1024) as *const Ext2SuperBlock };
        let superblock = unsafe { ptr::read_unaligned(sb_ptr) };

        if superblock.s_magic != EXT2_SUPER_MAGIC {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let block_size = 1024usize << superblock.s_log_block_size;
        // Simplify implementation: require ext2 block size == device block size (4096)
        if block_size != dev_block_size {
            return Err(FileSystemError::InvalidFileSystem);
        }

        // Get inode size from superblock
        let inode_size = if superblock.s_rev_level >= 1 && superblock.s_inode_size != 0 {
            superblock.s_inode_size as usize
        } else {
            128usize // EXT2_GOOD_OLD_INODE_SIZE
        };
        
        // Validate inode size
        if inode_size < 128 || (inode_size & (inode_size - 1)) != 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }

        let blocks_per_group = superblock.s_blocks_per_group as usize;
        let inodes_per_group = superblock.s_inodes_per_group as usize;
        let first_data_block = superblock.s_first_data_block;

        // Read group descriptor table
        let gdt_start_block = if block_size == 1024 { 2 } else { 1 } as usize;
        let total_blocks = superblock.s_blocks_count as usize;
        let group_count = ceil_div(total_blocks - first_data_block as usize, blocks_per_group);
        let gdt_bytes = group_count * mem::size_of::<Ext2GroupDesc>();
        let gdt_blocks = ceil_div(gdt_bytes, block_size);

        let mut groups = Vec::with_capacity(group_count);
        let mut gdt_buf = vec![0u8; gdt_blocks * block_size];
        for i in 0..gdt_blocks {
            device.read_block(gdt_start_block + i, &mut gdt_buf[i * block_size..(i + 1) * block_size])
                .map_err(|_| FileSystemError::IoError)?;
        }
        for i in 0..group_count {
            let start = i * mem::size_of::<Ext2GroupDesc>();
            let end = start + mem::size_of::<Ext2GroupDesc>();
            let gd = unsafe { ptr::read_unaligned(gdt_buf[start..end].as_ptr() as *const Ext2GroupDesc) };
            groups.push(gd);
        }

        let fs = Arc::new(Self {
            device,
            superblock,
            block_size,
            inode_size,
            inodes_per_group,
            blocks_per_group,
            first_data_block,
            groups: Mutex::new(groups),
            self_ref: spin::Mutex::new(Weak::new()),
        });
        // set self_ref
        *fs.self_ref.lock() = Arc::downgrade(&fs);
        Ok(fs)
    }

    fn read_block(&self, block_id: u32, buf: &mut [u8]) -> Result<(), FileSystemError> {
        if buf.len() != self.block_size { return Err(FileSystemError::IoError); }
        self.device.read_block(block_id as usize, buf).map_err(|_| FileSystemError::IoError)
    }

    fn write_block(&self, block_id: u32, buf: &[u8]) -> Result<(), FileSystemError> {
        if buf.len() != self.block_size { return Err(FileSystemError::IoError); }
        self.device.write_block(block_id as usize, buf).map_err(|_| FileSystemError::IoError)
    }

    fn inode_size(&self) -> usize { self.inode_size }

    fn group_index_and_local_inode(&self, inode_num: u32) -> (usize, usize) {
        // ext2 inode numbers start at 1
        let idx = (inode_num - 1) as usize;
        let group = idx / self.inodes_per_group;
        let local = idx % self.inodes_per_group;
        (group, local)
    }

    fn read_inode_disk(&self, inode_num: u32) -> Result<Ext2InodeDisk, FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num);
        let groups = self.groups.lock();
        let gd = groups.get(group).ok_or(FileSystemError::InvalidFileSystem)?;
        let table_block = gd.bg_inode_table;
        drop(groups);
        
        let inode_size = self.inode_size();
        let inodes_per_block = self.block_size / inode_size;
        let block_offset = local / inodes_per_block;
        let offset_in_block = (local % inodes_per_block) * inode_size;

        let mut buf = vec![0u8; self.block_size];
        self.read_block(table_block + block_offset as u32, &mut buf)?;
        let ptr = unsafe { buf.as_ptr().add(offset_in_block) as *const Ext2InodeDisk };
        Ok(unsafe { ptr::read_unaligned(ptr) })
    }

    fn write_inode_disk(&self, inode_num: u32, inode: &Ext2InodeDisk) -> Result<(), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num);
        let groups = self.groups.lock();
        let gd = groups.get(group).ok_or(FileSystemError::InvalidFileSystem)?;
        let table_block = gd.bg_inode_table;
        drop(groups);
        
        let inode_size = self.inode_size();
        let inodes_per_block = self.block_size / inode_size;
        let block_offset = local / inodes_per_block;
        let offset_in_block = (local % inodes_per_block) * inode_size;

        let mut buf = vec![0u8; self.block_size];
        self.read_block(table_block + block_offset as u32, &mut buf)?;
        let dst = unsafe { buf.as_mut_ptr().add(offset_in_block) as *mut Ext2InodeDisk };
        unsafe { ptr::write_unaligned(dst, *inode) };
        self.write_block(table_block + block_offset as u32, &buf)
    }

    fn alloc_from_bitmap(&self, bitmap_block: u32, total: usize) -> Result<u32, FileSystemError> {
        if bitmap_block == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut buf = vec![0u8; self.block_size];
        self.read_block(bitmap_block, &mut buf)?;
        let max_bits = cmp::min(total, self.block_size * 8);
        for (byte_index, b) in buf.iter_mut().enumerate().take(ceil_div(max_bits, 8)) {
            if *b != 0xFF { // has free bits
                for bit in 0..8 {
                    let bit_index = byte_index * 8 + bit;
                    if bit_index >= max_bits { break; }
                    if (*b & (1u8 << bit)) == 0 {
                        *b |= 1u8 << bit;
                        self.write_block(bitmap_block, &buf)?;
                        return Ok(bit_index as u32);
                    }
                }
            }
        }
        Err(FileSystemError::NoSpace)
    }

    fn free_in_bitmap(&self, bitmap_block: u32, idx: u32) -> Result<(), FileSystemError> {
        if bitmap_block == 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let mut buf = vec![0u8; self.block_size];
        self.read_block(bitmap_block, &mut buf)?;
        let byte_index = (idx / 8) as usize;
        let bit = (idx % 8) as u8;
        if byte_index >= buf.len() { 
            return Err(FileSystemError::InvalidFileSystem); 
        }
        if (buf[byte_index] & (1u8 << bit)) == 0 {
            return Err(FileSystemError::InvalidFileSystem); // double free
        }
        buf[byte_index] &= !(1u8 << bit);
        self.write_block(bitmap_block, &buf)
    }

    fn allocate_block_in_group(&self, group: usize) -> Result<u32, FileSystemError> {
        let mut groups = self.groups.lock();
        let gd = groups.get_mut(group).ok_or(FileSystemError::InvalidFileSystem)?;
        if gd.bg_free_blocks_count == 0 {
            return Err(FileSystemError::NoSpace);
        }
        let rel = self.alloc_from_bitmap(gd.bg_block_bitmap, self.blocks_per_group)?; // relative to group
        // Update block group descriptor
        gd.bg_free_blocks_count -= 1;
        self.write_group_descriptor(group, gd)?;
        let abs = (self.first_data_block + (group as u32) * self.blocks_per_group as u32 + rel) as u32;
        Ok(abs)
    }

    fn allocate_inode_in_group(&self, group: usize) -> Result<u32, FileSystemError> {
        let mut groups = self.groups.lock();
        let gd = groups.get_mut(group).ok_or(FileSystemError::InvalidFileSystem)?;
        if gd.bg_free_inodes_count == 0 {
            return Err(FileSystemError::NoSpace);
        }
        let rel = self.alloc_from_bitmap(gd.bg_inode_bitmap, self.inodes_per_group)?; // 0-based within group
        // Update block group descriptor
        gd.bg_free_inodes_count -= 1;
        self.write_group_descriptor(group, gd)?;
        let abs = (group as u32) * self.inodes_per_group as u32 + rel + 1; // inode numbers start at 1
        Ok(abs)
    }

    fn free_block(&self, block_id: u32) -> Result<(), FileSystemError> {
        let group = ((block_id - self.first_data_block) as usize) / self.blocks_per_group;
        let rel = (block_id - self.first_data_block) as usize % self.blocks_per_group;
        let mut groups = self.groups.lock();
        let gd = groups.get_mut(group).ok_or(FileSystemError::InvalidFileSystem)?;
        self.free_in_bitmap(gd.bg_block_bitmap, rel as u32)?;
        // Update block group descriptor
        gd.bg_free_blocks_count += 1;
        self.write_group_descriptor(group, gd)
    }

    fn free_inode(&self, inode_num: u32) -> Result<(), FileSystemError> {
        let (group, local) = self.group_index_and_local_inode(inode_num);
        let mut groups = self.groups.lock();
        let gd = groups.get_mut(group).ok_or(FileSystemError::InvalidFileSystem)?;
        self.free_in_bitmap(gd.bg_inode_bitmap, local as u32)?;
        // Update block group descriptor
        gd.bg_free_inodes_count += 1;
        self.write_group_descriptor(group, gd)
    }

    fn write_group_descriptor(&self, group: usize, gd: &Ext2GroupDesc) -> Result<(), FileSystemError> {
        let gdt_start_block = if self.block_size == 1024 { 2 } else { 1 } as usize;
        let gd_per_block = self.block_size / mem::size_of::<Ext2GroupDesc>();
        let block_offset = group / gd_per_block;
        let offset_in_block = (group % gd_per_block) * mem::size_of::<Ext2GroupDesc>();
        
        let mut buf = vec![0u8; self.block_size];
        self.read_block((gdt_start_block + block_offset) as u32, &mut buf)?;
        let dst = unsafe { buf.as_mut_ptr().add(offset_in_block) as *mut Ext2GroupDesc };
        unsafe { ptr::write_unaligned(dst, *gd) };
        self.write_block((gdt_start_block + block_offset) as u32, &buf)
    }
}

// ===== Inode Implementation =====

#[derive(Debug)]
struct Ext2Inode {
    fs: Arc<Ext2FileSystem>,
    inode_num: u32,
    disk: Mutex<Ext2InodeDisk>,
}

impl Ext2Inode {
    fn load(fs: Arc<Ext2FileSystem>, inode_num: u32) -> Result<Arc<Self>, FileSystemError> {
        let disk = fs.read_inode_disk(inode_num)?;
        Ok(Arc::new(Self { fs, inode_num, disk: Mutex::new(disk) }))
    }

    fn kind_from_mode(mode: u16) -> InodeType {
        match mode & 0xF000 {
            0x4000 => InodeType::Directory,
            0xA000 => InodeType::SymLink,
            0x1000 => InodeType::Fifo,
            _ => InodeType::File,
        }
    }

    fn current_timestamp() -> u32 {
        // In real implementation, this should get actual Unix timestamp
        // For now, return a placeholder
        0
    }

    fn update_timestamps(&self, access: bool, modify: bool, change: bool) -> Result<(), FileSystemError> {
        let mut ino = self.disk.lock();
        let now = Self::current_timestamp();
        if access { ino.i_atime = now; }
        if modify { ino.i_mtime = now; }
        if change { ino.i_ctime = now; }
        self.fs.write_inode_disk(self.inode_num, &ino)
    }

    fn ensure_block_mapped(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        // Map file logical block -> physical block, allocate if absent.
        if file_block_index >= (u32::MAX / self.fs.block_size as u32) {
            return Err(FileSystemError::NoSpace); // prevent overflow
        }
        let mut ino = self.disk.lock();
        let bs = self.fs.block_size as u32;
        let ptrs_per_block = (self.fs.block_size / 4) as u32;

        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            if b != 0 { return Ok(b); }
            drop(ino);
            let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
            let new_b = self.fs.allocate_block_in_group(group)?;
            let mut ino2 = self.disk.lock();
            ino2.i_block[file_block_index as usize] = new_b;
            self.fs.write_inode_disk(self.inode_num, &ino2)?;
            return Ok(new_b);
        }

        // Single indirect only (sufficient for our use cases)
        let idx = file_block_index - 12;
        if idx < ptrs_per_block {
            let mut ind = ino.i_block[12];
            if ind == 0 {
                drop(ino);
                let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
                ind = self.fs.allocate_block_in_group(group)?;
                let mut ino2 = self.disk.lock();
                ino2.i_block[12] = ind;
                self.fs.write_inode_disk(self.inode_num, &ino2)?;
                // zero the newly allocated indirect block
                let mut z = vec![0u8; self.fs.block_size];
                self.fs.write_block(ind, &z)?;
                drop(ino2);
                ino = self.disk.lock();
            }
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_block(ind, &mut buf)?;
            if (idx as usize * 4) + 4 > buf.len() {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let p = unsafe { (buf.as_mut_ptr() as *mut u32).add(idx as usize) };
            let mut b = unsafe { ptr::read_unaligned(p) };
            if b == 0 {
                drop(ino);
                let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
                b = self.fs.allocate_block_in_group(group)?;
                let mut buf2 = buf;
                unsafe { ptr::write_unaligned((buf2.as_mut_ptr() as *mut u32).add(idx as usize), b); }
                self.fs.write_block(ind, &buf2)?;
                let mut ino3 = self.disk.lock();
                // update on-disk inode (size/blocks updated by caller when writing)
                self.fs.write_inode_disk(self.inode_num, &ino3)?;
                return Ok(b);
            }
            return Ok(b);
        }

        Err(FileSystemError::NoSpace) // not supporting double/triple indirect for now
    }

    fn map_block(&self, file_block_index: u32) -> Result<u32, FileSystemError> {
        let ino = self.disk.lock();
        let ptrs_per_block = (self.fs.block_size / 4) as u32;
        if file_block_index < 12 {
            let b = ino.i_block[file_block_index as usize];
            if b == 0 { return Err(FileSystemError::NotFound); }
            return Ok(b);
        }
        let idx = file_block_index - 12;
        if idx < ptrs_per_block {
            let ind = ino.i_block[12];
            if ind == 0 { return Err(FileSystemError::NotFound); }
            drop(ino);
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_block(ind, &mut buf)?;
            if (idx as usize * 4) + 4 > buf.len() {
                return Err(FileSystemError::InvalidFileSystem);
            }
            let p = unsafe { (buf.as_ptr() as *const u32).add(idx as usize) };
            let b = unsafe { ptr::read_unaligned(p) };
            if b == 0 { return Err(FileSystemError::NotFound); }
            return Ok(b);
        }
        Err(FileSystemError::NotFound)
    }

    fn dir_iterate_blocks<F: FnMut(Ext2DirEntry2Header, &[u8]) -> bool>(&self, mut f: F) -> Result<(), FileSystemError> {
        let ino = self.disk.lock();
        let size = ino.i_size_lo as usize;
        drop(ino);
        let mut offset = 0usize;
        while offset < size {
            let blk_index = (offset / self.fs.block_size) as u32;
            let blk_off = offset % self.fs.block_size;
            let blk = self.map_block(blk_index).map_err(|_| FileSystemError::IoError)?;
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_block(blk, &mut buf)?;

            let mut pos = blk_off;
            while pos < self.fs.block_size {
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size { break; }
                let hdr = unsafe { ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header) };
                if hdr.rec_len == 0 { break; }
                let name_len = hdr.name_len as usize;
                let rec_len = hdr.rec_len as usize;
                let name_start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if name_start + name_len > self.fs.block_size { break; }
                let name_bytes = &buf[name_start..name_start + name_len];
                if !f(hdr, name_bytes) { return Ok(()); }
                pos += rec_len;
                if rec_len == 0 { break; }
            }
            offset = (blk_index as usize + 1) * self.fs.block_size;
        }
        Ok(())
    }

    fn add_dir_entry(&self, child_inode: u32, name: &str, file_type: u8) -> Result<(), FileSystemError> {
        let name_bytes = name.as_bytes();
        let needed = align_up(mem::size_of::<Ext2DirEntry2Header>() + name_bytes.len(), 4);
        let mut blk_index = 0u32;
        loop {
            // try to find space in current block
            let blk = match self.map_block(blk_index) { Ok(b) => b, Err(_) => {
                // need allocate new directory block
                let newb = self.ensure_block_mapped(blk_index)?;
                let mut z = vec![0u8; self.fs.block_size];
                self.fs.write_block(newb, &z)?;
                newb
            }};
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_block(blk, &mut buf)?;

            // scan entries to find tail room
            let mut pos = 0usize;
            while pos < self.fs.block_size {
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size { break; }
                let mut hdr = unsafe { ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header) };
                if hdr.rec_len == 0 { break; }
                let ideal = align_up(mem::size_of::<Ext2DirEntry2Header>() + hdr.name_len as usize, 4);
                let spare = (hdr.rec_len as usize).saturating_sub(ideal);
                if hdr.inode != 0 && spare >= needed {
                    // shrink current to ideal and insert new after it
                    hdr.rec_len = ideal as u16;
                    unsafe { ptr::write_unaligned(buf[pos..].as_mut_ptr() as *mut Ext2DirEntry2Header, hdr); }

                    let new_pos = pos + ideal;
                    let mut new_hdr = Ext2DirEntry2Header { inode: child_inode, rec_len: (spare as u16), name_len: name_bytes.len() as u8, file_type };
                    unsafe { ptr::write_unaligned(buf[new_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header, new_hdr); }
                    let name_dst = new_pos + mem::size_of::<Ext2DirEntry2Header>();
                    buf[name_dst..name_dst + name_bytes.len()].copy_from_slice(name_bytes);
                    self.fs.write_block(blk, &buf)?;

                    // update directory size if needed
                    let mut ino = self.disk.lock();
                    let new_size = cmp::max(ino.i_size_lo as usize, (blk_index as usize + 1) * self.fs.block_size);
                    ino.i_size_lo = new_size as u32;
                    self.fs.write_inode_disk(self.inode_num, &ino)?;
                    return Ok(());
                }
                pos += hdr.rec_len as usize;
            }

            // If we get here, no space in this block. Move to next
            blk_index += 1;
            if (blk_index as usize) * self.fs.block_size > 16 * 1024 * 1024 { // prevent infinite loop
                return Err(FileSystemError::NoSpace);
            }
        }
    }
}

impl Inode for Ext2Inode {
    fn inode_type(&self) -> InodeType {
        let ino = self.disk.lock();
        Self::kind_from_mode(ino.i_mode)
    }

    fn size(&self) -> u64 {
        let ino = self.disk.lock();
        ino.i_size_lo as u64
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, FileSystemError> {
        let mut done = 0usize;
        let ino = self.disk.lock();
        let size = ino.i_size_lo as usize;
        drop(ino);
        if offset as usize >= size { return Ok(0); }
        let to_read = cmp::min(buf.len(), size - offset as usize);
        if to_read == 0 { return Ok(0); }
        let bs = self.fs.block_size;
        let mut cur_off = offset as usize;
        while done < to_read {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let blk = self.map_block(blk_index).map_err(|_| FileSystemError::IoError)?;
            let mut b = vec![0u8; bs];
            self.fs.read_block(blk, &mut b)?;
            let n = cmp::min(bs - blk_off, to_read - done);
            buf[done..done + n].copy_from_slice(&b[blk_off..blk_off + n]);
            done += n;
            cur_off += n;
        }
        // Update access time
        self.update_timestamps(true, false, false).ok();
        Ok(done)
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, FileSystemError> {
        if matches!(self.inode_type(), InodeType::Directory) { 
            return Err(FileSystemError::IsDirectory); 
        }
        if buf.is_empty() {
            return Ok(0);
        }
        let mut done = 0usize;
        let bs = self.fs.block_size;
        let mut cur_off = offset as usize;
        while done < buf.len() {
            let blk_index = (cur_off / bs) as u32;
            let blk_off = cur_off % bs;
            let blk = self.ensure_block_mapped(blk_index)?;
            let mut b = vec![0u8; bs];
            if blk_off != 0 || (buf.len() - done) < bs { // partial block needs read-modify-write
                self.fs.read_block(blk, &mut b)?;
            }
            let n = cmp::min(bs - blk_off, buf.len() - done);
            b[blk_off..blk_off + n].copy_from_slice(&buf[done..done + n]);
            self.fs.write_block(blk, &b)?;
            done += n;
            cur_off += n;
        }
        // update size and blocks
        let mut ino = self.disk.lock();
        let new_size = cmp::max(ino.i_size_lo as usize, offset as usize + done);
        ino.i_size_lo = new_size as u32;
        // i_blocks is in 512-byte sectors for ext2; approximate
        let blocks_512 = ceil_div(new_size, 512);
        ino.i_blocks_lo = (blocks_512 as u32);
        // Update timestamps
        let now = Self::current_timestamp();
        ino.i_mtime = now;
        ino.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &ino)?;
        Ok(done)
    }

    fn list_dir(&self) -> Result<Vec<String>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) { return Err(FileSystemError::NotDirectory); }
        let mut out = Vec::new();
        self.dir_iterate_blocks(|hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes.len() > 0 {
                if let Ok(name) = core::str::from_utf8(name_bytes) {
                    if name != "." && name != ".." { out.push(name.to_string()); }
                }
            }
            true
        })?;
        Ok(out)
    }

    fn find_child(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) { return Err(FileSystemError::NotDirectory); }
        let mut found: Option<u32> = None;
        self.dir_iterate_blocks(|hdr, name_bytes| {
            if hdr.inode != 0 && name_bytes == name.as_bytes() {
                found = Some(hdr.inode);
                return false;
            }
            true
        })?;
        if let Some(ino) = found { return Ext2Inode::load(self.fs.clone(), ino).map(|x| x as Arc<dyn Inode>); }
        Err(FileSystemError::NotFound)
    }

    fn create_file(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) { 
            return Err(FileSystemError::NotDirectory); 
        }
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // allocate inode in same group as parent inode
        let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
        let child_ino_num = self.fs.allocate_inode_in_group(group)?;
        let now = Self::current_timestamp();
        let mut disk = Ext2InodeDisk { 
            i_mode: 0o100644,
            i_links_count: 1,
            i_atime: now,
            i_mtime: now,
            i_ctime: now,
            ..Default::default() 
        };
        // regular file: 0x8000 flag
        disk.i_mode |= 0x8000;
        self.fs.write_inode_disk(child_ino_num, &disk)?;
        // add directory entry
        self.add_dir_entry(child_ino_num, name, 1)?; // file_type 1 = regular
        // Update parent directory mtime and ctime
        self.update_timestamps(false, true, true).ok();
        Ext2Inode::load(self.fs.clone(), child_ino_num).map(|x| x as Arc<dyn Inode>)
    }

    fn create_directory(&self, name: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) { 
            return Err(FileSystemError::NotDirectory); 
        }
        if name.is_empty() || name.len() > 255 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let (group, _) = self.fs.group_index_and_local_inode(self.inode_num);
        let child_ino_num = self.fs.allocate_inode_in_group(group)?;
        let now = Self::current_timestamp();
        let mut disk = Ext2InodeDisk { 
            i_mode: 0o040755, 
            i_links_count: 2,
            i_atime: now,
            i_mtime: now,
            i_ctime: now,
            ..Default::default() 
        };
        disk.i_mode |= 0x4000; // dir flag
        // allocate first data block for '.' and '..'
        let blk0 = self.fs.allocate_block_in_group(group)?;
        disk.i_block[0] = blk0;
        disk.i_size_lo = self.fs.block_size as u32;
        disk.i_blocks_lo = (self.fs.block_size / 512) as u32;
        self.fs.write_inode_disk(child_ino_num, &disk)?;
        // write '.' and '..'
        let mut buf = vec![0u8; self.fs.block_size];
        // '.' entry
        let dot_name = b".";
        let mut dot = Ext2DirEntry2Header { inode: child_ino_num, rec_len: 0, name_len: 1, file_type: 2 };
        let dot_len = align_up(mem::size_of::<Ext2DirEntry2Header>() + dot_name.len(), 4);
        dot.rec_len = dot_len as u16;
        unsafe { ptr::write_unaligned(buf.as_mut_ptr() as *mut Ext2DirEntry2Header, dot); }
        buf[mem::size_of::<Ext2DirEntry2Header>()..mem::size_of::<Ext2DirEntry2Header>() + 1].copy_from_slice(dot_name);
        // '..' entry
        let dotdot_name = b"..";
        let mut dotdot = Ext2DirEntry2Header { inode: self.inode_num, rec_len: (self.fs.block_size - dot_len) as u16, name_len: 2, file_type: 2 };
        let off2 = dot_len;
        unsafe { ptr::write_unaligned(buf[off2..].as_mut_ptr() as *mut Ext2DirEntry2Header, dotdot); }
        let name_off2 = off2 + mem::size_of::<Ext2DirEntry2Header>();
        buf[name_off2..name_off2 + 2].copy_from_slice(dotdot_name);
        self.fs.write_block(blk0, &buf)?;
        // add entry under parent
        self.add_dir_entry(child_ino_num, name, 2)?;
        // Update parent directory link count and timestamps
        let mut parent_ino = self.disk.lock();
        parent_ino.i_links_count += 1; // '..' link from new directory
        parent_ino.i_mtime = now;
        parent_ino.i_ctime = now;
        self.fs.write_inode_disk(self.inode_num, &parent_ino)?;
        drop(parent_ino);
        Ext2Inode::load(self.fs.clone(), child_ino_num).map(|x| x as Arc<dyn Inode>)
    }

    fn remove(&self, name: &str) -> Result<(), FileSystemError> {
        if !matches!(self.inode_type(), InodeType::Directory) { return Err(FileSystemError::NotDirectory); }
        // find child and its entry to remove
        let mut target: Option<(u32, u32, usize, usize)> = None; // (ino, block, prev_pos, cur_pos)
        self.dir_iterate_blocks(|hdr, _name| {
            // We need positions; this helper doesn't pass positions. So do a manual second pass below.
            true
        })?;
        // Manual pass to remove entry
        let mut blk_index = 0u32;
        loop {
            let blk = match self.map_block(blk_index) { Ok(b) => b, Err(_) => break };
            let mut buf = vec![0u8; self.fs.block_size];
            self.fs.read_block(blk, &mut buf)?;
            let mut pos = 0usize;
            let mut prev_pos: Option<usize> = None;
            while pos < self.fs.block_size {
                if pos + mem::size_of::<Ext2DirEntry2Header>() > self.fs.block_size { break; }
                let hdr = unsafe { ptr::read_unaligned(buf[pos..].as_ptr() as *const Ext2DirEntry2Header) };
                if hdr.rec_len == 0 { break; }
                let name_len = hdr.name_len as usize;
                let rec_len = hdr.rec_len as usize;
                let name_start = pos + mem::size_of::<Ext2DirEntry2Header>();
                if name_start + name_len > self.fs.block_size { break; }
                let name_bytes = &buf[name_start..name_start + name_len];
                if hdr.inode != 0 && name_bytes == name.as_bytes() {
                    target = Some((hdr.inode, blk, prev_pos.unwrap_or(pos), pos));
                    break;
                }
                prev_pos = Some(pos);
                pos += rec_len;
            }
            if target.is_some() { break; }
            blk_index += 1;
        }
        let (child_ino, blk, prev_pos, cur_pos) = target.ok_or(FileSystemError::NotFound)?;

        // Merge current entry into previous by extending rec_len
        let mut buf = vec![0u8; self.fs.block_size];
        self.fs.read_block(blk, &mut buf)?;
        if prev_pos == cur_pos {
            // first entry in block: mark as empty
            let mut hdr: Ext2DirEntry2Header = unsafe { ptr::read_unaligned(buf[cur_pos..].as_ptr() as *const Ext2DirEntry2Header) };
            hdr.inode = 0;
            unsafe { ptr::write_unaligned(buf[cur_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header, hdr) };
        } else {
            let mut prev_hdr: Ext2DirEntry2Header = unsafe { ptr::read_unaligned(buf[prev_pos..].as_ptr() as *const Ext2DirEntry2Header) };
            let cur_hdr: Ext2DirEntry2Header = unsafe { ptr::read_unaligned(buf[cur_pos..].as_ptr() as *const Ext2DirEntry2Header) };
            prev_hdr.rec_len = (prev_hdr.rec_len as usize + cur_hdr.rec_len as usize) as u16;
            unsafe { ptr::write_unaligned(buf[prev_pos..].as_mut_ptr() as *mut Ext2DirEntry2Header, prev_hdr) };
        }
        self.fs.write_block(blk, &buf)?;

        // free child's blocks and inode
        let child_disk = self.fs.read_inode_disk(child_ino)?;
        for i in 0..12 {
            let b = child_disk.i_block[i]; if b != 0 { let _ = self.fs.free_block(b); }
        }
        if child_disk.i_block[12] != 0 {
            // free pointers inside single indirect then the block itself
            let mut ibuf = vec![0u8; self.fs.block_size];
            self.fs.read_block(child_disk.i_block[12], &mut ibuf)?;
            for i in 0..(self.fs.block_size / 4) {
                let p = unsafe { ptr::read_unaligned((ibuf.as_ptr() as *const u32).add(i)) };
                if p != 0 { let _ = self.fs.free_block(p); }
            }
            let _ = self.fs.free_block(child_disk.i_block[12]);
        }
        // clear inode
        let zero = Ext2InodeDisk::default();
        self.fs.write_inode_disk(child_ino, &zero)?;
        self.fs.free_inode(child_ino)?;
        Ok(())
    }

    fn truncate(&self, new_size: u64) -> Result<(), FileSystemError> {
        if matches!(self.inode_type(), InodeType::Directory) { return Err(FileSystemError::IsDirectory); }
        let bs = self.fs.block_size as u64;
        let mut ino = self.disk.lock();
        let old_size = ino.i_size_lo as u64;
        if new_size >= old_size { return Ok(()); }
        let old_blocks = ceil_div(old_size as usize, self.fs.block_size);
        let new_blocks = ceil_div(new_size as usize, self.fs.block_size);
        // free blocks beyond new_blocks
        for i in new_blocks..old_blocks {
            if i < 12 {
                let b = ino.i_block[i]; if b != 0 { let _ = self.fs.free_block(b); ino.i_block[i] = 0; }
            } else {
                let idx = i - 12;
                let ind = ino.i_block[12];
                if ind != 0 {
                    let mut ibuf = vec![0u8; self.fs.block_size];
                    self.fs.read_block(ind, &mut ibuf)?;
                    unsafe { ptr::write_unaligned((ibuf.as_mut_ptr() as *mut u32).add(idx), 0u32); }
                    self.fs.write_block(ind, &ibuf)?;
                }
            }
        }
        ino.i_size_lo = new_size as u32;
        ino.i_blocks_lo = ((ceil_div(new_size as usize, 512)) as u32);
        self.fs.write_inode_disk(self.inode_num, &ino)?;
        Ok(())
    }

    fn sync(&self) -> Result<(), FileSystemError> { Ok(()) }

    fn mode(&self) -> u32 { self.disk.lock().i_mode as u32 }
    fn set_mode(&self, mode: u32) -> Result<(), FileSystemError> { let mut i = self.disk.lock(); i.i_mode = mode as u16; self.fs.write_inode_disk(self.inode_num, &i)?; Ok(()) }
    fn uid(&self) -> u32 { self.disk.lock().i_uid as u32 }
    fn set_uid(&self, uid: u32) -> Result<(), FileSystemError> { let mut i = self.disk.lock(); i.i_uid = uid as u16; self.fs.write_inode_disk(self.inode_num, &i)?; Ok(()) }
    fn gid(&self) -> u32 { self.disk.lock().i_gid as u32 }
    fn set_gid(&self, gid: u32) -> Result<(), FileSystemError> { let mut i = self.disk.lock(); i.i_gid = gid as u16; self.fs.write_inode_disk(self.inode_num, &i)?; Ok(()) }
}

impl FileSystem for Ext2FileSystem {
    fn root_inode(&self) -> Arc<dyn Inode> {
        let fs_arc = self.self_ref.lock().upgrade().expect("Ext2 FS self Arc missing");
        Ext2Inode::load(fs_arc, 2).unwrap() as Arc<dyn Inode>
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

    fn sync(&self) -> Result<(), FileSystemError> { Ok(()) }
}


