use super::*;

impl Ext2FileSystem {
    /// Return one immutable directory/pointer metadata block under the filesystem-wide identity.
    pub(super) fn read_metadata_block(
        &self,
        fs_block_id: u32,
    ) -> Result<Arc<Vec<u8>>, FileSystemError> {
        let generation = {
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
        self.read_fs_block(
            fs_block_id,
            Arc::get_mut(&mut bytes).expect("new metadata block has one owner"),
        )?;
        self.metadata_cache
            .lock()
            .insert_if_unchanged(generation, fs_block_id, bytes.clone());
        Ok(bytes)
    }

    pub(super) fn read_fs_block(
        &self,
        fs_block_id: u32,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if self.journal.lock().copy_staged(fs_block_id, buf) {
            return Ok(());
        }
        self.read_fs_block_home(fs_block_id, buf)
    }

    pub(super) fn read_fs_block_home(
        &self,
        fs_block_id: u32,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        Self::read_fs_block_from(&self.device, self.block_size, fs_block_id, buf)
    }

    pub(super) fn read_fs_block_from(
        device: &Arc<dyn BlockDevice>,
        fs_block_size: usize,
        fs_block_id: u32,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if buf.len() != fs_block_size {
            return Err(FileSystemError::IoError);
        }

        let dev_block_size = device.block_size();

        if fs_block_size == dev_block_size {
            // Simple 1:1 mapping
            device
                .read_block(fs_block_id as usize, buf)
                .map_err(block_error)
                .map(|_| ())
        } else if fs_block_size > dev_block_size {
            // Filesystem block spans multiple device blocks
            let dev_blocks_per_fs_block = fs_block_size / dev_block_size;
            let start_dev_block = (fs_block_id as usize) * dev_blocks_per_fs_block;

            for i in 0..dev_blocks_per_fs_block {
                let offset = i * dev_block_size;
                device
                    .read_block(
                        start_dev_block + i,
                        &mut buf[offset..offset + dev_block_size],
                    )
                    .map_err(block_error)?;
            }
            Ok(())
        } else {
            // Multiple filesystem blocks per device block
            let fs_blocks_per_dev_block = dev_block_size / fs_block_size;
            let dev_block = (fs_block_id as usize) / fs_blocks_per_dev_block;
            let offset_in_dev_block =
                ((fs_block_id as usize) % fs_blocks_per_dev_block) * fs_block_size;

            let mut dev_buf = try_zeroed(dev_block_size)?;
            device
                .read_block(dev_block, &mut dev_buf)
                .map_err(block_error)?;

            buf.copy_from_slice(&dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size]);
            Ok(())
        }
    }

    pub(super) fn write_fs_block(
        &self,
        fs_block_id: u32,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        let mut owner = self.journal.lock();
        owner
            .ready_mut()?
            .stage(fs_block_id, buf, self.block_size)?;
        self.metadata_cache
            .lock()
            .update_if_present(fs_block_id, buf);
        Ok(())
    }

    pub(super) fn write_fs_block_home(
        &self,
        fs_block_id: u32,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        record_test_home_write();
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if buf.len() != self.block_size {
            return Err(FileSystemError::IoError);
        }
        let device_block_size = self.device.block_size();
        if self.block_size == device_block_size {
            self.device
                .write_block(fs_block_id as usize, buf)
                .map_err(block_error)?;
        } else if self.block_size > device_block_size {
            let count = self.block_size / device_block_size;
            let first = fs_block_id as usize * count;
            for index in 0..count {
                let offset = index * device_block_size;
                self.device
                    .write_block(first + index, &buf[offset..offset + device_block_size])
                    .map_err(block_error)?;
            }
        } else {
            let count = device_block_size / self.block_size;
            let device_block = fs_block_id as usize / count;
            let offset = fs_block_id as usize % count * self.block_size;
            let mut device_buf = try_zeroed(device_block_size)?;
            self.device
                .read_block(device_block, &mut device_buf)
                .map_err(block_error)?;
            device_buf[offset..offset + self.block_size].copy_from_slice(buf);
            self.device
                .write_block(device_block, &device_buf)
                .map_err(block_error)?;
        }
        self.metadata_cache
            .lock()
            .update_if_present(fs_block_id, buf);
        Ok(())
    }
}
