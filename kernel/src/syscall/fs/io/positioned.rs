use super::*;

/// `pread64`/`preadv` 的入口：按当前 task 的 fd 表取得 OFD 后进入 [`positioned_read`]。
fn positioned_read_fd(fd: usize, vectors: &[UserIoVec], offset: i64) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pread64 requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    positioned_read(&task, &ofd, vectors, offset)
}

/// 从显式 offset 读取已取得的 OFD，不修改共享 OFD offset。
///
/// 显式接收 task 与 OFD，使 io_uring 的 worker 能以提交线程的地址空间执行它。
///
/// # Returns
///
/// byte count、EOF 零或负 errno。
pub(in crate::syscall) fn positioned_read(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    vectors: &[UserIoVec],
    offset: i64,
) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    if ofd.status_flags() & O_ACCMODE == O_WRONLY {
        return -errno::EBADF;
    }
    if let OpenFileKind::Device(file) = &ofd.kind
        && let Some(file) = file.positioned()
    {
        let total_length = vectors.iter().map(|vector| vector.length).sum();
        let result = crate::syscall::device::read_positioned_file(
            task,
            file,
            vectors,
            total_length,
            offset as u64,
        );
        task.account_read_result(result);
        if result > 0 {
            ofd.notify(crate::fs::IN_ACCESS);
        }
        return result;
    }
    // 路径打开的文件与 memfd 都有 inode 内容；pipe、socket 与字符设备没有可定位的内容。
    if !matches!(ofd.kind, OpenFileKind::Inode) {
        return -errno::ESPIPE;
    }
    let inode = ofd.inode_ref();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return -errno::ESPIPE;
    }
    if vectors.iter().all(|vector| vector.length == 0) {
        return 0;
    }
    let file = match ofd.regular_file() {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };
    let mut position = offset as u64;
    let result = read_regular_vectors(task, ofd, &file, &mut position, vectors);
    task.account_read_result(result);
    if result > 0 {
        ofd.notify(crate::fs::IN_ACCESS);
    }
    result
}

/// io_uring 的显式偏移 regular-file 读：不阻塞调用线程的状态机，语义与 [`positioned_read`] 相同。
///
/// 每一块从 page cache 同步复制，或开始一次异步填页并让出；块读完成后继续。被丢弃即取消，
/// 已交付给用户的部分不回滚（与同步读被信号打断后的 partial read 相同）。
pub(in crate::syscall) struct AsyncPositionedRead {
    ofd: Arc<OpenFileDescription>,
    file: RegularFile,
    /// `Some(start)`：这是 `off == -1` 的当前位置读，起点是发起时的共享 position，完成时发布 `start + 读到的字节`。
    /// 缺失时当前位置读不推进 position，下一次读会重复读同一区间。
    shared_start: Option<u64>,
    position: u64,
    progress: ReadProgress,
    chunk: Vec<u8>,
    current: Option<(crate::fs::NowaitRead, usize)>,
}

/// [`AsyncPositionedRead::step`] 的进展。
pub(in crate::syscall) enum AsyncReadStep {
    /// 读完，值同 [`positioned_read`]。
    Done(isize),
    /// 有块读在途；完成通知触发后再次调用。
    Wait,
    /// 无法继续不阻塞地读：调用 [`AsyncPositionedRead::finish_blocking`]。
    Fallback,
}

/// 判断一次显式偏移读能否走不阻塞路径并开始它。
///
/// 只覆盖 [`positioned_read`] 里“持久 page-cache 的 regular 文件”这一支；其余情形（设备文件、
/// 目录、写专用 OFD、全零长度、非 page-cache 后端）返回 `None`，调用者用同步的 [`positioned_read`]
/// 得到相同的错误或结果。
///
/// # Errors
///
/// 暂存缓冲区内存不足返回 `-ENOMEM`。
pub(in crate::syscall) fn begin_positioned_read(
    ofd: &Arc<OpenFileDescription>,
    vectors: &[UserIoVec],
    offset: i64,
) -> Result<Option<AsyncPositionedRead>, isize> {
    if offset < 0
        || ofd.status_flags() & O_ACCMODE == O_WRONLY
        || !matches!(ofd.kind, OpenFileKind::Inode)
        || vectors.iter().all(|vector| vector.length == 0)
    {
        return Ok(None);
    }
    let inode = ofd.inode_ref();
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return Ok(None);
    }
    let Ok(file) = ofd.regular_file() else {
        return Ok(None);
    };
    if !file.supports_nowait() {
        return Ok(None);
    }
    let mut chunk = Vec::new();
    chunk
        .try_reserve_exact(crate::memory::PAGE_SIZE)
        .map_err(|_| -errno::ENOMEM)?;
    chunk.resize(crate::memory::PAGE_SIZE, 0);
    Ok(Some(AsyncPositionedRead {
        ofd: ofd.clone(),
        file,
        shared_start: None,
        position: offset as u64,
        progress: ReadProgress::default(),
        chunk,
        current: None,
    }))
}

/// `off == -1` 的 regular-file 读：以发起时的共享 position 为起点开始不阻塞读，完成时发布新 position。
///
/// 与 Linux io_uring 一致，不在读期间持有 position（同步 `read(2)` 才持有）：并发的两个当前位置读
/// 可能读到同一区间，完成时各自把 position 写成 `start + 读到的字节`。
///
/// # Returns
///
/// 与 [`begin_positioned_read`] 相同；position 正被同步读写占用时也返回 `None`（调用者改走阻塞车道，
/// 因为 reactor 不能等待它）。
pub(in crate::syscall) fn begin_current_position_read(
    ofd: &Arc<OpenFileDescription>,
    vectors: &[UserIoVec],
) -> Result<Option<AsyncPositionedRead>, isize> {
    let Some(start) = ofd.try_with_position(|position| *position) else {
        return Ok(None);
    };
    let Ok(offset) = i64::try_from(start) else {
        return Ok(None);
    };
    let read = begin_positioned_read(ofd, vectors, offset)?;
    Ok(read.map(|mut read| {
        read.shared_start = Some(start);
        read
    }))
}

impl AsyncPositionedRead {
    /// 这次读所属的 OFD；io_uring 按 fd 取消时据此匹配。
    pub(in crate::syscall) fn ofd(&self) -> &Arc<OpenFileDescription> {
        &self.ofd
    }

    /// 发布当前位置读的新 position；非当前位置读或读到 0 字节时无事可做。
    ///
    /// # Returns
    ///
    /// `false` 表示 position 被占用、需要在可睡眠的线程里用 [`AsyncPositionedRead::publish_position_blocking`] 发布。
    pub(in crate::syscall) fn publish_position(&self, result: isize) -> bool {
        match self.shared_start {
            Some(start) if result > 0 => self
                .ofd
                .try_with_position(|position| *position = start + result as u64)
                .is_some(),
            _ => true,
        }
    }

    /// 可睡眠版本的 [`AsyncPositionedRead::publish_position`]。
    pub(in crate::syscall) fn publish_position_blocking(&self, result: isize) {
        if let Some(start) = self.shared_start
            && result > 0
        {
            let _ = self
                .ofd
                .with_position(|position| *position = start + result as u64);
        }
    }

    /// 与同步 [`positioned_read`] 相同的收尾：统计、`IN_ACCESS` 与 atime。
    fn complete(&self, task: &TaskControlBlock, result: isize) -> isize {
        if self.file.tracks_access_time() {
            drop(self.ofd.read_access());
        }
        task.account_read_result(result);
        if result > 0 {
            self.ofd.notify(crate::fs::IN_ACCESS);
        }
        result
    }

    /// 推进读取；`notify` 在块读完成时被调用。
    pub(in crate::syscall) fn step(
        &mut self,
        task: &TaskControlBlock,
        vectors: &[UserIoVec],
        notify: &Arc<dyn crate::fs::CompletionNotify>,
    ) -> AsyncReadStep {
        loop {
            if self.current.is_none() {
                let Some(count) = self.progress.next_count(vectors) else {
                    let total = self.progress.total();
                    return AsyncReadStep::Done(self.complete(task, total));
                };
                let Some(read) = self
                    .file
                    .begin_read_nowait(self.position, count, notify.clone())
                else {
                    return AsyncReadStep::Fallback;
                };
                self.current = Some((read, count));
            }
            let (read, count) = self.current.as_mut().expect("set above");
            let count = *count;
            match read.step(&mut self.chunk[..count]) {
                Err(error) => {
                    let result = self.progress.failed(error);
                    return AsyncReadStep::Done(self.complete(task, result));
                }
                Ok(crate::fs::ReadStep::Wait) => return AsyncReadStep::Wait,
                Ok(crate::fs::ReadStep::Fallback) => return AsyncReadStep::Fallback,
                Ok(crate::fs::ReadStep::Done(read)) => {
                    task.account_read_storage(read.storage_bytes);
                    self.current = None;
                    if let Some(result) = self.progress.deliver(
                        task,
                        vectors,
                        &mut self.position,
                        &self.chunk,
                        read.bytes,
                        count,
                    ) {
                        return AsyncReadStep::Done(self.complete(task, result));
                    }
                }
            }
        }
    }

    /// 在可以睡眠的线程里用阻塞读完成剩余部分（阻塞车道）。
    pub(in crate::syscall) fn finish_blocking(
        mut self,
        task: &TaskControlBlock,
        vectors: &[UserIoVec],
    ) -> isize {
        let result = read_vectors_from(
            task,
            &self.file,
            &mut self.position,
            vectors,
            core::mem::take(&mut self.progress),
        );
        let result = self.complete(task, result);
        self.publish_position_blocking(result);
        result
    }
}

/// 从 regular-file OFD 的显式 offset 读取，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `pointer`: userspace 输出地址。
/// - `length`: 最大读取长度。
/// - `offset`: 非负文件偏移。
///
/// # Returns
///
/// byte count、EOF 零或负 errno。
pub(crate) fn sys_pread64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    positioned_read_fd(
        fd,
        &[UserIoVec {
            base: pointer,
            length,
        }],
        offset,
    )
}

/// 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `vectors`: 按序消费的 userspace buffers。
/// - `offset`: 非负文件偏移；Linux `O_APPEND` OFD 仍在 inode end 执行写入。
/// - `append_override`: `pwritev2` 对 OFD O_APPEND 的 operation-local override。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
fn positioned_write_fd(
    fd: usize,
    vectors: &[UserIoVec],
    offset: i64,
    append_override: Option<bool>,
) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    let task = current_task().expect("pwrite64 requires current task");
    let Some(ofd) = task.fd_get(fd) else {
        return -errno::EBADF;
    };
    positioned_write(&task, &ofd, vectors, offset, append_override)
}

pub(in crate::syscall) fn positioned_write(
    task: &TaskControlBlock,
    ofd: &Arc<OpenFileDescription>,
    vectors: &[UserIoVec],
    offset: i64,
    append_override: Option<bool>,
) -> isize {
    if offset < 0 {
        return -errno::EINVAL;
    }
    if ofd.status_flags() & O_ACCMODE == O_RDONLY {
        return -errno::EBADF;
    }
    if let OpenFileKind::Device(file) = &ofd.kind
        && let Some(file) = file.positioned()
    {
        if append_override.is_some() {
            return -errno::EOPNOTSUPP;
        }
        let total_length = vectors.iter().map(|vector| vector.length).sum();
        let result = crate::syscall::device::write_positioned_file(
            task,
            file,
            vectors,
            total_length,
            offset as u64,
        );
        task.account_write_result(result);
        if result > 0 {
            ofd.notify(crate::fs::IN_MODIFY);
        }
        return result;
    }
    // 路径打开的文件与 memfd 都有 inode 内容；pipe、socket 与字符设备没有可定位的内容。
    if !matches!(ofd.kind, OpenFileKind::Inode) {
        return -errno::ESPIPE;
    }
    let inode = ofd.inode_ref();
    if inode.inode_type() == InodeType::Directory {
        return -errno::EISDIR;
    }
    if !matches!(inode.inode_type(), InodeType::File | InodeType::BlockDevice) {
        return -errno::ESPIPE;
    }
    if vectors.iter().all(|vector| vector.length == 0) {
        return 0;
    }
    let total_length = vectors
        .iter()
        .try_fold(0usize, |total, vector| total.checked_add(vector.length))
        .expect("positioned write vectors must have a checked total length");
    let file = match ofd.regular_file() {
        Ok(file) => file,
        Err(error) => return ferr(error),
    };

    let append = append_override.unwrap_or_else(|| ofd.status_flags() & O_APPEND != 0);
    let staging = PreparedRegularWriteStaging::prepare(total_length);
    let result = with_prepared_staging(staging, |staging| {
        let mut staging = staging.as_input_staging();
        let mut position = offset as u64;
        let writer = match file.begin_write(task.access_identity(true), append) {
            Ok(writer) => writer,
            Err(error) => return ferr(error),
        };
        write_regular_vectors(task, &writer, &mut position, vectors, append, &mut staging)
    });
    task.account_write_result(result);
    if result > 0 {
        ofd.notify(crate::fs::IN_MODIFY);
    }
    result
}

/// 向 regular-file OFD 的显式 offset 写入，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `pointer`: userspace 输入地址。
/// - `length`: 待写入长度。
/// - `offset`: 非负文件偏移；Linux legacy pwrite64 仍继承 OFD 的 O_APPEND。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_pwrite64(fd: usize, pointer: usize, length: usize, offset: i64) -> isize {
    positioned_write_fd(
        fd,
        &[UserIoVec {
            base: pointer,
            length,
        }],
        offset,
        None,
    )
}

fn positioned_readv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    let task = current_task().expect("preadv requires current task");
    let (vectors, _) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    positioned_read_fd(fd, &vectors, offset)
}

fn positioned_writev(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    append_override: Option<bool>,
) -> isize {
    let task = current_task().expect("pwritev requires current task");
    let (vectors, _) = match import_iovecs(&task, iovector, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    positioned_write_fd(fd, &vectors, offset, append_override)
}

/// 按 Linux preadv ABI 从显式 offset scatter read，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 非负显式 offset。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_preadv(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_readv(fd, iovector, count, offset)
}

/// 按 Linux pwritev ABI 向显式 offset gather write，不修改共享 OFD offset。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 非负显式 offset。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_pwritev(fd: usize, iovector: usize, count: usize, offset: i64) -> isize {
    positioned_writev(fd, iovector, count, offset, None)
}

/// 实现 Linux preadv2；offset=-1 使用共享 OFD offset，其余为 positioned read。
///
/// # Parameters
///
/// - `fd`: 源 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 显式 offset 或 -1。
/// - `flags`: 当前同步 VFS 不支持异步/缓存 hint，非零 flags 返回 EOPNOTSUPP。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_preadv2(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    flags: u32,
) -> isize {
    if flags != 0 {
        return -errno::EOPNOTSUPP;
    }
    if offset == -1 {
        return sys_readv(fd, iovector, count);
    }
    positioned_readv(fd, iovector, count, offset)
}

const RWF_DSYNC: u32 = 0x02;
const RWF_SYNC: u32 = 0x04;
const RWF_APPEND: u32 = 0x10;
const RWF_NOAPPEND: u32 = 0x20;
const SUPPORTED_WRITE_FLAGS: u32 = RWF_DSYNC | RWF_SYNC | RWF_APPEND | RWF_NOAPPEND;

/// 实现 Linux pwritev2 的 append override 与同步写语义。
///
/// # Parameters
///
/// - `fd`: 目标 descriptor。
/// - `iovector`: userspace iovec 数组。
/// - `count`: iovec 数量。
/// - `offset`: 显式 offset 或 -1。
/// - `flags`: 支持 RWF_DSYNC/RWF_SYNC/RWF_APPEND/RWF_NOAPPEND；其他 flags 返回 EOPNOTSUPP。
///
/// # Returns
///
/// byte count、partial count 或负 errno。
pub(crate) fn sys_pwritev2(
    fd: usize,
    iovector: usize,
    count: usize,
    offset: i64,
    flags: u32,
) -> isize {
    if flags & !SUPPORTED_WRITE_FLAGS != 0 {
        return -errno::EOPNOTSUPP;
    }
    if flags & RWF_APPEND != 0 && flags & RWF_NOAPPEND != 0 {
        return -errno::EINVAL;
    }
    if offset == -1 {
        if flags & (RWF_APPEND | RWF_NOAPPEND) != 0 {
            return -errno::EOPNOTSUPP;
        }
        let result = sys_writev(fd, iovector, count);
        if result >= 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
            let sync = super::super::sync_file(fd);
            if sync < 0 {
                return sync;
            }
        }
        return result;
    }
    let append_override = if flags & RWF_APPEND != 0 {
        Some(true)
    } else if flags & RWF_NOAPPEND != 0 {
        Some(false)
    } else {
        None
    };
    let result = positioned_writev(fd, iovector, count, offset, append_override);
    if result >= 0 && flags & (RWF_DSYNC | RWF_SYNC) != 0 {
        let sync = super::super::sync_file(fd);
        if sync < 0 {
            return sync;
        }
    }
    result
}
