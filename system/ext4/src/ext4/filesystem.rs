use super::*;
use crate::vfs::FileSystemStatistics;
use core::sync::atomic::Ordering;

impl Ext4FileSystem {
    /// 根目录 inode。
    pub(crate) fn root_inode(&self) -> Result<Arc<Ext4Inode>, FileSystemError> {
        let fs_arc = self
            .self_ref
            .lock()
            .upgrade()
            .ok_or(FileSystemError::InvalidFileSystem)?;
        Ext4Inode::load(fs_arc, EXT4_ROOT_INO)
    }

    /// statfs。
    pub(crate) fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
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
        Ok(FileSystemStatistics {
            block_size: self.block_size as u64,
            blocks: superblock.blocks_count().saturating_sub(overhead),
            blocks_free: superblock
                .free_blocks_count()
                .saturating_sub(self.delalloc_reserved.load(Ordering::Relaxed)),
            blocks_available: superblock
                .free_blocks_count()
                .saturating_sub(superblock.reserved_blocks_count())
                .saturating_sub(self.delalloc_reserved.load(Ordering::Relaxed)),
            files: superblock.s_inodes_count as u64,
            files_free: superblock.s_free_inodes_count as u64,
            name_length: 255,
            fragment_size: self.block_size as u64,
        })
    }

    /// 只读挂载重新挂载为可写（日志恢复后的首次写入准备）。
    pub(crate) fn remount_writable(&self) -> Result<(), FileSystemError> {
        self.make_writable()
    }

    /// 卸载：停止写回线程并提交最后的 transaction；每个挂载恰好一次。
    pub(crate) fn shutdown(&self) -> Result<(), FileSystemError> {
        // mount lifecycle 的 shutdown 由 MOUNT_TRANSACTION 串行，恰好一次。
        // 先无分配等待所有旧 mutation/I/O 离开，再在同一 owner 内停止 daemon 和最终 drain；
        // 即使 commit Arc 准备 OOM，返回后 daemon 也不再碰已释放的物理租约。
        let mut waiter = self
            .shutdown_waiter
            .lock()
            .take()
            .expect("filesystem shutdown repeated outside its mount lifecycle");
        let _mutation = self.mutation.lock_prepared(&mut waiter);
        self.stopping.store(true, Ordering::Release);
        let committed = self.commit_running_transaction();
        self.commit_event.signal();
        committed
    }
}

impl Ext4FileSystem {
    /// 排空已失去引用的延迟 orphan 回收，再提交 running transaction；`fsync`、`fdatasync` 与
    /// `sync` 的持久化边界。
    ///
    /// 1. 沿唯一回收路径每轮回收一个 final Drop 时因 owner 忙而延迟的 orphan；缺失时 sync 之后
    ///    磁盘仍留着 nlink 0、dtime 0、占用 bitmap 与 orphan 槽位的 inode（e2fsck 报不一致）；
    /// 2. 取得 owner 后重验待办，回收期间新发布的待办继续排空；仍被打开或映射的 orphan 保留；
    /// 3. 同一 owner 内提交；每次提交都以 home checkpoint 后的 barrier 结束，没有 running
    ///    transaction 时不产生 I/O。
    ///
    /// # Errors
    ///
    /// owner 或回收分配失败、元数据无效、停止状态与提交 I/O 错误向上传递；回收失败保留重试 bit，
    /// 提交 I/O 失败使 journal fail-stop。
    pub(super) fn sync_journal(&self) -> Result<(), FileSystemError> {
        loop {
            // 回收自己取得 mutation owner；持 owner 调用会再次获取同一 TaskMutex 而自锁。
            self.reclaim_pending_orphan()?;
            let _mutation = self
                .mutation
                .lock()
                .map_err(|_| FileSystemError::OutOfMemory)?;
            if self.stopping.load(Ordering::Acquire) {
                return Err(FileSystemError::InvalidOperation);
            }
            if self.pending_orphan_reclaim.load(Ordering::Acquire) {
                continue;
            }
            return self.commit_running_transaction();
        }
    }

    /// 创建本 filesystem 的写回线程（对应 Linux 每个 journal 的 jbd2 线程）。
    ///
    /// # Errors
    ///
    /// 线程创建失败时返回 `OutOfMemory`。
    pub(crate) fn start_writeback(self: &Arc<Self>) -> Result<(), FileSystemError> {
        let filesystem = self.clone();
        let thread = native_runtime::thread::spawn(b"ext4-writeback", move || {
            filesystem.run_writeback_daemon(|deadline| {
                let _ = native_runtime::system::sleep_until(
                    i64::try_from(deadline).unwrap_or(i64::MAX),
                );
            });
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        // 写回线程在卸载时自行结束。
        drop(thread);
        Ok(())
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
    /// - `sleep_until`: 睡到 absolute monotonic deadline。
    ///
    /// umount 置位 `stopping` 并 signal 后返回，线程随之终止。
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
        if self.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        self.commit_running_transaction()
    }
}

/// Host 只替换 deadline/scheduler adapter，执行原始 daemon 循环。
#[cfg(test)]
pub(crate) fn test_run_writeback_daemon(fs: Arc<Ext4FileSystem>, sleep_until: fn(u64)) {
    fs.run_writeback_daemon(sleep_until);
}

impl Ext4FileSystem {
    /// 为延迟分配预留 `blocks` 个 block：成功后写者收到的成功不会因回写时没有空间而失效。
    ///
    /// 记账是精确的块数；准入检查另给总预留留出 extent 树节点的余量（每 128 个数据块一个，
    /// 回写分配数据块时还可能要拆分 extent 叶），所以余量不在预留/归还之间漂移。
    ///
    /// # Errors
    /// 除去 root 保留块、已有预留与余量后的空闲 block 不足返回 `NoSpace`。
    pub(crate) fn reserve_delalloc(&self, blocks: u64) -> Result<(), FileSystemError> {
        let superblock = self.superblock.lock();
        let available = superblock
            .free_blocks_count()
            .saturating_sub(superblock.reserved_blocks_count());
        self.delalloc_reserved
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |reserved| {
                let total = reserved.checked_add(blocks)?;
                (total + total / 128 < available).then_some(total)
            })
            .map(drop)
            .map_err(|_| FileSystemError::NoSpace)
    }

    /// 归还 [`Self::reserve_delalloc`] 预留的 `blocks` 个 block（回写即将分配它们，或预留所属的区间被丢弃）。
    pub(crate) fn release_delalloc(&self, blocks: u64) {
        let _ =
            self.delalloc_reserved
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |reserved| {
                    Some(reserved.saturating_sub(blocks))
                });
    }

    /// 回写分配失败后把已归还的预留补回（不检查空闲数）：数据仍在内容 VMO 里等待下一次回写，
    /// 承诺不能因为一次失败的尝试而消失。
    pub(crate) fn restore_delalloc(&self, blocks: u64) {
        self.delalloc_reserved.fetch_add(blocks, Ordering::Relaxed);
    }
}
