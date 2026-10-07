use super::*;
use crate::fs::{FileSystemStatistics, KernelThreadSupport};
use core::sync::atomic::Ordering;

impl FileSystem for Ext4FileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ext4Inode::load(fs_arc, EXT4_ROOT_INO).map(|inode| inode as Arc<dyn Inode>)
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        // 1. 与 allocator mutation 共锁取得 superblock 计数；缺少该锁会观察到
        // group descriptor 与 superblock 更新之间的中间状态。
        let _mutation = self
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let superblock = *self.superblock.lock();
        let group_count = self.groups.lock().len();
        // 2. 按 Linux ext4_calculate_overhead 排除每个 group 的 backup superblock/GDT/reserved
        //    GDT、两个 bitmap、inode table，以及内部 journal 占用的 block。
        let per_group: u64 = (0..group_count)
            .map(|group| {
                (self.group_base_metadata_blocks(group) + 2 + self.inode_table_blocks()) as u64
            })
            .sum();
        let journal_blocks =
            self.read_inode_disk(EXT4_JOURNAL_INO)?.size() / self.block_size as u64;
        let overhead = per_group + journal_blocks;
        // 3. Linux uuid_to_fsid 将两个 little-endian 64-bit half xor 后折叠为 fsid。
        let first = u64::from_le_bytes(superblock.s_uuid[..8].try_into().unwrap());
        let second = u64::from_le_bytes(superblock.s_uuid[8..].try_into().unwrap());
        let fsid = first ^ second;
        Ok(FileSystemStatistics {
            type_name: "ext4",
            magic: EXT4_SUPER_MAGIC as u64,
            block_size: self.block_size as u64,
            blocks: superblock.blocks_count().saturating_sub(overhead),
            blocks_free: superblock.free_blocks_count(),
            blocks_available: superblock
                .free_blocks_count()
                .saturating_sub(superblock.reserved_blocks_count()),
            files: superblock.s_inodes_count as u64,
            files_free: superblock.s_free_inodes_count as u64,
            fsid: [fsid as u32, (fsid >> 32) as u32],
            name_length: 255,
            fragment_size: self.block_size as u64,
            flags: 0,
        })
    }

    /// 提交 running transaction（每次提交以 barrier 结束），再让写回线程退出。
    fn make_writable(&self) -> Result<(), FileSystemError> {
        Ext4FileSystem::make_writable(self)
    }

    fn shutdown(&self) -> Result<(), FileSystemError> {
        let committed = self.sync_journal();
        self.stopping.store(true, Ordering::Release);
        self.commit_event.signal();
        committed
    }
}

impl Ext4FileSystem {
    /// 提交 running transaction；`fsync`、`fdatasync` 与 `sync` 的持久化边界。
    ///
    /// 每次提交都以 home checkpoint 后的 barrier 结束，因此已提交事务都已 durable；提交后不再
    /// 需要额外 flush。没有 running transaction 时不产生任何 I/O。
    ///
    /// # Errors
    ///
    /// mutation owner 取得失败返回 `OutOfMemory`；提交 I/O 失败返回错误并使 journal fail-stop。
    pub(super) fn sync_journal(&self) -> Result<(), FileSystemError> {
        let _mutation = self
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.commit_running_transaction()
    }

    /// 创建本 filesystem 的写回内核线程（对应 Linux 每个 journal 的 jbd2 线程）。
    ///
    /// # Errors
    ///
    /// 线程主体或内核线程分配失败时返回 `OutOfMemory`。
    pub(in crate::fs) fn start_writeback(
        self: &Arc<Self>,
        threads: KernelThreadSupport,
    ) -> Result<(), FileSystemError> {
        let filesystem = self.clone();
        let sleep_until = threads.sleep_until;
        let body = alloc::boxed::Box::try_new(move || filesystem.run_writeback_daemon(sleep_until))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        (threads.spawn)("ext4-writeback", body).map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 写回线程主体：running transaction 出现后，在其年龄达到提交间隔时提交。
    ///
    /// 1. 没有未提交 mutation 时阻塞在 `commit_event`，不产生周期唤醒；
    /// 2. 被唤醒后睡到 running transaction 开始时刻加提交间隔；
    /// 3. 取得 mutation owner 并提交此刻累积的全部 mutation。若期间已由 `fsync` 或容量阈值
    ///    提交，第 3 步为空操作；之后新建的 running transaction 会再次 signal。
    ///
    /// # Parameters
    ///
    /// - `sleep_until`: composition root 注入的 absolute monotonic deadline 睡眠；fs 不依赖 task。
    ///
    /// umount 置位 `stopping` 并 signal 后返回，内核线程随之终止。
    fn run_writeback_daemon(self: Arc<Self>, sleep_until: fn(u64)) {
        loop {
            self.commit_event.wait();
            if self.stopping.load(Ordering::Acquire) {
                return;
            }
            let started = self
                .journal
                .lock()
                .ready_mut_ref()
                .ok()
                .and_then(|journal| journal.running_started_ns());
            let Some(started) = started else {
                continue;
            };
            sleep_until(started.saturating_add(journal::COMMIT_INTERVAL_NS));
            if let Err(error) = self.commit_aged() {
                error!("ext4 writeback commit failed: {:?}", error);
            }
        }
    }

    fn commit_aged(&self) -> Result<(), FileSystemError> {
        let _mutation = self
            .mutation
            .lock()
            .map_err(|_| FileSystemError::OutOfMemory)?;
        self.commit_running_transaction()
    }
}
