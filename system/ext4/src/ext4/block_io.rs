use super::*;

impl Ext4FileSystem {
    /// metadata cache 里已有的 block；不做 I/O。
    pub(super) fn metadata_cached(&self, fs_block_id: u64) -> Option<Arc<Vec<u8>>> {
        self.metadata_cache.lock().get(fs_block_id)
    }

    /// Return one immutable directory/extent metadata block under the filesystem-wide identity.
    pub(super) fn read_metadata_block(
        &self,
        fs_block_id: u64,
    ) -> Result<Arc<Vec<u8>>, FileSystemError> {
        let mut generation = {
            // Staged writes publish cache replacement/invalidation before releasing journal state;
            // a cloned old Arc therefore linearizes before the write, while a miss rechecks the
            // unique Ready/Committing journal owner in read_fs_block.
            let mut cache = self.metadata_cache.lock();
            if let Some(bytes) = cache.get(fs_block_id) {
                return Ok(bytes);
            }
            cache.generation()
        };
        if fail_test_metadata_owner() {
            return Err(FileSystemError::OutOfMemory);
        }
        record_test_allocation_attempt();
        let mut bytes =
            Arc::try_new(try_zeroed(self.block_size)?).map_err(|_| FileSystemError::OutOfMemory)?;
        loop {
            self.read_fs_block(
                fs_block_id,
                Arc::get_mut(&mut bytes).expect("unpublished metadata image must be unique"),
            )?;
            let mut cache = self.metadata_cache.lock();
            if cache.generation() != generation {
                // journal miss 与 home I/O 之间允许 task 切换；该期间开始的 checkpoint
                // 可让扇区读取得撕裂 image。拒绝 admission 还不够，禁止把它返回给 caller。
                if let Some(current) = cache.get(fs_block_id) {
                    return Ok(current);
                }
                generation = cache.generation();
                continue;
            }
            cache.insert_if_unchanged(generation, fs_block_id, bytes.clone());
            return Ok(bytes);
        }
    }

    /// 读一个 filesystem block；见 [`Self::read_fs_blocks`]。
    pub(super) fn read_fs_block(
        &self,
        fs_block_id: u64,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        self.read_fs_blocks(fs_block_id, buf)
    }

    /// 读从 `first_block` 起连续的 filesystem block，长度为 `block_size` 的整数倍。
    ///
    /// 1. 全部 caller 共用 journal → home 的一致 snapshot seam：先读 home 区间，再在一次 journal
    ///    guard 内用 staged 映像覆盖，staged 命中因此在 guard 内线性化。
    /// 2. 单块先查 staged，命中则省去 home I/O；多块不预查，避免对整段区间逐块持锁探测。
    /// 3. home I/O 可跨 task 切换；期间若 mutation/checkpoint 推进了 cache generation，
    ///    区间可能撕裂旧/新映像，复用 caller buffer 整段重读，绝不返回撕裂结果。
    pub(super) fn read_fs_blocks(
        &self,
        first_block: u64,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if buf.is_empty() || !buf.len().is_multiple_of(self.block_size) {
            return Err(FileSystemError::IoError);
        }
        let blocks = (buf.len() / self.block_size) as u64;
        if first_block
            .checked_add(blocks)
            .is_none_or(|end| end > self.superblock.lock().blocks_count())
        {
            return Err(FileSystemError::InvalidFileSystem);
        }
        loop {
            let generation = self.metadata_cache.lock().generation();
            if blocks == 1 && self.journal.lock().copy_staged(first_block, buf) {
                return Ok(());
            }
            self.read_fs_block_home(first_block, buf)?;
            {
                let journal = self.journal.lock();
                for (index, chunk) in buf.chunks_exact_mut(self.block_size).enumerate() {
                    journal.copy_staged(first_block + index as u64, chunk);
                }
            }
            if self.metadata_cache.lock().generation() == generation {
                return Ok(());
            }
        }
    }

    /// 读多段互不相邻的连续 filesystem block，设备可并发处理整批；一致性语义同 [`Self::read_fs_blocks`]。
    ///
    /// 每段的缓冲长度为 `block_size` 的整数倍。整批共用一次 home 读与一次 journal 覆盖，
    /// 期间 cache generation 变化则整批重读，不返回撕裂映像。
    pub(super) fn read_fs_runs(
        &self,
        runs: &mut [(u64, &mut [u8])],
    ) -> Result<(), FileSystemError> {
        if let [(first, buf)] = runs {
            return self.read_fs_blocks(*first, buf);
        }
        let total = self.superblock.lock().blocks_count();
        for (first, buf) in runs.iter() {
            if buf.is_empty() || !buf.len().is_multiple_of(self.block_size) {
                return Err(FileSystemError::IoError);
            }
            if first
                .checked_add((buf.len() / self.block_size) as u64)
                .is_none_or(|end| end > total)
            {
                return Err(FileSystemError::InvalidFileSystem);
            }
        }
        loop {
            let generation = self.metadata_cache.lock().generation();
            self.read_fs_runs_home(runs)?;
            {
                let journal = self.journal.lock();
                for (first, buf) in runs.iter_mut() {
                    for (index, chunk) in buf.chunks_exact_mut(self.block_size).enumerate() {
                        journal.copy_staged(*first + index as u64, chunk);
                    }
                }
            }
            if self.metadata_cache.lock().generation() == generation {
                return Ok(());
            }
        }
    }

    fn read_fs_runs_home(&self, runs: &mut [(u64, &mut [u8])]) -> Result<(), FileSystemError> {
        let device_block_size = self.device.block_size();
        if self.block_size < device_block_size {
            // 一个设备块容纳多个 filesystem block：逐段读出所在设备块后截取。
            for (first, buf) in runs.iter_mut() {
                Self::read_fs_blocks_from(&self.device, self.block_size, *first, buf)?;
            }
            return Ok(());
        }
        let ratio = self.block_size / device_block_size;
        let mut ranges: Vec<(usize, &mut [u8])> = Vec::new();
        ranges
            .try_reserve_exact(runs.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        for (first, buf) in runs.iter_mut() {
            ranges.push((*first as usize * ratio, &mut **buf));
        }
        self.device
            .read_block_ranges(&mut ranges)
            .map_err(block_error)
    }

    /// 只读 home 区间，不查 journal；`buf` 为 `block_size` 的整数倍，边界由调用者校验。
    pub(super) fn read_fs_block_home(
        &self,
        first_block: u64,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        Self::read_fs_blocks_from(&self.device, self.block_size, first_block, buf)
    }

    pub(super) fn read_fs_blocks_from(
        device: &Arc<dyn BlockDevice>,
        fs_block_size: usize,
        first_block: u64,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if buf.is_empty() || !buf.len().is_multiple_of(fs_block_size) {
            return Err(FileSystemError::IoError);
        }
        let dev_block_size = device.block_size();
        if fs_block_size >= dev_block_size {
            // 一个 filesystem block 对应整数个设备块：整段区间一次提交，由设备层按协商窗口拆分。
            let start = first_block as usize * (fs_block_size / dev_block_size);
            return device
                .read_blocks(start, buf)
                .map_err(block_error)
                .map(|_| ());
        }
        // 一个设备块容纳多个 filesystem block：逐块读出所在设备块后截取。
        let fs_blocks_per_dev_block = dev_block_size / fs_block_size;
        let mut dev_buf = try_zeroed(dev_block_size)?;
        for (index, chunk) in buf.chunks_exact_mut(fs_block_size).enumerate() {
            let block = first_block as usize + index;
            device
                .read_blocks(block / fs_blocks_per_dev_block, &mut dev_buf)
                .map_err(block_error)?;
            let offset = block % fs_blocks_per_dev_block * fs_block_size;
            chunk.copy_from_slice(&dev_buf[offset..offset + fs_block_size]);
        }
        Ok(())
    }

    /// 暂存一个 metadata block；提交时经 journal 原子写入。
    pub(super) fn write_fs_block(
        &self,
        fs_block_id: u64,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        self.stage_block(fs_block_id, buf, BlockKind::Metadata)
    }

    /// 暂存一个文件数据 block；提交时在 commit record 之前直接写回 home（`data=ordered`）。
    pub(super) fn write_data_block(
        &self,
        fs_block_id: u64,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        self.stage_block(fs_block_id, buf, BlockKind::Data)
    }

    pub(super) fn stage_block(
        &self,
        fs_block_id: u64,
        buf: &[u8],
        kind: BlockKind,
    ) -> Result<(), FileSystemError> {
        let mut owner = self.journal.lock();
        owner
            .ready_mut()?
            .stage(fs_block_id, buf, self.block_size, kind)?;
        self.metadata_cache
            .lock()
            .update_if_present(fs_block_id, buf);
        Ok(())
    }

    pub(super) fn write_fs_block_home(
        &self,
        fs_block_id: u64,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        record_test_home_write();
        if fs_block_id >= self.superblock.lock().blocks_count() {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if buf.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }
        let device_block_size = self.device.block_size();
        if self.block_size == device_block_size {
            self.device
                .write_blocks(fs_block_id as usize, buf)
                .map_err(block_error)?;
        } else if self.block_size > device_block_size {
            let count = self.block_size / device_block_size;
            let first = fs_block_id as usize * count;
            for index in 0..count {
                let offset = index * device_block_size;
                self.device
                    .write_blocks(first + index, &buf[offset..offset + device_block_size])
                    .map_err(block_error)?;
            }
        } else {
            let count = device_block_size / self.block_size;
            let device_block = fs_block_id as usize / count;
            let offset = fs_block_id as usize % count * self.block_size;
            let mut device_buf = try_zeroed(device_block_size)?;
            self.device
                .read_blocks(device_block, &mut device_buf)
                .map_err(block_error)?;
            device_buf[offset..offset + self.block_size].copy_from_slice(buf);
            self.device
                .write_blocks(device_block, &device_buf)
                .map_err(block_error)?;
        }
        // checkpoint 只持久化 running preimage；stage/abort 才拥有逻辑 cache publication。
        // 这里重写 cache 会覆盖仍在 active overlay 中的较新 image。mount/recovery
        // 在发布 journal/root 之前统一 reload 并清除缓存。
        Ok(())
    }
}
