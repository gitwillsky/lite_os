//! ext4 的不阻塞读：把 `read_storage` 的“映射 → 读块”拆成可恢复的步进，调用者在块读未完成时让出而不是睡眠。
//!
//! 与同步路径共享同一套一致性规则，而不是另造一套：
//!
//! 1. 每次读块先查 journal 暂存（命中即同步完成），miss 才读 home；
//! 2. 读 home 前后比较 `metadata_cache.generation()`，变了就丢弃重读，绝不返回撕裂的 image
//!    （同步路径 `read_fs_block` 的循环）；
//! 3. extent 索引节点经 `insert_if_unchanged` 进入 metadata cache（同步路径 `read_metadata_block`）。
//!
//! 只支持文件系统块等于设备块的映射；其余情形、请求槽暂满、元数据反复失效都上报 `Fallback`，
//! 由调用者改走同步路径。

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::task::Poll;

use super::Ext4InodeDisk;
use super::{
    Ext4FileSystem, FileSystemError, block_error,
    extent::{ExtentTree, Mapping, Walk},
};
use crate::{
    devices::block::{BlockError, CompletionNotify, PendingRead},
    fs::{StorageRead, StorageStep},
};

/// 元数据反复失效（并发 checkpoint）时放弃异步，退回同步路径的次数上限；缺失该上限，持续写入的文件系统
/// 会让一次读无限重试。
const MAX_METADATA_ATTEMPTS: u8 = 8;

enum ReadError {
    Fs(FileSystemError),
    Fallback,
}

impl From<FileSystemError> for ReadError {
    fn from(error: FileSystemError) -> Self {
        ReadError::Fs(error)
    }
}

fn device_error(error: BlockError) -> ReadError {
    match error {
        // 没有空闲请求槽：同步路径会排队等待，异步路径让调用者改走它。
        BlockError::WouldBlock => ReadError::Fallback,
        other => ReadError::Fs(block_error(other)),
    }
}

/// 一个 fs 块的读：journal 暂存命中同步完成，否则异步读 home 并在完成时校验 generation。
struct FsBlockRead {
    fs: Arc<Ext4FileSystem>,
    block: u64,
    notify: Arc<dyn CompletionNotify>,
    generation: u64,
    pending: Option<Box<dyn PendingRead>>,
    bytes: Vec<u8>,
}

impl FsBlockRead {
    fn begin(
        fs: &Arc<Ext4FileSystem>,
        block: u64,
        notify: &Arc<dyn CompletionNotify>,
    ) -> Result<Self, ReadError> {
        if block >= fs.superblock.lock().blocks_count() {
            return Err(FileSystemError::InvalidFileSystem.into());
        }
        let mut read = Self {
            fs: fs.clone(),
            block,
            notify: notify.clone(),
            generation: 0,
            pending: None,
            bytes: super::try_zeroed(fs.block_size)?,
        };
        read.start()?;
        Ok(read)
    }

    /// 取 generation → 查 journal 暂存 → 未命中则提交 home 读。
    fn start(&mut self) -> Result<(), ReadError> {
        self.generation = self.fs.metadata_cache.lock().generation();
        if self
            .fs
            .journal
            .lock()
            .copy_staged(self.block, &mut self.bytes)
        {
            self.pending = None;
            return Ok(());
        }
        self.pending = Some(
            self.fs
                .device
                .clone()
                .begin_read(self.block as usize, self.notify.clone())
                .map_err(device_error)?,
        );
        Ok(())
    }

    fn poll(&mut self) -> Result<Poll<()>, ReadError> {
        loop {
            let Some(pending) = &self.pending else {
                return Ok(Poll::Ready(()));
            };
            if !pending.is_complete() {
                return Ok(Poll::Pending);
            }
            let pending = self.pending.take().expect("checked above");
            pending.finish(&mut self.bytes).map_err(device_error)?;
            if self.fs.metadata_cache.lock().generation() == self.generation {
                return Ok(Poll::Ready(()));
            }
            // journal miss 与 home I/O 之间开始了 checkpoint：这份 image 可能撕裂，重读。
            self.start()?;
        }
    }
}

/// 一个 extent 节点的读入并缓存。
struct MetadataFetch {
    read: FsBlockRead,
    /// 首次 miss 时的 generation；cache 在此之后变化则不能插入这份 image。
    generation: u64,
}

impl MetadataFetch {
    fn begin(
        fs: &Arc<Ext4FileSystem>,
        block: u64,
        notify: &Arc<dyn CompletionNotify>,
    ) -> Result<Self, ReadError> {
        let generation = fs.metadata_cache.lock().generation();
        Ok(Self {
            read: FsBlockRead::begin(fs, block, notify)?,
            generation,
        })
    }

    fn poll(&mut self) -> Result<Poll<()>, ReadError> {
        loop {
            if self.read.poll()?.is_pending() {
                return Ok(Poll::Pending);
            }
            let fs = self.read.fs.clone();
            let mut cache = fs.metadata_cache.lock();
            if cache.generation() == self.generation {
                let bytes = Arc::try_new(core::mem::take(&mut self.read.bytes))
                    .map_err(|_| FileSystemError::OutOfMemory)?;
                cache.insert_if_unchanged(self.generation, self.read.block, bytes);
                return Ok(Poll::Ready(()));
            }
            if cache.get(self.read.block).is_some() {
                return Ok(Poll::Ready(()));
            }
            self.generation = cache.generation();
            drop(cache);
            self.read.start()?;
        }
    }
}

enum State {
    Idle,
    Meta(MetadataFetch),
    Data {
        read: FsBlockRead,
        within: usize,
        length: usize,
    },
}

/// 一个 ext4 inode 上一段字节范围的不阻塞读。
pub(super) struct Ext4Read {
    fs: Arc<Ext4FileSystem>,
    inode_num: u32,
    /// 开始时的 inode 快照；之后的并发 truncate/write 由 page cache 的 epoch 协议丢弃本次填充。
    disk: Ext4InodeDisk,
    notify: Arc<dyn CompletionNotify>,
    offset: usize,
    to_read: usize,
    done: usize,
    attempts: u8,
    state: State,
}

impl Ext4Read {
    pub(super) fn new(
        fs: &Arc<Ext4FileSystem>,
        inode_num: u32,
        disk: Ext4InodeDisk,
        offset: u64,
        length: usize,
        notify: Arc<dyn CompletionNotify>,
    ) -> Result<Self, FileSystemError> {
        let size = usize::try_from(disk.size()).map_err(|_| FileSystemError::InvalidOperation)?;
        let offset = usize::try_from(offset).map_err(|_| FileSystemError::InvalidOperation)?;
        let to_read = if offset >= size {
            0
        } else {
            length.min(size - offset)
        };
        Ok(Self {
            fs: fs.clone(),
            inode_num,
            disk,
            notify,
            offset,
            to_read,
            done: 0,
            attempts: 0,
            state: State::Idle,
        })
    }

    fn advance(&mut self, output: &mut [u8]) -> Result<StorageStep, ReadError> {
        loop {
            match core::mem::replace(&mut self.state, State::Idle) {
                State::Idle => {
                    if self.done == self.to_read {
                        return Ok(StorageStep::Done(self.done));
                    }
                    let block_size = self.fs.block_size;
                    let current = self.offset + self.done;
                    let index = (current / block_size) as u32;
                    let within = current % block_size;
                    let length = (block_size - within).min(self.to_read - self.done);
                    let tree = ExtentTree::new(&self.fs, self.inode_num, &self.disk)?;
                    match tree.lookup_cached(index)? {
                        Walk::Missing(block) => {
                            self.attempts += 1;
                            if self.attempts > MAX_METADATA_ATTEMPTS {
                                return Err(ReadError::Fallback);
                            }
                            self.state =
                                State::Meta(MetadataFetch::begin(&self.fs, block, &self.notify)?);
                        }
                        Walk::Found(Mapping::Mapped(extent)) if !extent.unwritten => {
                            let block = extent.physical + u64::from(index - extent.logical);
                            self.state = State::Data {
                                read: FsBlockRead::begin(&self.fs, block, &self.notify)?,
                                within,
                                length,
                            };
                        }
                        // hole 与 unwritten extent 都读为零。
                        Walk::Found(_) => {
                            output[self.done..self.done + length].fill(0);
                            self.done += length;
                        }
                    }
                }
                State::Meta(mut fetch) => {
                    if fetch.poll()?.is_pending() {
                        self.state = State::Meta(fetch);
                        return Ok(StorageStep::Wait);
                    }
                }
                State::Data {
                    mut read,
                    within,
                    length,
                } => {
                    if read.poll()?.is_pending() {
                        self.state = State::Data {
                            read,
                            within,
                            length,
                        };
                        return Ok(StorageStep::Wait);
                    }
                    output[self.done..self.done + length]
                        .copy_from_slice(&read.bytes[within..within + length]);
                    self.done += length;
                }
            }
        }
    }
}

impl StorageRead for Ext4Read {
    fn step(&mut self, output: &mut [u8]) -> Result<StorageStep, FileSystemError> {
        match self.advance(output) {
            Ok(step) => Ok(step),
            Err(ReadError::Fs(error)) => Err(error),
            Err(ReadError::Fallback) => Ok(StorageStep::Fallback),
        }
    }
}
