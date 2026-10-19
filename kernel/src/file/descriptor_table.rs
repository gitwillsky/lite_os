use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{Ordering, fence};

use super::{Epoll, OpenFileDescription, PreparedFileDescription};
use crate::fs::{AccessIdentity, FileSystemError, O_RDONLY, O_WRONLY, vfs};

mod indexed_slots;

use indexed_slots::{IndexedSlots, SlotInsertError};

pub(crate) const MAX_FILE_DESCRIPTORS: usize = indexed_slots::MAX_FILE_DESCRIPTORS;

/// fd-table 查找、resource limit 与 owner metadata OOM 的稳定失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileDescriptorError {
    NotFound,
    Limit,
    OutOfMemory,
    Busy,
}

impl From<SlotInsertError> for FileDescriptorError {
    fn from(error: SlotInsertError) -> Self {
        match error {
            SlotInsertError::Limit => Self::Limit,
            SlotInsertError::OutOfMemory => Self::OutOfMemory,
        }
    }
}

struct FileDescriptor {
    ofd: Option<Arc<OpenFileDescription>>,
    cloexec: bool,
    published: bool,
}

const _: () = assert!(
    core::mem::size_of::<Option<FileDescriptor>>() == 16,
    "fd-index memory proof assumes the reviewed LP64 descriptor slot layout"
);

impl FileDescriptor {
    fn new(ofd: Arc<OpenFileDescription>, cloexec: bool) -> Self {
        Self::with_state(Some(ofd), cloexec, true)
    }

    fn reserved(ofd: Option<Arc<OpenFileDescription>>, cloexec: bool) -> Self {
        Self::with_state(ofd, cloexec, false)
    }

    fn with_state(ofd: Option<Arc<OpenFileDescription>>, cloexec: bool, published: bool) -> Self {
        // fd table lock/Process publication owns entry visibility；该原子只计数，不发布 OFD 数据，
        // 因此 increment 使用 Relaxed。缺少 increment 会让任一 close 提前删除仍存活的 interest。
        if published {
            ofd.as_ref()
                .expect("published descriptor requires OFD")
                .descriptor_refs
                .fetch_add(1, Ordering::Relaxed);
        }
        Self {
            ofd,
            cloexec,
            published,
        }
    }

    fn file(&self) -> &Arc<OpenFileDescription> {
        self.ofd
            .as_ref()
            .expect("file operation requires prepared OFD")
    }
}

impl Clone for FileDescriptor {
    fn clone(&self) -> Self {
        Self::with_state(self.ofd.clone(), self.cloexec, self.published)
    }
}

impl Drop for FileDescriptor {
    fn drop(&mut self) {
        if !self.published {
            return;
        }
        // Release/Acquire 与其他 fd table 的最后 decrement 配对，确保判定为最后引用后才执行
        // 全局 cleanup；缺少原子 RMW 会让 fork 后两个 table 同时误判生命周期。
        if self.file().descriptor_refs.fetch_sub(1, Ordering::Release) == 1 {
            fence(Ordering::Acquire);
            Epoll::release_file(self.file());
            vfs().release_advisory_lock(self.file().lock_holder().identity);
        }
    }
}

/// 已从 fd table 原子摘除、等待在 Process files lock 外完成析构的 entry。
pub(crate) struct DetachedFileDescriptor(FileDescriptor);

/// 已回滚、等待在 Process files lock 外析构的不可见 fd slot。
pub(crate) struct CancelledFileReservation {
    _descriptor: FileDescriptor,
}

impl DetachedFileDescriptor {
    /// 完成 descriptor_refs/epoll/flock cleanup，并保留 OFD 供 record-lock cleanup。
    ///
    /// # Returns
    ///
    /// 被关闭 descriptor 原先引用的 OFD。
    pub(crate) fn finish_close(self) -> Arc<OpenFileDescription> {
        let ofd = self.0.file().clone();
        drop(self);
        ofd
    }
}

/// 进程 fd table；slot、FD_CLOEXEC 与 descriptor publication 的唯一 owner。
pub(crate) struct FileDescriptorTable {
    slots: IndexedSlots<FileDescriptor>,
}

const _: () = assert!(
    core::mem::size_of::<FileDescriptorTable>() == 24,
    "fd table inline size proof assumes one lazy radix pointer, logical length and free prefix"
);

impl FileDescriptorTable {
    fn empty() -> Self {
        Self {
            slots: IndexedSlots::new(),
        }
    }

    /// 返回当前 fd table 已发布过的 logical slot capacity。
    ///
    /// # Returns
    ///
    /// 包含空洞与未物化 radix path 的容量，对应 Linux `/proc/<pid>/status` FDSize。
    pub(crate) fn slot_capacity(&self) -> usize {
        self.slots.len()
    }

    /// 只遍历并复制 materialized published entries，同时保持每个 entry 共享原 OFD Arc。
    ///
    /// # Returns
    ///
    /// 成功返回独立 descriptor table；kernel heap 耗尽会回滚已克隆引用并返回错误。
    pub(crate) fn try_clone(&self) -> Result<Self, ()> {
        let slots = self.slots.try_clone_where(|entry| entry.published)?;
        Ok(Self { slots })
    }

    /// 构造 init 的三个 inherited console descriptor。
    ///
    /// `/dev/console` 从已挂载 devfs 解析一次，经字符设备注册表打开；fd 0/1/2 分别为只读、
    /// 只写、只写 OFD。
    ///
    /// # Errors
    ///
    /// console 设备打开或 descriptor storage 分配失败返回对应 filesystem error。
    pub(crate) fn with_console() -> Result<Self, FileSystemError> {
        let backing_opened = vfs()
            .open_file(b"/dev/console")
            .expect("mounted console device must resolve");
        let number = backing_opened
            .inode()
            .device_number()
            .expect("/dev/console must carry a device number");
        let identity = AccessIdentity::root();
        let open = |flags| {
            let prepared = PreparedFileDescription::try_new()?;
            let prepared = prepared.prepare_named(backing_opened.clone(), flags)?;
            crate::fs::chardev::open(&crate::fs::chardev::OpenRequest {
                number,
                flags,
                identity: &identity,
            })
            .map(|file| prepared.finish(crate::fs::InodeOpen::Device(file)))
        };
        let input = open(O_RDONLY)?;
        let output = open(O_WRONLY)?;
        let error = open(O_WRONLY)?;
        let mut table = Self::empty();
        table
            .slots
            .insert_pair_with(3, || {
                (
                    FileDescriptor::new(input, false),
                    FileDescriptor::new(output, false),
                )
            })
            .map_err(|_| FileSystemError::OutOfMemory)?;
        table
            .slots
            .insert_with(0, 3, || FileDescriptor::new(error, false))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        Ok(table)
    }

    pub(crate) fn get(&self, fd: usize) -> Option<Arc<OpenFileDescription>> {
        self.slots
            .get(fd)
            .filter(|entry| entry.published)
            .map(|entry| entry.file().clone())
    }
    /// 单一 table guard 下捕获实际 OFD 与该 descriptor CLOEXEC；不可见 reservation 不泄漏。
    pub(crate) fn get_with_flags(&self, fd: usize) -> Option<(Arc<OpenFileDescription>, u32)> {
        self.slots
            .get(fd)
            .filter(|entry| entry.published)
            .map(|entry| (entry.file().clone(), u32::from(entry.cloexec)))
    }

    /// 在一次 fd-table owner lock 内捕获完整 SCM_RIGHTS descriptor 集合。
    ///
    /// # Parameters
    ///
    /// - `descriptors`: 用户 cmsg 中按序验证的 descriptor numbers。
    ///
    /// # Returns
    ///
    /// 与输入同序、共享原 OFD 的 Arc 集合。
    ///
    /// # Errors
    ///
    /// 任一 fd 不存在时整批返回 NotFound；staging OOM 返回 OutOfMemory。
    pub(crate) fn capture_many(
        &self,
        descriptors: &[usize],
    ) -> Result<Vec<Arc<OpenFileDescription>>, FileDescriptorError> {
        let mut files = Vec::new();
        files
            .try_reserve_exact(descriptors.len())
            .map_err(|_| FileDescriptorError::OutOfMemory)?;
        for &fd in descriptors {
            files.push(self.get(fd).ok_or(FileDescriptorError::NotFound)?);
        }
        Ok(files)
    }

    pub(crate) fn allocate(
        &mut self,
        ofd: Arc<OpenFileDescription>,
        minimum: usize,
        cloexec: bool,
        limit: usize,
    ) -> Result<usize, FileDescriptorError> {
        self.slots
            .insert_with(minimum, limit, || FileDescriptor::new(ofd, cloexec))
            .map_err(Into::into)
    }

    /// 原子分配 pipe/socketpair 的两个 descriptor entry。
    ///
    /// # Parameters
    ///
    /// - `first`: 第一个 OFD。
    /// - `second`: 第二个 OFD。
    /// - `cloexec`: 两个 descriptor 的 FD_CLOEXEC 初值。
    /// - `limit`: Process 当前 fd limit。
    ///
    /// # Returns
    ///
    /// 两个 fd；容量不足时 fd table 不变。
    #[cfg(test)]
    pub(crate) fn allocate_pair(
        &mut self,
        first: Arc<OpenFileDescription>,
        second: Arc<OpenFileDescription>,
        cloexec: bool,
        limit: usize,
    ) -> Result<(usize, usize), FileDescriptorError> {
        self.slots
            .insert_pair_with(limit, || {
                (
                    FileDescriptor::new(first, cloexec),
                    FileDescriptor::new(second, cloexec),
                )
            })
            .map_err(Into::into)
    }

    /// 在任何 filesystem/device 副作用前占用最低空闲 fd；不预构造假 OFD。
    ///
    /// # Parameters
    /// - `cloexec`: 成功发布时的 FD_CLOEXEC。
    /// - `limit`: caller 的 fd resource limit。
    /// # Returns
    /// 不可由 get/close/dup/proc/fork 观察的 fd number。
    /// # Errors
    /// Limit 或 backing OOM；失败不占用 slot，也不调用 filesystem。
    pub(crate) fn reserve_open(
        &mut self,
        cloexec: bool,
        limit: usize,
    ) -> Result<usize, FileDescriptorError> {
        self.slots
            .insert_with(0, limit, || FileDescriptor::reserved(None, cloexec))
            .map_err(Into::into)
    }

    /// 消费普通 open 的 reservation，无分配安装实际 OFD 并原子发布。
    ///
    /// `fd` 必须仍由原 reservation 独占；成功前不增加 descriptor_refs。
    /// # Returns
    /// 无返回值；错误 owner/token 在改变可见性前 fail-stop。
    pub(crate) fn publish_open(&mut self, fd: usize, file: Arc<OpenFileDescription>) {
        let entry = self
            .slots
            .get_mut(fd)
            .expect("open reservation disappeared before publication");
        assert!(
            !entry.published && entry.ofd.is_none(),
            "open publication requires empty reservation"
        );
        entry.ofd = Some(file);
        entry.file().descriptor_refs.fetch_add(1, Ordering::Relaxed);
        entry.published = true;
    }

    /// 无分配摘除 copyout 或 preparation 失败的实际 fd reservation。
    ///
    /// # Parameters
    ///
    /// - `fd`: 仍不可见的 reserved slot。
    ///
    /// # Returns
    ///
    /// 独立 cleanup capability，调用者必须在 files lock 外析构。
    pub(crate) fn cancel_reserved(&mut self, fd: usize) -> CancelledFileReservation {
        CancelledFileReservation {
            _descriptor: self
                .slots
                .take_if(fd, |entry| !entry.published)
                .expect("fd reservation disappeared before rollback"),
        }
    }

    /// 原子摘除一个 entry，不在 fd-table owner lock 内执行其 Drop cleanup。
    ///
    /// # Parameters
    ///
    /// - `fd`: 待关闭 descriptor。
    ///
    /// # Returns
    ///
    /// detached entry；空洞或越界返回错误。
    pub(crate) fn detach(&mut self, fd: usize) -> Result<DetachedFileDescriptor, ()> {
        self.slots
            .take_if(fd, |entry| entry.published)
            .map(DetachedFileDescriptor)
            .ok_or(())
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

    /// 原子发布目标 descriptor，并 detach 被替换 entry 供锁外 cleanup。
    ///
    /// # Returns
    ///
    /// 旧目标 entry；目标原为空洞时返回 None。
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
        if self.slots.get(new).is_some_and(|entry| !entry.published) {
            return Err(FileDescriptorError::Busy);
        }
        Ok(self
            .slots
            .replace_with(new, limit, || FileDescriptor::new(ofd, cloexec))?
            .map(DetachedFileDescriptor))
    }

    /// 接收一个实际 OFD；指定 slot 时原子替换，旧 descriptor 在表锁外完成 cleanup。
    /// # Errors
    /// 精确目标超出 rlimit 返回 NotFound（Linux replace_fd 的 EBADF）；reservation 冲突 Busy。
    pub(crate) fn receive(
        &mut self,
        file: Arc<OpenFileDescription>,
        destination: Option<usize>,
        cloexec: bool,
        limit: usize,
    ) -> Result<(usize, Option<DetachedFileDescriptor>), FileDescriptorError> {
        let Some(fd) = destination else {
            return self.allocate(file, 0, cloexec, limit).map(|fd| (fd, None));
        };
        if fd >= limit.min(MAX_FILE_DESCRIPTORS) {
            return Err(FileDescriptorError::NotFound);
        }
        if self.slots.get(fd).is_some_and(|entry| !entry.published) {
            return Err(FileDescriptorError::Busy);
        }
        let retired = self
            .slots
            .replace_with(fd, limit, || FileDescriptor::new(file, cloexec))?
            .map(DetachedFileDescriptor);
        Ok((fd, retired))
    }

    pub(crate) fn descriptor_flags(&self, fd: usize) -> Result<u32, ()> {
        let entry = self
            .slots
            .get(fd)
            .filter(|entry| entry.published)
            .ok_or(())?;
        Ok(if entry.cloexec { 1 } else { 0 })
    }

    pub(crate) fn set_descriptor_flags(&mut self, fd: usize, flags: u32) -> Result<(), ()> {
        self.slots
            .get_mut(fd)
            .filter(|entry| entry.published)
            .ok_or(())?
            .cloexec = flags & 1 != 0;
        Ok(())
    }

    /// 从 cursor 单调扫描并 detach 一批 FD_CLOEXEC entries。
    ///
    /// # Parameters
    ///
    /// - `cursor`: 下一待检查 slot；每个 slot 在一次 exec cleanup 中只访问一次。
    /// - `output`: caller 提供的非空、已清空固定栈 batch。
    ///
    /// # Returns
    ///
    /// 本批 detached entry 数；零表示 cursor 已到 table 末尾。
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
        while count < output.len() {
            let Some(fd) = self
                .slots
                .find_from(*cursor, |entry| entry.published && entry.cloexec)
            else {
                *cursor = self.slots.len();
                break;
            };
            *cursor = fd + 1;
            let entry = self
                .slots
                .take(fd)
                .expect("sparse CLOEXEC candidate disappeared before detach");
            output[count] = Some(DetachedFileDescriptor(entry));
            count += 1;
        }
        count
    }

    /// 在 fd-table lock 内复制 live descriptor/OFD identity，供 procfs 锁外解析路径。
    ///
    /// # Returns
    ///
    /// 按 fd 递增的 `(descriptor, OFD)` 快照；内存不足返回错误。
    pub(crate) fn snapshot(&self) -> Result<Vec<(usize, Arc<OpenFileDescription>)>, ()> {
        let count = self
            .slots
            .iter()
            .filter(|(_, entry)| entry.published)
            .count();
        let mut snapshot = Vec::new();
        snapshot.try_reserve_exact(count).map_err(|_| ())?;
        snapshot.extend(
            self.slots
                .iter()
                .filter(|(_, entry)| entry.published)
                .map(|(fd, entry)| (fd, entry.file().clone())),
        );
        Ok(snapshot)
    }
}
