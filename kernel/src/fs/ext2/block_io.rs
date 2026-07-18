use super::*;

impl Ext2FileSystem {
    pub(super) fn read_fs_block(
        &self,
        fs_block_id: u32,
        buf: &mut [u8],
    ) -> Result<(), FileSystemError> {
        if fs_block_id >= self.superblock.lock().s_blocks_count {
            return Err(FileSystemError::InvalidFileSystem);
        }
        if self
            .journal
            .lock()
            .as_ref()
            .is_some_and(|journal| journal.copy_staged(fs_block_id, buf))
        {
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
                .map_err(|_| FileSystemError::IoError)
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
                    .map_err(|_| FileSystemError::IoError)?;
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
                .map_err(|_| FileSystemError::IoError)?;

            buf.copy_from_slice(&dev_buf[offset_in_dev_block..offset_in_dev_block + fs_block_size]);
            Ok(())
        }
    }

    pub(super) fn write_fs_block(
        &self,
        fs_block_id: u32,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
        let mut journal = self.journal.lock();
        if let Some(journal) = journal.as_mut() {
            return journal.stage(fs_block_id, buf, self.block_size);
        }
        drop(journal);
        self.write_fs_block_home(fs_block_id, buf)
    }

    pub(super) fn write_fs_block_home(
        &self,
        fs_block_id: u32,
        buf: &[u8],
    ) -> Result<(), FileSystemError> {
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
                .map_err(|_| FileSystemError::IoError)?;
        } else if self.block_size > device_block_size {
            let count = self.block_size / device_block_size;
            let first = fs_block_id as usize * count;
            for index in 0..count {
                let offset = index * device_block_size;
                self.device
                    .write_block(first + index, &buf[offset..offset + device_block_size])
                    .map_err(|_| FileSystemError::IoError)?;
            }
        } else {
            let count = device_block_size / self.block_size;
            let device_block = fs_block_id as usize / count;
            let offset = fs_block_id as usize % count * self.block_size;
            let mut device_buf = try_zeroed(device_block_size)?;
            self.device
                .read_block(device_block, &mut device_buf)
                .map_err(|_| FileSystemError::IoError)?;
            device_buf[offset..offset + self.block_size].copy_from_slice(buf);
            self.device
                .write_block(device_block, &device_buf)
                .map_err(|_| FileSystemError::IoError)?;
        }
        Ok(())
    }
}
