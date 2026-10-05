use alloc::{sync::Arc, vec::Vec};

use super::allocation_dirty::AllocationDirty;
use super::journal_layout::JournalLayout;
use super::*;
use crate::fallible_tree::FallibleMap;
use crate::memory::PAGE_SIZE;
use crate::sync::TaskMutexGuard;

#[path = "journal/codec.rs"]
mod codec;
use codec::*;
#[path = "journal/commit_owner.rs"]
mod commit_owner;
pub(super) use commit_owner::JournalOwner;
#[path = "journal/inode_mutation.rs"]
mod inode_mutation;
use inode_mutation::InodeMutation;

const JBD2_MAGIC: u32 = 0xC03B_3998;
const JBD2_DESCRIPTOR_BLOCK: u32 = 1;
const JBD2_COMMIT_BLOCK: u32 = 2;
const JBD2_SUPERBLOCK_V2: u32 = 4;
const JBD2_FLAG_ESCAPE: u32 = 1;
const JBD2_FLAG_SAME_UUID: u32 = 2;
const JBD2_FLAG_LAST_TAG: u32 = 8;
const JBD2_FEATURE_INCOMPAT_64BIT: u32 = 0x2;
const JBD2_FEATURE_INCOMPAT_CSUM_V3: u32 = 0x10;
/// ext4 `metadata_csum` + `64bit` 下 Linux 为 journal 设置的唯一 feature 组合。
const JBD2_PROFILE_INCOMPAT: u32 = JBD2_FEATURE_INCOMPAT_64BIT | JBD2_FEATURE_INCOMPAT_CSUM_V3;
const JBD2_CRC32C_CHKSUM: u8 = 4;
/// `journal_superblock_t` 覆盖 journal block 0 的前 1024 byte。
const JBD2_SUPERBLOCK_SIZE: usize = 1024;
const JBD2_SUPERBLOCK_CHECKSUM_OFFSET: usize = 0xFC;
const JBD2_TAG3_SIZE: usize = 16;
const JBD2_UUID_SIZE: usize = 16;
const JBD2_BLOCK_TAIL_SIZE: usize = 4;
const JBD2_COMMIT_CHECKSUM_OFFSET: usize = 16;
const JBD2_COMMIT_SECONDS_OFFSET: usize = 48;
const JBD2_COMMIT_NANOSECONDS_OFFSET: usize = 56;
// rename is the widest current mutation: old/new parent, moved inode, replacement inode.
const MAX_LIVE_INODE_UNDOS: usize = 4;
const _: () = assert!(core::mem::size_of::<Option<(Arc<Ext4Inode>, Ext4InodeDisk)>>() == 264);

/// 标准 JBD2 journal inode 的单事务 redo-log owner（CSUM_V3 + 64BIT）。
pub(super) struct Journal {
    blocks: Vec<u64>,
    superblock: Vec<u8>,
    // OWNER: Journal 缓存由已验证 block count/size 唯一推导的 immutable layout；
    // 过高会让 commit 越界，过低只会提前拆分 transaction。
    layout: JournalLayout,
    /// Linux `j_csum_seed`：crc32c(!0, journal UUID)。
    checksum_seed: u32,
    sequence: u32,
    active: Option<ActiveTransaction>,
    failed: bool,
}

/// 已提交事务按日志顺序的 (home block, image) 列表。
type ReplaySet = Vec<(u64, Vec<u8>)>;

struct ActiveTransaction {
    writes: FallibleMap<u64, Vec<u8>>,
    allocation_dirty: AllocationDirty,
}

/// Linux `jbd2_superblock_csum`：checksum 字段按零参与，覆盖完整 1024-byte superblock。
fn superblock_checksum(superblock: &[u8]) -> u32 {
    let mut bytes = [0u8; JBD2_SUPERBLOCK_SIZE];
    bytes.copy_from_slice(&superblock[..JBD2_SUPERBLOCK_SIZE]);
    bytes[JBD2_SUPERBLOCK_CHECKSUM_OFFSET..JBD2_SUPERBLOCK_CHECKSUM_OFFSET + 4].fill(0);
    checksum::crc32c(!0, &bytes)
}

/// 把 `offset..offset + 4` 视为零后计算整个 journal block 的 checksum。
fn block_checksum(seed: u32, block: &[u8], offset: usize) -> u32 {
    let crc = checksum::crc32c(seed, &block[..offset]);
    let crc = checksum::crc32c(crc, &[0; 4]);
    checksum::crc32c(crc, &block[offset + 4..])
}

/// Linux `jbd2_block_tag_csum_set`（v3）：seed 混入 be32 sequence 与 journal 中的 data image。
fn tag_checksum(seed: u32, sequence: u32, data: &[u8]) -> u32 {
    checksum::crc32c(checksum::crc32c(seed, &sequence.to_be_bytes()), data)
}

impl Journal {
    /// 从固定 journal inode 加载并验证 JBD2 v2 superblock 与 extent mapping。
    ///
    /// # Errors
    ///
    /// journal inode、mapping、layout、feature、checksum 或 I/O 无效时拒绝挂载。
    pub(super) fn load(fs: &Arc<Ext4FileSystem>) -> Result<Self, FileSystemError> {
        let journal_inode = fs.superblock.lock().s_journal_inum;
        if journal_inode != EXT4_JOURNAL_INO {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let inode = Ext4Inode::load(fs.clone(), journal_inode)?;
        if inode.inode_type() != InodeType::File {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let logical_blocks = usize::try_from(inode.size())
            .map_err(|_| FileSystemError::InvalidFileSystem)?
            / fs.block_size;
        let mut blocks = Vec::new();
        blocks
            .try_reserve_exact(logical_blocks)
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for index in 0..logical_blocks {
            blocks.push(inode.map_block(index as u32)?);
        }
        let mut superblock = zeroed(fs.block_size)?;
        fs.read_fs_block_home(blocks[0], &mut superblock)?;
        if be32(&superblock, 0)? != JBD2_MAGIC
            || be32(&superblock, 4)? != JBD2_SUPERBLOCK_V2
            || be32(&superblock, 12)? as usize != fs.block_size
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let maximum = be32(&superblock, 16)? as usize;
        let first = be32(&superblock, 20)? as usize;
        if maximum > blocks.len() || first != 1 || maximum < 4 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        // compat 与 ro_compat 必须为空；incompat 只允许 profile 组合。mkfs 新建的空 journal
        // 尚无 feature，首次 mount 的 recover 会升级；需要 replay 的 journal 必须已是 CSUM_V3。
        let incompat = be32(&superblock, 40)?;
        if be32(&superblock, 36)? != 0
            || be32(&superblock, 44)? != 0
            || incompat & !JBD2_PROFILE_INCOMPAT != 0
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        let start = be32(&superblock, 28)?;
        if incompat == JBD2_PROFILE_INCOMPAT {
            if superblock[80] != JBD2_CRC32C_CHKSUM
                || be32(&superblock, JBD2_SUPERBLOCK_CHECKSUM_OFFSET)?
                    != superblock_checksum(&superblock)
            {
                error!("journal superblock checksum mismatch");
                return Err(FileSystemError::InvalidFileSystem);
            }
        } else if incompat != 0 || start != 0 {
            return Err(FileSystemError::InvalidFileSystem);
        }
        blocks.truncate(maximum);
        let layout = JournalLayout::new(blocks.len(), fs.block_size)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        let checksum_seed = checksum::crc32c(!0, &superblock[48..48 + JBD2_UUID_SIZE]);
        let sequence = be32(&superblock, 24)?;
        Ok(Self {
            blocks,
            superblock,
            layout,
            checksum_seed,
            sequence,
            active: None,
            failed: false,
        })
    }

    /// 读取 active transaction 中覆盖指定 home block 的最新 staged bytes。
    pub(super) fn copy_staged(&self, block: u64, output: &mut [u8]) -> bool {
        let Some(bytes) = self
            .active
            .as_ref()
            .and_then(|active| active.writes.get(&block))
        else {
            return false;
        };
        output.copy_from_slice(bytes);
        true
    }

    /// 把一次完整 home-block image 去重加入 active redo write-set。
    ///
    /// # Errors
    ///
    /// journal aborted、无 active transaction、容量耗尽或 block size 不匹配时返回错误。
    pub(super) fn stage(
        &mut self,
        block: u64,
        bytes: &[u8],
        block_size: usize,
    ) -> Result<(), FileSystemError> {
        if self.failed || bytes.len() != block_size {
            return Err(FileSystemError::IoError);
        }
        let writes = &mut self
            .active
            .as_mut()
            .ok_or(FileSystemError::InvalidOperation)?
            .writes;
        if let Some(image) = writes.get_mut(&block) {
            image.copy_from_slice(bytes);
            return Ok(());
        }
        if writes.len() >= test_stage_capacity(self.layout.write_capacity()) {
            return Err(FileSystemError::NoSpace);
        }
        let mut image = Vec::new();
        image
            .try_reserve_exact(bytes.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        image.extend_from_slice(bytes);
        let entry =
            FallibleMap::try_prepare(block, image).map_err(|_| FileSystemError::OutOfMemory)?;
        writes.commit_vacant(entry);
        Ok(())
    }

    fn begin(&mut self, group_count: usize) -> Result<(), FileSystemError> {
        if self.failed {
            return Err(FileSystemError::IoError);
        }
        if self.active.is_some() {
            return Err(FileSystemError::InvalidOperation);
        }
        self.active = Some(ActiveTransaction {
            writes: FallibleMap::new(),
            allocation_dirty: AllocationDirty::try_new(group_count)?,
        });
        Ok(())
    }

    pub(super) fn mark_allocation_dirty(&mut self, group: usize) -> Result<(), FileSystemError> {
        self.active
            .as_mut()
            .ok_or(FileSystemError::InvalidOperation)?
            .allocation_dirty
            .mark(group)
    }

    fn take_allocation_dirty(&mut self) -> Result<AllocationDirty, FileSystemError> {
        let active = self
            .active
            .as_mut()
            .ok_or(FileSystemError::InvalidOperation)?;
        Ok(core::mem::replace(
            &mut active.allocation_dirty,
            AllocationDirty::empty(),
        ))
    }

    fn abort(&mut self, fs: &Ext4FileSystem) {
        if let Some(writes) = &self.active {
            let mut cache = fs.metadata_cache.lock();
            for (block, _) in &writes.writes {
                cache.invalidate(*block);
            }
        }
        self.active = None;
    }

    fn journal_read(
        &self,
        fs: &Ext4FileSystem,
        logical: usize,
        bytes: &mut [u8],
    ) -> Result<(), FileSystemError> {
        let block = *self
            .blocks
            .get(logical)
            .ok_or(FileSystemError::InvalidFileSystem)?;
        fs.read_fs_block_home(block, bytes)
    }

    fn journal_write(
        &self,
        fs: &Ext4FileSystem,
        logical: usize,
        bytes: &[u8],
    ) -> Result<(), FileSystemError> {
        record_test_journal_write();
        let block = *self.blocks.get(logical).ok_or(FileSystemError::NoSpace)?;
        fs.write_fs_block_home(block, bytes)
    }

    /// 写入 sequence/start，并以 profile feature 与 checksum 重新封装 journal superblock。
    fn write_state(
        &mut self,
        fs: &Ext4FileSystem,
        start: u32,
        sequence: u32,
    ) -> Result<(), FileSystemError> {
        put_be32(&mut self.superblock, 24, sequence)?;
        put_be32(&mut self.superblock, 28, start)?;
        put_be32(&mut self.superblock, 40, JBD2_PROFILE_INCOMPAT)?;
        self.superblock[80] = JBD2_CRC32C_CHKSUM;
        let checksum = superblock_checksum(&self.superblock);
        put_be32(
            &mut self.superblock,
            JBD2_SUPERBLOCK_CHECKSUM_OFFSET,
            checksum,
        )?;
        self.journal_write(fs, 0, &self.superblock)
    }

    /// 按 journal superblock sequence 扫描并重放唯一已提交未 checkpoint 事务。
    ///
    /// descriptor 或 commit checksum 不符视为事务未提交；已提交事务的 data tag checksum
    /// 不符则拒绝挂载，避免部分 replay。未提交事务的 data slot 可能仍是上一事务的 image
    /// （crash 发生在 descriptor 与 data durable 之间），因此 tag checksum 只在找到有效 commit
    /// 后裁决，与 Linux `do_one_pass` 只在 PASS_REPLAY 校验 tag 一致。
    pub(super) fn recover(&mut self, fs: &Ext4FileSystem) -> Result<(), FileSystemError> {
        let start = be32(&self.superblock, 28)? as usize;
        let sequence = be32(&self.superblock, 24)?;
        if start != 0 {
            let replay = self.scan_committed(fs, start, sequence)?;
            if let Some(replay) = replay {
                for (block, bytes) in replay {
                    fs.write_fs_block_home(block, &bytes)?;
                }
                fs.device.flush().map_err(block_error)?;
            }
        }
        self.sequence = sequence.wrapping_add(1);
        self.write_state(fs, 0, self.sequence)?;
        fs.device.flush().map_err(block_error)
    }

    fn scan_committed(
        &self,
        fs: &Ext4FileSystem,
        start: usize,
        sequence: u32,
    ) -> Result<Option<ReplaySet>, FileSystemError> {
        let tail = fs.block_size - JBD2_BLOCK_TAIL_SIZE;
        let mut cursor = start;
        let mut replay = Vec::new();
        let mut corrupt_block = None;
        loop {
            let mut header = zeroed(fs.block_size)?;
            self.journal_read(fs, cursor, &mut header)?;
            if be32(&header, 0)? != JBD2_MAGIC || be32(&header, 8)? != sequence {
                return Ok(None);
            }
            match be32(&header, 4)? {
                JBD2_DESCRIPTOR_BLOCK => {
                    if be32(&header, tail)? != block_checksum(self.checksum_seed, &header, tail) {
                        return Ok(None);
                    }
                    cursor += 1;
                    let mut offset = 12;
                    loop {
                        if offset + JBD2_TAG3_SIZE > tail {
                            return Err(FileSystemError::InvalidFileSystem);
                        }
                        let home = u64::from(be32(&header, offset + 8)?) << 32
                            | u64::from(be32(&header, offset)?);
                        let flags = be32(&header, offset + 4)?;
                        let expected = be32(&header, offset + 12)?;
                        offset += JBD2_TAG3_SIZE;
                        if flags & JBD2_FLAG_SAME_UUID == 0 {
                            offset += JBD2_UUID_SIZE;
                        }
                        let mut data = zeroed(fs.block_size)?;
                        self.journal_read(fs, cursor, &mut data)?;
                        if corrupt_block.is_none()
                            && tag_checksum(self.checksum_seed, sequence, &data) != expected
                        {
                            corrupt_block = Some(cursor);
                        }
                        if flags & JBD2_FLAG_ESCAPE != 0 {
                            data[..4].copy_from_slice(&JBD2_MAGIC.to_be_bytes());
                        }
                        replay
                            .try_reserve(1)
                            .map_err(|_| FileSystemError::OutOfMemory)?;
                        replay.push((home, data));
                        cursor += 1;
                        if flags & JBD2_FLAG_LAST_TAG != 0 {
                            break;
                        }
                    }
                }
                JBD2_COMMIT_BLOCK => {
                    let committed = be32(&header, JBD2_COMMIT_CHECKSUM_OFFSET)?
                        == block_checksum(self.checksum_seed, &header, JBD2_COMMIT_CHECKSUM_OFFSET);
                    if !committed {
                        return Ok(None);
                    }
                    if let Some(cursor) = corrupt_block {
                        error!("journal block {cursor} checksum mismatch");
                        return Err(FileSystemError::InvalidFileSystem);
                    }
                    return Ok(Some(replay));
                }
                _ => return Ok(None),
            }
            if cursor >= self.blocks.len() {
                return Ok(None);
            }
        }
    }

    fn commit_inner(
        &mut self,
        fs: &Ext4FileSystem,
        writes: &FallibleMap<u64, Vec<u8>>,
    ) -> Result<(), FileSystemError> {
        if writes.is_empty() {
            return Ok(());
        }
        record_test_transaction();
        let tag_capacity = self.layout.tags_per_descriptor();
        let descriptor_count = writes.len().div_ceil(tag_capacity);
        assert!(
            1 + writes.len() + descriptor_count < self.blocks.len(),
            "staged transaction exceeded validated journal layout"
        );
        let sequence = self.sequence;
        self.write_state(fs, 1, sequence)?;
        let uuid = fs.superblock.lock().s_uuid;
        let tail = fs.block_size - JBD2_BLOCK_TAIL_SIZE;
        let mut cursor = 1;
        // commit 已把 journal 标为 dirty；此后 descriptor/escape 复用同一固定栈 scratch。
        // 若在 durable state publication 后才尝试 heap allocation，OOM 会留下无法安全重试的半事务。
        let mut scratch_storage = [0u8; PAGE_SIZE];
        let scratch = &mut scratch_storage[..fs.block_size];
        let mut next_block = writes.first_key_value().map(|(&block, _)| block);
        while let Some(first_block) = next_block {
            scratch.fill(0);
            put_header(&mut *scratch, JBD2_DESCRIPTOR_BLOCK, sequence)?;
            let mut offset = 12;
            let count = writes.iter_from(&first_block).take(tag_capacity).count();
            let mut last_block = first_block;
            for (index, (block, bytes)) in writes
                .iter_from(&first_block)
                .take(tag_capacity)
                .enumerate()
            {
                let escaped = bytes[..4] == JBD2_MAGIC.to_be_bytes();
                let mut flags = if index == 0 { 0 } else { JBD2_FLAG_SAME_UUID };
                if escaped {
                    flags |= JBD2_FLAG_ESCAPE;
                }
                if index + 1 == count {
                    flags |= JBD2_FLAG_LAST_TAG;
                }
                // tag checksum 覆盖写入 journal 的 image（escape 后首 4 byte 为零）。
                let checksum = if escaped {
                    let crc = checksum::crc32c(self.checksum_seed, &sequence.to_be_bytes());
                    let crc = checksum::crc32c(crc, &[0; 4]);
                    checksum::crc32c(crc, &bytes[4..])
                } else {
                    tag_checksum(self.checksum_seed, sequence, bytes)
                };
                put_be32(&mut *scratch, offset, *block as u32)?;
                put_be32(&mut *scratch, offset + 4, flags)?;
                put_be32(&mut *scratch, offset + 8, (*block >> 32) as u32)?;
                put_be32(&mut *scratch, offset + 12, checksum)?;
                offset += JBD2_TAG3_SIZE;
                if index == 0 {
                    scratch[offset..offset + JBD2_UUID_SIZE].copy_from_slice(&uuid);
                    offset += JBD2_UUID_SIZE;
                }
                last_block = *block;
            }
            let descriptor_checksum = block_checksum(self.checksum_seed, scratch, tail);
            put_be32(&mut *scratch, tail, descriptor_checksum)?;
            self.journal_write(fs, cursor, scratch)?;
            cursor += 1;
            for (_, bytes) in writes.iter_from(&first_block).take(count) {
                let journal_bytes: &[u8] = if bytes[..4] == JBD2_MAGIC.to_be_bytes() {
                    scratch.copy_from_slice(bytes);
                    scratch[..4].fill(0);
                    &*scratch
                } else {
                    bytes
                };
                self.journal_write(fs, cursor, journal_bytes)?;
                cursor += 1;
            }
            next_block = writes.successor(&last_block).map(|(&block, _)| block);
        }
        // Descriptor 和全部 data image 必须先于 commit record 到达稳定存储。否则断电可留下
        // durable commit 与旧 journal slot data 的组合，recovery 会把旧 image 当成新事务 replay。
        fs.device.flush().map_err(block_error)?;
        scratch.fill(0);
        put_header(&mut *scratch, JBD2_COMMIT_BLOCK, sequence)?;
        let nanoseconds = crate::timer::get_realtime_ns();
        let seconds = nanoseconds / 1_000_000_000;
        put_be32(
            &mut *scratch,
            JBD2_COMMIT_SECONDS_OFFSET,
            (seconds >> 32) as u32,
        )?;
        put_be32(
            &mut *scratch,
            JBD2_COMMIT_SECONDS_OFFSET + 4,
            seconds as u32,
        )?;
        put_be32(
            &mut *scratch,
            JBD2_COMMIT_NANOSECONDS_OFFSET,
            (nanoseconds % 1_000_000_000) as u32,
        )?;
        let commit_checksum =
            block_checksum(self.checksum_seed, scratch, JBD2_COMMIT_CHECKSUM_OFFSET);
        put_be32(&mut *scratch, JBD2_COMMIT_CHECKSUM_OFFSET, commit_checksum)?;
        self.journal_write(fs, cursor, scratch)?;
        fs.device.flush().map_err(block_error)?;
        for (block, bytes) in writes {
            fs.write_fs_block_home(*block, bytes)?;
        }
        fs.device.flush().map_err(block_error)?;
        self.sequence = sequence.wrapping_add(1);
        self.write_state(fs, 0, self.sequence)?;
        // Home blocks are already durable. Persisting clean state may lag: a crash can only replay
        // the just-checkpointed transaction idempotently, while the next transaction's initial
        // barrier also orders this clean marker before its commit record.
        Ok(())
    }
}

fn zeroed(length: usize) -> Result<Vec<u8>, FileSystemError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| FileSystemError::OutOfMemory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

/// mutation mutex、lazy runtime undo set 与 journal transaction 的唯一 RAII owner。
pub(super) struct MutationGuard<'a> {
    fs: &'a Ext4FileSystem,
    _lock: TaskMutexGuard<'a, ()>,
    superblock: Ext4SuperBlock,
    groups: Vec<Ext4GroupDesc>,
    // OWNER: this guard exclusively owns runtime inode preimages until commit/abort. Four live
    // slots cover the widest rename transaction; one transient slot covers create or final Drop.
    // Overflow fails before an untracked mutation, otherwise abort could publish stale live state.
    inodes: [Option<(Arc<Ext4Inode>, Ext4InodeDisk)>; MAX_LIVE_INODE_UNDOS],
    inode_count: usize,
    discarded_inode: Option<u32>,
    committed: bool,
}

impl<'a> MutationGuard<'a> {
    /// 取得唯一 mutation lock、冻结 allocator snapshot 并开始空 redo write-set。
    ///
    /// # Parameters
    ///
    /// - `fs`: journal 已加载且未 aborted 的 filesystem。
    ///
    /// # Returns
    ///
    /// 拥有 transaction 与 rollback snapshot 的 guard。
    ///
    /// # Errors
    ///
    /// journal 缺失、aborted 或 transaction 重入时返回错误。
    pub(super) fn begin(fs: &'a Ext4FileSystem) -> Result<Self, FileSystemError> {
        Self::begin_after(fs, || Ok(())).map(|(guard, ())| guard)
    }

    /// 仅在无需等待时取得 mutation owner 并开始空 transaction。
    ///
    /// # Parameters
    ///
    /// - `fs`: journal 已加载且未 aborted 的 filesystem。
    ///
    /// # Returns
    ///
    /// owner 空闲时返回完整 guard，忙时返回 `None` 且不执行任何 prepare。
    ///
    /// # Errors
    ///
    /// owner 取得后的 snapshot 或 journal begin 失败返回对应错误。
    pub(super) fn try_begin(fs: &'a Ext4FileSystem) -> Result<Option<Self>, FileSystemError> {
        let Some(lock) = fs.mutation.try_lock() else {
            return Ok(None);
        };
        Self::begin_after_lock(fs, lock, || Ok(())).map(|(guard, ())| Some(guard))
    }

    /// 取得 mutation lock，执行无副作用 live-state prepare，成功后才冻结 rollback 并开 journal。
    ///
    /// # Parameters
    ///
    /// - `prepare`: 只读当前 mutation domain、不得发布状态的 fallible prepare。
    ///
    /// # Returns
    ///
    /// guard 与锁内准备结果；prepare 失败不分配 snapshot、不发布 active transaction。
    pub(super) fn begin_after<T>(
        fs: &'a Ext4FileSystem,
        prepare: impl FnOnce() -> Result<T, FileSystemError>,
    ) -> Result<(Self, T), FileSystemError> {
        let lock = fs
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Self::begin_after_lock(fs, lock, prepare)
    }

    fn begin_after_lock<T>(
        fs: &'a Ext4FileSystem,
        lock: TaskMutexGuard<'a, ()>,
        prepare: impl FnOnce() -> Result<T, FileSystemError>,
    ) -> Result<(Self, T), FileSystemError> {
        let prepared = prepare()?;
        let superblock = *fs.superblock.lock();
        // 1. The topology-wide allocator snapshot precedes the active transaction. Live inode
        // preimages use the fixed current-domain slots and are captured before first mutation.
        let groups = {
            let source = fs.groups.lock();
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(source.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            snapshot.extend_from_slice(&source);
            snapshot
        };
        // 2. Only after the eager allocator rollback allocation succeeds may the journal publish
        // an active transaction. Inode undo is stack-resident and cannot add an OOM path.
        fs.journal.lock().ready_mut()?.begin(groups.len())?;
        Ok((
            Self {
                fs,
                _lock: lock,
                superblock,
                groups,
                inodes: [const { None }; MAX_LIVE_INODE_UNDOS],
                inode_count: 0,
                discarded_inode: None,
                committed: false,
            },
            prepared,
        ))
    }

    /// 首次可变访问 live inode 时先捕获其唯一 rollback preimage。
    ///
    /// # Parameters
    ///
    /// - `inode`: 当前 filesystem inode-cache 中由 caller 保活的 inode。
    ///
    /// # Returns
    ///
    /// 已建立 abort 恢复证明、锁外可修改的 inode working copy。
    ///
    /// # Errors
    ///
    /// cache owner 分裂或超过当前事务已证明的四 inode 上限返回 invalid operation。
    pub(super) fn inode<'mutation, 'inode>(
        &'mutation mut self,
        inode: &'inode Ext4Inode,
    ) -> Result<InodeMutation<'mutation, 'inode>, FileSystemError> {
        let number = inode.inode_num;
        // mutation mutex 是 live inode 的唯一 writer owner；短锁只取得完整 snapshot，绝不
        // 穿过后续 journal 或 block I/O。否则单核 waiter 会在关中断 spin loop 中永久饿死
        // 正在 DriverIo 中睡眠的 owner。
        let disk = *inode.disk.lock();
        let discarded_on_abort = self.discarded_inode == Some(number);
        let captured = self
            .inodes
            .iter()
            .take(self.inode_count)
            .flatten()
            .any(|(captured, _)| captured.inode_num == number);
        if !discarded_on_abort && !captured {
            let slot = self
                .inodes
                .get_mut(self.inode_count)
                .ok_or(FileSystemError::InvalidOperation)?;
            let owner = self
                .fs
                .inode_cache
                .lock()
                .get(&number)
                .and_then(Weak::upgrade)
                .filter(|owner| core::ptr::eq(Arc::as_ptr(owner), inode))
                .ok_or(FileSystemError::InvalidFileSystem)?;
            *slot = Some((owner, disk));
            self.inode_count += 1;
        }
        Ok(InodeMutation::new(inode, disk))
    }

    /// 在 transient inode 可能被修改前登记 abort 删除责任。
    ///
    /// # Parameters
    ///
    /// - `number`: 本 transaction 新分配、或已进入 final Drop 无法保活 Arc 的 inode number。
    ///
    /// # Returns
    ///
    /// 后续 inode mutation/cache publication 不再需要 rollback state。
    ///
    /// # Errors
    ///
    /// 同一 transaction 出现第二 transient inode 返回 invalid operation。
    pub(super) fn discard_inode_on_abort(&mut self, number: u32) -> Result<(), FileSystemError> {
        match self.discarded_inode {
            Some(existing) if existing == number => Ok(()),
            Some(_) => Err(FileSystemError::InvalidOperation),
            None => {
                self.discarded_inode = Some(number);
                Ok(())
            }
        }
    }

    /// 按 journal→commit→home→clean 顺序持久化并消费本次 guard。
    ///
    /// # Returns
    ///
    /// 所有 home blocks 已 checkpoint 到 stable-storage capability 时成功。
    ///
    /// # Errors
    ///
    /// journal 容量或 block I/O/FLUSH 失败时返回错误并 fail-stop 后续 mutation。
    pub(super) fn commit(mut self) -> Result<(), FileSystemError> {
        let allocation_dirty = self
            .fs
            .journal
            .lock()
            .ready_mut()?
            .take_allocation_dirty()?;
        self.fs.write_dirty_allocation_metadata(&allocation_dirty)?;
        commit_owner::JournalCommit::begin(self.fs)?.commit()?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for MutationGuard<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(journal) = self.fs.journal.lock().ready_mut() {
            journal.abort(self.fs);
        }
        *self.fs.superblock.lock() = self.superblock;
        *self.fs.groups.lock() = core::mem::take(&mut self.groups);
        for (inode, disk) in self.inodes.iter().take(self.inode_count).flatten() {
            *inode.disk.lock() = *disk;
        }
        if let Some(number) = self.discarded_inode {
            self.fs.inode_cache.lock().remove(&number);
        }
    }
}
