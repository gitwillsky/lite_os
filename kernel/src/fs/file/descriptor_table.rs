use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use super::{O_RDONLY, O_WRONLY, OpenFileDescription, Terminal};
use crate::fs::{Epoll, vfs};

pub(crate) const MAX_FILE_DESCRIPTORS: usize = 1_048_576;

/// @description fd-table 查找、resource limit 与 owner metadata OOM 的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileDescriptorError {
    NotFound,
    Limit,
    OutOfMemory,
}

struct FileDescriptor {
    ofd: Arc<OpenFileDescription>,
    cloexec: bool,
}

impl FileDescriptor {
    fn new(ofd: Arc<OpenFileDescription>, cloexec: bool) -> Self {
        // fd table lock/Process publication owns entry visibility；该原子只计数，不发布 OFD 数据，
        // 因此 increment 使用 Relaxed。缺少 increment 会让任一 close 提前删除仍存活的 interest。
        ofd.descriptor_refs.fetch_add(1, Ordering::Relaxed);
        Self { ofd, cloexec }
    }
}

impl Clone for FileDescriptor {
    fn clone(&self) -> Self {
        Self::new(self.ofd.clone(), self.cloexec)
    }
}

impl Drop for FileDescriptor {
    fn drop(&mut self) {
        // Release/Acquire 与其他 fd table 的最后 decrement 配对，确保判定为最后引用后才执行
        // 全局 cleanup；缺少原子 RMW 会让 fork 后两个 table 同时误判生命周期。
        if self.ofd.descriptor_refs.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            Epoll::release_file(&self.ofd);
            vfs().release_advisory_lock(&self.ofd);
        }
    }
}

/// @description 已从 fd table 原子摘除、等待在 Process files lock 外完成析构的 entry。
pub(crate) struct DetachedFileDescriptor(FileDescriptor);

impl DetachedFileDescriptor {
    /// @description 完成 descriptor_refs/epoll/flock cleanup，并保留 OFD 供 record-lock cleanup。
    /// @return 被关闭 descriptor 原先引用的 OFD。
    pub(crate) fn finish_close(self) -> Arc<OpenFileDescription> {
        let ofd = self.0.ofd.clone();
        drop(self);
        ofd
    }
}

/// @description 进程 fd table；slot、FD_CLOEXEC 与 descriptor publication 的唯一 owner。
pub(crate) struct FileDescriptorTable {
    entries: Vec<Option<FileDescriptor>>,
}

impl FileDescriptorTable {
    fn ensure_len(&mut self, length: usize) -> Result<(), FileDescriptorError> {
        if length <= self.entries.len() {
            return Ok(());
        }
        self.entries
            .try_reserve_exact(length - self.entries.len())
            .map_err(|_| FileDescriptorError::OutOfMemory)?;
        self.entries.resize(length, None);
        Ok(())
    }

    /// @description 返回当前 fd table 已分配的 descriptor slot 数。
    /// @return 包含空洞的 slot 容量，对应 Linux `/proc/<pid>/status` FDSize。
    pub(crate) fn slot_capacity(&self) -> usize {
        self.entries.len()
    }

    /// @description 复制 fd entries，同时保持每个 entry 共享原 OFD Arc。
    /// @return 成功返回独立 descriptor table；kernel heap 耗尽返回错误。
    pub(crate) fn try_clone(&self) -> Result<Self, ()> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(self.entries.len())
            .map_err(|_| ())?;
        entries.extend(self.entries.iter().cloned());
        Ok(Self { entries })
    }

    /// @description 构造 init 的三个 inherited console descriptor。
    /// @param terminal 唯一 TTY owner；backing opened entry 从已挂载 devfs 解析一次。
    /// @return fd 0/1/2 分别为 console read/write/write OFD 的 descriptor table。
    pub(crate) fn with_terminal(terminal: Arc<Terminal>) -> Result<Self, ()> {
        let backing_opened = vfs()
            .open_file(b"/dev/console")
            .expect("mounted console device must resolve");
        let mut entries = Vec::new();
        entries.try_reserve_exact(3).map_err(|_| ())?;
        entries.extend([
            Some(FileDescriptor::new(
                OpenFileDescription::terminal(terminal.clone(), backing_opened.clone(), O_RDONLY)?,
                false,
            )),
            Some(FileDescriptor::new(
                OpenFileDescription::terminal(terminal.clone(), backing_opened.clone(), O_WRONLY)?,
                false,
            )),
            Some(FileDescriptor::new(
                OpenFileDescription::terminal(terminal, backing_opened, O_WRONLY)?,
                false,
            )),
        ]);
        Ok(Self { entries })
    }

    pub(crate) fn get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.entries
            .get(fd)?
            .as_ref()
            .map(|entry| entry.ofd.clone())
    }

    pub(crate) fn allocate(
        &mut self,
        ofd: Arc<OpenFileDescription>,
        minimum: usize,
        cloexec: bool,
        limit: usize,
    ) -> Result<usize, FileDescriptorError> {
        let limit = limit.min(MAX_FILE_DESCRIPTORS);
        if minimum >= limit {
            return Err(FileDescriptorError::Limit);
        }
        for fd in minimum..self.entries.len().min(limit) {
            if self.entries[fd].is_none() {
                self.entries[fd] = Some(FileDescriptor::new(ofd, cloexec));
                return Ok(fd);
            }
        }
        if self.entries.len() < minimum {
            self.ensure_len(minimum)?;
        }
        let fd = self.entries.len();
        if fd >= limit {
            return Err(FileDescriptorError::Limit);
        }
        self.entries
            .try_reserve(1)
            .map_err(|_| FileDescriptorError::OutOfMemory)?;
        self.entries.push(Some(FileDescriptor::new(ofd, cloexec)));
        Ok(fd)
    }

    /// @description 原子分配 pipe/socketpair 的两个 descriptor entry。
    /// @param first 第一个 OFD。
    /// @param second 第二个 OFD。
    /// @param cloexec 两个 descriptor 的 FD_CLOEXEC 初值。
    /// @param limit Process 当前 fd limit。
    /// @return 两个 fd；容量不足时 fd table 不变。
    pub(crate) fn allocate_pair(
        &mut self,
        first: Arc<OpenFileDescription>,
        second: Arc<OpenFileDescription>,
        cloexec: bool,
        limit: usize,
    ) -> Result<(usize, usize), FileDescriptorError> {
        let mut available = [usize::MAX; 2];
        let mut found = 0;
        for fd in 0..limit.min(MAX_FILE_DESCRIPTORS) {
            if self.entries.get(fd).is_none_or(Option::is_none) {
                available[found] = fd;
                found += 1;
                if found == 2 {
                    break;
                }
            }
        }
        if found != 2 {
            return Err(FileDescriptorError::Limit);
        }
        let required = available[1] + 1;
        self.ensure_len(required)?;
        self.entries[available[0]] = Some(FileDescriptor::new(first, cloexec));
        self.entries[available[1]] = Some(FileDescriptor::new(second, cloexec));
        Ok((available[0], available[1]))
    }

    /// @description 原子摘除一个 entry，不在 fd-table owner lock 内执行其 Drop cleanup。
    /// @param fd 待关闭 descriptor。
    /// @return detached entry；空洞或越界返回错误。
    pub(crate) fn detach(&mut self, fd: usize) -> Result<DetachedFileDescriptor, ()> {
        let entry = self.entries.get_mut(fd).ok_or(())?;
        Ok(DetachedFileDescriptor(entry.take().ok_or(())?))
    }

    /// @description 从 live Process 原子取走全部 fd entry，供 exit 在 files lock 外关闭。
    /// @return 拥有原全部 entry 的独立 table；self 变为空 table。
    pub(crate) fn take_all(&mut self) -> Self {
        Self {
            entries: core::mem::take(&mut self.entries),
        }
    }

    pub(crate) fn duplicate(
        &mut self,
        old: usize,
        minimum: usize,
        cloexec: bool,
        limit: usize,
    ) -> Result<usize, FileDescriptorError> {
        let ofd = self.get(old).ok_or(FileDescriptorError::NotFound)?;
        self.allocate(ofd, minimum, cloexec, limit)
    }

    /// @description 原子发布目标 descriptor，并 detach 被替换 entry 供锁外 cleanup。
    /// @return 旧目标 entry；目标原为空洞时返回 None。
    pub(crate) fn duplicate_to(
        &mut self,
        old: usize,
        new: usize,
        cloexec: bool,
        limit: usize,
    ) -> Result<Option<DetachedFileDescriptor>, FileDescriptorError> {
        if new >= limit.min(MAX_FILE_DESCRIPTORS) {
            return Err(FileDescriptorError::Limit);
        }
        let ofd = self.get(old).ok_or(FileDescriptorError::NotFound)?;
        if self.entries.len() <= new {
            self.ensure_len(new + 1)?;
        }
        Ok(self.entries[new]
            .replace(FileDescriptor::new(ofd, cloexec))
            .map(DetachedFileDescriptor))
    }

    pub(crate) fn descriptor_flags(&self, fd: usize) -> Result<u32, ()> {
        Ok(
            if self
                .entries
                .get(fd)
                .and_then(Option::as_ref)
                .ok_or(())?
                .cloexec
            {
                1
            } else {
                0
            },
        )
    }

    pub(crate) fn set_descriptor_flags(&mut self, fd: usize, flags: u32) -> Result<(), ()> {
        self.entries
            .get_mut(fd)
            .and_then(Option::as_mut)
            .ok_or(())?
            .cloexec = flags & 1 != 0;
        Ok(())
    }

    /// @description 从 cursor 单调扫描并 detach 一批 FD_CLOEXEC entries。
    /// @param cursor 下一待检查 slot；每个 slot 在一次 exec cleanup 中只访问一次。
    /// @param output caller 提供的非空、已清空固定栈 batch。
    /// @return 本批 detached entry 数；零表示 cursor 已到 table 末尾。
    pub(crate) fn take_cloexec_batch(
        &mut self,
        cursor: &mut usize,
        output: &mut [Option<DetachedFileDescriptor>],
    ) -> usize {
        assert!(!output.is_empty(), "CLOEXEC close batch must not be empty");
        assert!(
            output.iter().all(Option::is_none),
            "CLOEXEC close batch still owns a detached descriptor"
        );
        let mut count = 0;
        while *cursor < self.entries.len() && count < output.len() {
            let slot = &mut self.entries[*cursor];
            *cursor += 1;
            if slot.as_ref().is_some_and(|entry| entry.cloexec) {
                output[count] = slot.take().map(DetachedFileDescriptor);
                count += 1;
            }
        }
        count
    }

    /// @description 在 fd-table lock 内复制 live descriptor/OFD identity，供 procfs 锁外解析路径。
    /// @return 按 fd 递增的 `(descriptor, OFD)` 快照；内存不足返回错误。
    pub(crate) fn snapshot(&self) -> Result<Vec<(usize, Arc<OpenFileDescription>)>, ()> {
        let count = self.entries.iter().filter(|entry| entry.is_some()).count();
        let mut snapshot = Vec::new();
        snapshot.try_reserve_exact(count).map_err(|_| ())?;
        snapshot.extend(
            self.entries
                .iter()
                .enumerate()
                .filter_map(|(fd, entry)| entry.as_ref().map(|entry| (fd, entry.ofd.clone()))),
        );
        Ok(snapshot)
    }
}
