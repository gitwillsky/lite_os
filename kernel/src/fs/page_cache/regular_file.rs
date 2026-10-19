use super::*;

/// 持久 page-cache 与只读动态快照共用的 regular-file I/O facade。
///
/// syscall 在一次 read/write 操作内复用该值，避免每个 user-copy chunk 重复读取 inode
/// metadata、获取全局 FILES lock 并查找同一个 ordered entry。
pub(crate) struct RegularFile(RegularFileBackend);

enum RegularFileBackend {
    Cached(Arc<CachedFile>),
    Snapshot(Arc<dyn Inode>),
    Memory {
        file: Arc<MemoryFile>,
        inode: Arc<dyn Inode>,
    },
}

/// 一次 regular-file cache read 的 logical 与实际 storage 结果。
#[derive(Debug, Clone, Copy)]
pub(crate) struct RegularFileRead {
    /// 复制到 kernel output 的 logical file bytes。
    pub(crate) bytes: usize,
    /// 本次调用因 cache miss 从 filesystem storage 成功填充的 bytes。
    pub(crate) storage_bytes: usize,
}

impl RegularFile {
    /// 将 regular inode 解析为持久 page-cache owner 或只读动态快照。
    ///
    /// # Parameters
    ///
    /// - `inode`: 目标 regular inode。
    ///
    /// # Returns
    ///
    /// 可在本次 I/O 内复用的 facade；volatile inode 不注册全局 cache entry。
    ///
    /// # Errors
    ///
    /// - `InvalidOperation` 表示 inode 不是 regular file。
    /// - inode metadata 读取失败时透传 filesystem error。
    /// - 首次注册 memory reclaimer 失败时返回 `OutOfMemory`。
    pub(crate) fn from_inode(inode: Arc<dyn Inode>) -> Result<Self, FileSystemError> {
        if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
            return Err(FileSystemError::InvalidOperation);
        }
        match inode.data_backing() {
            DataBacking::PageCache => Ok(Self(match cached_file(inode.clone())? {
                Some(file) => RegularFileBackend::Cached(file),
                None => RegularFileBackend::Snapshot(inode),
            })),
            DataBacking::Snapshot => Ok(Self(RegularFileBackend::Snapshot(inode))),
            DataBacking::Memory(file) => Ok(Self(RegularFileBackend::Memory { file, inode })),
        }
    }

    /// 普通 page-cache/shmem read 执行 file_accessed；snapshot 专属 file operations 不伪造 generic atime。
    pub(crate) fn tracks_access_time(&self) -> bool {
        !matches!(self.0, RegularFileBackend::Snapshot(_))
    }

    /// 返回持久文件的唯一 page-cache backing identity。
    ///
    /// # Returns
    ///
    /// 持久文件返回 filesystem/inode identity；动态快照没有 cache identity，返回 None。
    pub(crate) fn id(&self) -> Option<SharedFileId> {
        match &self.0 {
            RegularFileBackend::Cached(file) => Some(file.id),
            RegularFileBackend::Memory { file, .. } => Some(file.id()),
            RegularFileBackend::Snapshot(_) => None,
        }
    }

    /// 返回当前 regular-file byte size。
    ///
    /// # Returns
    ///
    /// filesystem metadata owner 的当前 i_size 投影。
    pub(crate) fn size(&self) -> u64 {
        match &self.0 {
            RegularFileBackend::Cached(file) => file.inode.size(),
            RegularFileBackend::Memory { file, .. } => file.size(),
            RegularFileBackend::Snapshot(inode) => inode.size(),
        }
    }

    /// 从持久 page cache 或只读动态 inode 读取 regular-file bytes。
    ///
    /// # Parameters
    ///
    /// - `offset`: 文件 byte offset。
    /// - `output`: kernel-owned 输出缓冲区。
    ///
    /// # Returns
    ///
    /// 实际读取字节数；EOF 返回零。
    ///
    /// # Errors
    ///
    /// - cache fill 分配失败时返回 `OutOfMemory`。
    /// - size snapshot 后并发 truncate 越过当前 page 时返回 `InvalidOperation`。
    /// - storage read 失败或短读时返回对应 filesystem error。
    pub(crate) fn read(
        &self,
        offset: u64,
        output: &mut [u8],
    ) -> Result<RegularFileRead, FileSystemError> {
        let file = match &self.0 {
            RegularFileBackend::Cached(file) => file,
            RegularFileBackend::Snapshot(inode) => {
                return inode
                    .read_storage(offset, output)
                    .map(|bytes| RegularFileRead {
                        bytes,
                        storage_bytes: 0,
                    });
            }
            RegularFileBackend::Memory { file, .. } => {
                return file.read(offset, output).map(|bytes| RegularFileRead {
                    bytes,
                    storage_bytes: 0,
                });
            }
        };
        let size = file.inode.size();
        let count = usize::try_from(size.saturating_sub(offset))
            .unwrap_or(usize::MAX)
            .min(output.len());
        let mut done = 0;
        let mut storage_bytes = 0;
        while done < count {
            let current = offset + done as u64;
            let (page, filled) = file.page_with_storage(current / PAGE_SIZE as u64)?;
            storage_bytes += filled;
            let page_offset = current as usize % PAGE_SIZE;
            let part = (PAGE_SIZE - page_offset).min(count - done);
            page.frame.read(page_offset, &mut output[done..done + part]);
            done += part;
        }
        Ok(RegularFileRead {
            bytes: done,
            storage_bytes,
        })
    }

    /// 开始一次不可被其他 regular-file mutation 穿插的 write operation。
    ///
    /// # Returns
    ///
    /// 持有 per-inode write-sequence gate 的 mutation facade；Drop 自动释放。
    ///
    /// # Errors
    ///
    /// 只读动态 inode 返回 `ReadOnly`。
    pub(crate) fn begin_write(
        &self,
        identity: AccessIdentity,
    ) -> Result<RegularFileWrite<'_>, FileSystemError> {
        match &self.0 {
            RegularFileBackend::Cached(file) => {
                let sequence = file
                    .write_sequence
                    .lock()
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                if file.inode.is_swap_file() {
                    return Err(FileSystemError::TextBusy);
                }
                Ok(RegularFileWrite::Cached(CachedWrite {
                    file,
                    identity,
                    _sequence: sequence,
                }))
            }
            RegularFileBackend::Memory { file, inode } => {
                Ok(RegularFileWrite::Memory(MemoryInodeWrite {
                    write: file.begin_write()?,
                    inode,
                    identity,
                }))
            }
            RegularFileBackend::Snapshot(_) => Err(FileSystemError::ReadOnly),
        }
    }
}
