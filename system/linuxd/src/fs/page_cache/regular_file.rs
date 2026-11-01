use super::*;

/// 持久 page-cache、per-open 直接 I/O 与内存/快照共用的 regular-file I/O facade。
///
/// syscall 在一次 read/write 操作内复用该值，避免每个 user-copy chunk 重复读取 inode
/// metadata、获取全局 FILES lock 并查找同一个 ordered entry。
pub(crate) struct RegularFile(RegularFileBackend);

enum RegularFileBackend {
    Cached(Arc<CachedFile>),
    Direct {
        file: Arc<CachedFile>,
        io: Arc<dyn super::DirectIo>,
    },
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
    /// 本次调用因 cache miss 填充或直接 I/O 从 filesystem 成功读取的 bytes。
    pub(crate) storage_bytes: usize,
}

impl RegularFile {
    /// 由 `inode` 与实际打开的可选 `io` 建立 facade；返回共享 cache owner 的 I/O 视图。
    /// 直接 I/O 只适用于持久 backing；传播 inode/cache 分配与准入错误。
    pub(crate) fn from_open(
        inode: Arc<dyn Inode>,
        io: Option<Arc<dyn super::DirectIo>>,
    ) -> Result<Self, FileSystemError> {
        let file = Self::from_inode(inode)?;
        match (file.0, io) {
            (RegularFileBackend::Cached(file), Some(io)) => {
                Ok(Self(RegularFileBackend::Direct { file, io }))
            }
            (backend, None) => Ok(Self(backend)),
            (_, Some(_)) => Err(FileSystemError::InvalidOperation),
        }
    }

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
            RegularFileBackend::Cached(file) | RegularFileBackend::Direct { file, .. } => {
                Some(file.id)
            }
            RegularFileBackend::Memory { file, .. } => Some(file.id()),
            RegularFileBackend::Snapshot(_) => None,
        }
    }

    /// 是否支持现有的非阻塞 cache fill；直接 I/O 交给 io_uring 阻塞 worker，不能从缓存返回。
    pub(crate) fn supports_nowait(&self) -> bool {
        matches!(self.0, RegularFileBackend::Cached(_))
    }

    /// 开始一段不阻塞的 cache 读；其余后端返回 None，由调用者执行阻塞路径。
    /// 直接 I/O 可能等待服务端，io_uring 必须派给 worker，不能在提交线程调用 read。
    pub(crate) fn begin_read_nowait(
        &self,
        offset: u64,
        length: usize,
        notify: Arc<dyn CompletionNotify>,
    ) -> Option<NowaitRead> {
        match &self.0 {
            RegularFileBackend::Cached(file) => {
                Some(NowaitRead::new(file.clone(), offset, length, notify))
            }
            _ => None,
        }
    }

    /// 返回当前 regular-file byte size。
    ///
    /// # Returns
    ///
    /// filesystem metadata owner 的当前 i_size 投影。
    pub(crate) fn size(&self) -> u64 {
        match &self.0 {
            RegularFileBackend::Cached(file) | RegularFileBackend::Direct { file, .. } => {
                file.inode.size()
            }
            RegularFileBackend::Memory { file, .. } => file.size(),
            RegularFileBackend::Snapshot(inode) => inode.size(),
        }
    }

    /// 给定 `offset` 与请求 `limit`，返回本次 copy 可尝试的长度；直接读由服务端短读判定结束。
    pub(crate) fn read_limit(&self, offset: u64, limit: usize) -> usize {
        match self.0 {
            RegularFileBackend::Direct { .. } => limit,
            _ => usize::try_from(self.size().saturating_sub(offset))
                .unwrap_or(usize::MAX)
                .min(limit),
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
            RegularFileBackend::Direct { file, io } => {
                if output.is_empty() {
                    return Ok(RegularFileRead {
                        bytes: 0,
                        storage_bytes: 0,
                    });
                }
                let _operation = file
                    .operation
                    .lock()
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                file.writeback_range(offset, output.len() as u64, false)?;
                // 不看缓存 EOF、不填页、不补零；短读长度与每次打开的 fh 都属于服务端。
                let bytes = io.read(offset, output)?;
                return Ok(RegularFileRead {
                    bytes,
                    storage_bytes: bytes,
                });
            }
            RegularFileBackend::Cached(file) => {
                file.inode.revalidate_data()?;
                file
            }
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
    /// `identity` 为实际 caller，`append` 为本次 syscall 冻结的追加语义；直接追加先刷新 i_size。
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
        append: bool,
    ) -> Result<RegularFileWrite<'_>, FileSystemError> {
        match &self.0 {
            RegularFileBackend::Cached(file) | RegularFileBackend::Direct { file, .. } => {
                let sequence = file
                    .write_sequence
                    .lock()
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                if file.inode.is_swap_file() {
                    return Err(FileSystemError::TextBusy);
                }
                let cached = CachedWrite {
                    file,
                    identity,
                    _sequence: sequence,
                };
                if let RegularFileBackend::Direct { io, .. } = &self.0 {
                    // 追加的 range/limit 检查前刷新一次 i_size，后续 user-copy 与提交共享该 write_sequence。
                    if append {
                        io.size()?;
                    }
                    Ok(RegularFileWrite::Direct(super::DirectWrite {
                        cached,
                        io: &**io,
                    }))
                } else {
                    Ok(RegularFileWrite::Cached(cached))
                }
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
