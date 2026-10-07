//! tmpfs：内存文件系统（Linux `mm/shmem.c` 的 mount/inode 语义）。
//!
//! 目录项、权限与 link count 在这里；regular file 内容由 [`MemoryFile`] 单份持有，不经 page cache，
//! 文件在最后一个 link 与最后一个打开引用都消失时立即释放页并归还 `size=` 配额。
//!
//! 并发：所有会改变目录结构、link count 或 inode 表的操作先取 `Shared::namespace`，再取涉及的
//! inode 状态锁；只读操作（lookup、readdir、stat）只取单个 inode 状态锁。因此多锁路径被
//! `namespace` 串行化，不会互相死锁。

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU64, Ordering};

use super::{
    CreateMetadata, DataBacking, DirectoryEntry, DirectoryRead, DirectoryVisit, DirectoryVisitor,
    FileSystem, FileSystemError, FileSystemStatistics, Inode, InodeMetadata, InodeType, MemoryFile,
    OwnerModeChange, PageBudget,
    mount::{FileSystemType, MountRequest},
    permission::OwnerModeState,
};
use crate::{
    fallible_tree::FallibleMap,
    memory::SharedFileId,
    sync::{TaskMutex, TaskMutexGuard},
};
use directory::{Directory, FIRST_ENTRY_COOKIE};

#[path = "tmpfs/directory.rs"]
mod directory;
#[path = "tmpfs/options.rs"]
mod options;

const TMPFS_MAGIC: u64 = 0x0102_1994;
const NAME_MAX: usize = 255;
const BLOCK_SIZE: u64 = 4096;
/// Linux `BOGO_DIRENT_SIZE`：tmpfs 目录的 `st_size` 是 20 字节乘以目录项数（含 `.` 与 `..`）。
const DIRECTORY_ENTRY_SIZE: u64 = 20;
const ROOT_INODE: u64 = 1;
/// `.` 与 `..` 的 readdir cookie；真实目录项从 [`FIRST_ENTRY_COOKIE`] 开始。
const DOT_COOKIE: u64 = 1;
const DOT_DOT_COOKIE: u64 = 2;

const S_IFSOCK: u16 = 0o140000;
const S_IFLNK: u16 = 0o120000;
const S_IFREG: u16 = 0o100000;
const S_IFDIR: u16 = 0o040000;

/// 一个 tmpfs 实例内全部 inode 共享的状态。
struct Shared {
    filesystem_id: usize,
    budget: Arc<PageBudget>,
    // `nr_inodes=`，`u64::MAX` 为不限；`remount` 可调整，所以是原子量。
    inode_limit: AtomicU64,
    // inode 编号只增不复用；缺失单调性会让已删除 inode 的 `(st_dev, st_ino)` 被新文件复用，
    // advisory lock 等按 inode 身份判断的机制会串到别的文件。
    next_inode: AtomicU64,
    // 存活 inode 数（含已 unlink 但仍被打开的）；在 `TmpInode` 构造时预留、Drop 时归还，用来执行
    // `nr_inodes=` 并供 statfs 使用。缺失时 inode 耗尽无法被限制。
    live_inodes: AtomicU64,
    // OWNER: 串行化目录结构变更并拥有 inode 表；锁顺序见模块文档。
    namespace: TaskMutex<Namespace>,
}

/// 仍有 link 的 inode。rename 的目标目录与 link 的目标 inode 只给出编号，必须由这里解析回对象。
struct Namespace {
    inodes: FallibleMap<u64, Weak<TmpInode>>,
}

/// `create`/`symlink` 请求的新 inode 种类；内容在拿到 inode 编号后才能建立。
enum NewKind {
    File,
    Directory,
    Socket,
    Symlink(Box<[u8]>),
}

enum Body {
    Directory,
    File(Arc<MemoryFile>),
    Symlink(Box<[u8]>),
    Socket,
}

struct State {
    /// 文件类型位加权限位，与 `st_mode` 一致。
    mode: u16,
    uid: u32,
    gid: u32,
    links: u32,
    atime: u64,
    mtime: u64,
    ctime: u64,
    entries: Directory<Arc<TmpInode>>,
    /// 目录已被 rmdir：仍可能被 cwd 或打开的 fd 持有，但不能再接收新项。
    dead: bool,
}

/// tmpfs inode。
pub(crate) struct TmpInode {
    shared: Arc<Shared>,
    inode: u64,
    body: Body,
    // 目录的父目录编号（根指向自己）。只在持有 `Shared::namespace` 时改变，原子类型仅为了让
    // 只读路径（`..` 的 readdir）无需取锁；rename 的“不得移入自己的子树”检查沿它向上走。
    parent: AtomicU64,
    state: TaskMutex<State>,
    // 目录拆除链的下一个节点，只在 `Drop` 的拆除循环里使用；见 `Drop for TmpInode`。
    next_reap: spin::Mutex<Option<Arc<TmpInode>>>,
}

impl Drop for TmpInode {
    /// 归还 inode 名额并拆除子树。
    ///
    /// 目录树的深度由用户决定（逐层 `mkdir` + `cd` 没有上限），沿 `Arc` 递归释放会让内核栈溢出。所以
    /// 这里把“只剩父目录这一个引用的子目录”串到 `next_reap` 链上，用循环逐个拆除：栈深为常数，
    /// 也不分配内存。链上的目录不可达（它们的父目录正在被释放），所以拆除不与任何操作竞争。
    fn drop(&mut self) {
        self.shared.live_inodes.fetch_sub(1, Ordering::AcqRel);
        let mut pending = None;
        self.detach_children(&mut pending);
        while let Some(node) = pending.take() {
            pending = node.next_reap.lock().take();
            node.detach_children(&mut pending);
        }
    }
}

fn now_seconds() -> u64 {
    crate::timer::get_realtime_ns() / 1_000_000_000
}

fn lock_error<T>(_: T) -> FileSystemError {
    FileSystemError::OutOfMemory
}

fn validate_name(name: &[u8]) -> Result<(), FileSystemError> {
    if name.is_empty()
        || name.len() > NAME_MAX
        || name == b"."
        || name == b".."
        || name.contains(&b'/')
        || name.contains(&0)
    {
        return Err(FileSystemError::InvalidPath);
    }
    Ok(())
}

impl Shared {
    fn allocate_inode_number(&self) -> u64 {
        self.next_inode.fetch_add(1, Ordering::Relaxed)
    }

    /// 预留一个 inode 名额；成功后必须立即构造 [`TmpInode`]，由它的 Drop 归还。
    fn reserve_inode(&self) -> Result<(), FileSystemError> {
        self.live_inodes
            .try_update(Ordering::AcqRel, Ordering::Relaxed, |live| {
                (live < self.inode_limit.load(Ordering::Acquire)).then(|| live + 1)
            })
            .map(|_| ())
            .map_err(|_| FileSystemError::NoSpace)
    }

    fn namespace(&self) -> Result<TaskMutexGuard<'_, Namespace>, FileSystemError> {
        self.namespace.lock().map_err(lock_error)
    }
}

impl TmpInode {
    /// 构造一个尚未链接的 inode。
    ///
    /// # Parameters
    ///
    /// - `inode`: 由 [`Shared::allocate_inode_number`] 取得的编号。
    /// - `mode`: `S_IF*` 文件类型位加权限位。
    /// - `links`: 初始 link count。
    /// - `parent`: 目录的父目录编号；非目录为 0。
    ///
    /// # Errors
    ///
    /// 超过 `nr_inodes=` 返回 `NoSpace`；分配失败返回 `OutOfMemory`。
    fn new(
        shared: &Arc<Shared>,
        inode: u64,
        body: Body,
        mode: u16,
        owner: (u32, u32),
        links: u32,
        parent: u64,
    ) -> Result<Arc<Self>, FileSystemError> {
        shared.reserve_inode()?;
        let now = now_seconds();
        // `Self` 一旦构造，任何后续失败都由它的 Drop 归还名额。
        let value = Self {
            shared: shared.clone(),
            inode,
            body,
            parent: AtomicU64::new(parent),
            next_reap: spin::Mutex::new(None),
            state: TaskMutex::new(State {
                mode,
                uid: owner.0,
                gid: owner.1,
                links,
                atime: now,
                mtime: now,
                ctime: now,
                entries: Directory::new(),
                dead: false,
            }),
        };
        Arc::try_new(value).map_err(|_| FileSystemError::OutOfMemory)
    }

    /// 摘下所有子项；仍被别处引用的子项只减引用，唯一引用的子目录挂到 `pending` 链上待拆除。
    fn detach_children(&self, pending: &mut Option<Arc<Self>>) {
        // 调用者独占该 inode（正在 Drop，或在不可达的 reap 链上），锁不会被争用。
        let mut state = self
            .state
            .try_lock()
            .expect("an unreachable tmpfs inode was locked");
        while let Some(child) = state.entries.pop_first() {
            if matches!(child.body, Body::Directory) && Arc::strong_count(&child) == 1 {
                *child.next_reap.lock() = pending.take();
                *pending = Some(child);
            }
        }
    }

    fn kind(&self) -> InodeType {
        match self.body {
            Body::Directory => InodeType::Directory,
            Body::File(_) => InodeType::File,
            Body::Symlink(_) => InodeType::SymLink,
            Body::Socket => InodeType::Socket,
        }
    }

    fn state(&self) -> Result<TaskMutexGuard<'_, State>, FileSystemError> {
        self.state.lock().map_err(lock_error)
    }

    fn file(&self) -> Result<&Arc<MemoryFile>, FileSystemError> {
        match &self.body {
            Body::File(file) => Ok(file),
            Body::Directory => Err(FileSystemError::IsDirectory),
            Body::Symlink(_) | Body::Socket => Err(FileSystemError::InvalidOperation),
        }
    }

    /// 校验 `self` 是存活的目录。
    fn require_live_directory(&self, state: &State) -> Result<(), FileSystemError> {
        if !matches!(self.body, Body::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        if state.dead {
            return Err(FileSystemError::NotFound);
        }
        Ok(())
    }

    /// 创建新 inode 并链接到 `self` 下。
    ///
    /// 1. 先分配 inode 编号、内容与目录项/inode 表节点：OOM 与 `nr_inodes=` 都发生在任何可见变化之前；
    /// 2. 再在 `namespace` 与目录状态锁内检查并提交，提交阶段不会失败。
    fn create_child(
        &self,
        name: &[u8],
        kind: NewKind,
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        validate_name(name)?;
        let shared = &self.shared;
        let inode = shared.allocate_inode_number();
        let permissions = metadata.mode as u16 & 0o7777;
        let (body, type_bits) = match kind {
            NewKind::File => (
                Body::File(MemoryFile::new(
                    SharedFileId {
                        filesystem: shared.filesystem_id,
                        inode,
                    },
                    shared.budget.clone(),
                    false,
                )?),
                S_IFREG,
            ),
            NewKind::Directory => (Body::Directory, S_IFDIR),
            NewKind::Socket => (Body::Socket, S_IFSOCK),
            NewKind::Symlink(target) => (Body::Symlink(target), S_IFLNK),
        };
        let is_directory = matches!(body, Body::Directory);
        let child = Self::new(
            shared,
            inode,
            body,
            type_bits | permissions,
            (metadata.uid, metadata.gid),
            if is_directory { 2 } else { 1 },
            if is_directory { self.inode } else { 0 },
        )?;
        let reserved =
            Directory::reserve(name, child.clone()).map_err(|_| FileSystemError::OutOfMemory)?;
        let table = FallibleMap::try_prepare(child.inode, Arc::downgrade(&child))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        let mut namespace = shared.namespace()?;
        let mut state = self.state()?;
        self.require_live_directory(&state)?;
        if state.entries.get(name).is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        if is_directory {
            state.links = state
                .links
                .checked_add(1)
                .ok_or(FileSystemError::TooManyLinks)?;
        }
        state.entries.insert(reserved);
        namespace.inodes.commit_vacant(table);
        let now = now_seconds();
        state.mtime = now;
        state.ctime = now;
        Ok(child)
    }

    /// 按编号取得仍有 link 的 inode。
    fn resolve(namespace: &Namespace, inode: u64) -> Option<Arc<Self>> {
        namespace.inodes.get(&inode)?.upgrade()
    }
}

impl Inode for TmpInode {
    fn filesystem_id(&self) -> usize {
        self.shared.filesystem_id
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        let state = self.state()?;
        let (size, blocks, modified) = match &self.body {
            Body::File(file) => (
                file.size(),
                file.resident_pages()? as u64 * (BLOCK_SIZE / 512),
                file.modified_seconds(),
            ),
            Body::Directory => (
                (state.entries.len() as u64 + 2) * DIRECTORY_ENTRY_SIZE,
                0,
                0,
            ),
            Body::Symlink(target) => (target.len() as u64, 0, 0),
            Body::Socket => (0, 0, 0),
        };
        Ok(InodeMetadata {
            filesystem: self.shared.filesystem_id as u64,
            inode: self.inode,
            kind: self.kind(),
            mode: u32::from(state.mode),
            links: state.links,
            uid: state.uid,
            gid: state.gid,
            size,
            blocks,
            block_size: BLOCK_SIZE as u32,
            atime: state.atime,
            // 文件内容的写入不经 inode，修改时间由存储记录。
            mtime: state.mtime.max(modified),
            ctime: state.ctime.max(modified),
            device: None,
        })
    }

    fn inode_type(&self) -> InodeType {
        self.kind()
    }

    /// regular file 与 symlink 的长度；目录的 bogo size 只出现在 [`Inode::metadata`]（它需要状态锁）。
    fn size(&self) -> u64 {
        match &self.body {
            Body::File(file) => file.size(),
            Body::Symlink(target) => target.len() as u64,
            Body::Directory | Body::Socket => 0,
        }
    }

    fn is_executable(&self) -> bool {
        self.metadata()
            .is_ok_and(|metadata| metadata.kind == InodeType::File && metadata.mode & 0o111 != 0)
    }

    fn data_backing(&self) -> DataBacking {
        match &self.body {
            Body::File(file) => DataBacking::Memory(file.clone()),
            Body::Directory | Body::Symlink(_) | Body::Socket => DataBacking::PageCache,
        }
    }

    fn read_storage(&self, offset: u64, output: &mut [u8]) -> Result<usize, FileSystemError> {
        self.file()?.read(offset, output)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        let Body::Symlink(target) = &self.body else {
            return Err(FileSystemError::InvalidOperation);
        };
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(target.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        bytes.extend_from_slice(target);
        Ok(bytes)
    }

    fn write_storage(&self, offset: u64, input: &[u8]) -> Result<usize, FileSystemError> {
        self.file()?.begin_write()?.write(offset, input)
    }

    fn append_storage(&self, input: &[u8]) -> Result<(u64, usize), FileSystemError> {
        self.file()?.begin_write()?.append(input, u64::MAX)
    }

    fn truncate_storage(&self, size: u64) -> Result<(), FileSystemError> {
        self.file()?.truncate(size)
    }

    fn allocate_storage(&self, offset: u64, length: u64) -> Result<(), FileSystemError> {
        self.file()?.allocate(offset, length)
    }

    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        let mut state = self.state()?;
        if let Some(atime) = atime {
            state.atime = atime;
        }
        if let Some(mtime) = mtime {
            state.mtime = mtime;
        }
        state.ctime = now_seconds();
        Ok(())
    }

    /// 迭代 `.`、`..` 与按 cookie 排序的目录项。
    ///
    /// cursor 是上一项的 cookie：`.`、`..` 固定为 1、2，真实项从 3 起单调分配且不复用，所以迭代
    /// 中途创建或删除任意项都不会让已读位置漂移。
    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        if !matches!(self.body, Body::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let state = self.state()?;
        let mut cursor = cursor;
        let fixed = [
            (DOT_COOKIE, self.inode, &b"."[..]),
            (
                DOT_DOT_COOKIE,
                self.parent.load(Ordering::Relaxed),
                &b".."[..],
            ),
        ];
        for (cookie, inode, name) in fixed {
            if cursor >= cookie {
                continue;
            }
            let entry = DirectoryEntry {
                inode,
                kind: InodeType::Directory,
                name,
            };
            if visitor.visit(cookie, entry)? == DirectoryVisit::Stop {
                return Ok(DirectoryRead { cursor, eof: false });
            }
            cursor = cookie;
        }
        debug_assert!(cursor < FIRST_ENTRY_COOKIE);
        for (cookie, name, child) in state.entries.entries_after(cursor) {
            let entry = DirectoryEntry {
                inode: child.inode,
                kind: child.kind(),
                name,
            };
            if visitor.visit(cookie, entry)? == DirectoryVisit::Stop {
                return Ok(DirectoryRead { cursor, eof: false });
            }
            cursor = cookie;
        }
        Ok(DirectoryRead { cursor, eof: true })
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let state = self.state()?;
        if !matches!(self.body, Body::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        match state.entries.get(name) {
            Some(child) => Ok(child.clone()),
            None => Err(FileSystemError::NotFound),
        }
    }

    fn create(
        &self,
        name: &[u8],
        kind: InodeType,
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let kind = match kind {
            InodeType::File => NewKind::File,
            InodeType::Directory => NewKind::Directory,
            InodeType::Socket => NewKind::Socket,
            _ => return Err(FileSystemError::InvalidOperation),
        };
        self.create_child(name, kind, metadata)
    }

    fn change_owner_mode(&self, change: OwnerModeChange) -> Result<(), FileSystemError> {
        let mut state = self.state()?;
        let updated = change.authorize(OwnerModeState::new(
            self.kind(),
            state.mode,
            state.uid,
            state.gid,
        ))?;
        state.mode = updated.mode();
        state.uid = updated.uid();
        state.gid = updated.gid();
        state.ctime = now_seconds();
        Ok(())
    }

    fn symlink(
        &self,
        name: &[u8],
        target: &[u8],
        metadata: CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(target.len())
            .map_err(|_| FileSystemError::OutOfMemory)?;
        bytes.extend_from_slice(target);
        self.create_child(name, NewKind::Symlink(bytes.into_boxed_slice()), metadata)
    }

    fn link(&self, name: &[u8], target: Arc<dyn Inode>) -> Result<(), FileSystemError> {
        validate_name(name)?;
        let reserved_target = target.metadata()?.inode;
        let namespace = self.shared.namespace()?;
        // 已 unlink 的 inode 不在表里：Linux 对 `link()` 一个 link count 为 0 的 inode 返回 ENOENT。
        let target = Self::resolve(&namespace, reserved_target).ok_or(FileSystemError::NotFound)?;
        if matches!(target.body, Body::Directory) {
            return Err(FileSystemError::PermissionDenied);
        }
        let reserved =
            Directory::reserve(name, target.clone()).map_err(|_| FileSystemError::OutOfMemory)?;
        let mut state = self.state()?;
        self.require_live_directory(&state)?;
        if state.entries.get(name).is_some() {
            return Err(FileSystemError::AlreadyExists);
        }
        let mut target_state = target.state()?;
        target_state.links = target_state
            .links
            .checked_add(1)
            .ok_or(FileSystemError::TooManyLinks)?;
        let now = now_seconds();
        target_state.ctime = now;
        state.entries.insert(reserved);
        state.mtime = now;
        state.ctime = now;
        Ok(())
    }

    fn unlink(&self, name: &[u8], remove_directory: bool) -> Result<(), FileSystemError> {
        let mut namespace = self.shared.namespace()?;
        let mut state = self.state()?;
        self.require_live_directory(&state)?;
        let child = state
            .entries
            .get(name)
            .ok_or(FileSystemError::NotFound)?
            .clone();
        let is_directory = matches!(child.body, Body::Directory);
        if remove_directory && !is_directory {
            return Err(FileSystemError::NotDirectory);
        }
        if !remove_directory && is_directory {
            return Err(FileSystemError::IsDirectory);
        }
        let mut child_state = child.state()?;
        if is_directory {
            if child_state.entries.len() != 0 {
                return Err(FileSystemError::DirectoryNotEmpty);
            }
            child_state.dead = true;
            child_state.links = 0;
            state.links -= 1;
        } else {
            child_state.links -= 1;
        }
        let now = now_seconds();
        child_state.ctime = now;
        if child_state.links == 0 {
            namespace.inodes.remove(&child.inode);
        }
        state.entries.remove(name);
        state.mtime = now;
        state.ctime = now;
        // 最后一个引用在锁释放后才被丢弃：内容页的释放不需要在 namespace 锁内完成。
        drop(child_state);
        drop(state);
        drop(namespace);
        drop(child);
        Ok(())
    }

    /// 原子地把 `old_name` 移到 `new_parent_inode` 下的 `new_name`。
    ///
    /// 1. 在 `namespace` 内解析目标目录与被移动的项，目录移动先检查不会落入自己的子树；
    /// 2. 预分配新目录项；
    /// 3. 锁住两个目录（同一目录只锁一次，否则按 inode 编号顺序）与被覆盖的项，完成全部类型、
    ///    非空与 link-count 溢出检查；
    /// 4. 此后的修改不会失败，两个目录对 readdir 与 lookup 同时可见。
    fn rename(
        &self,
        old_name: &[u8],
        new_parent_inode: u64,
        new_name: &[u8],
        no_replace: bool,
    ) -> Result<(), FileSystemError> {
        validate_name(new_name)?;
        let mut namespace = self.shared.namespace()?;
        let new_parent =
            Self::resolve(&namespace, new_parent_inode).ok_or(FileSystemError::NotFound)?;
        if !matches!(new_parent.body, Body::Directory) {
            return Err(FileSystemError::NotDirectory);
        }
        let child = {
            let state = self.state()?;
            self.require_live_directory(&state)?;
            state
                .entries
                .get(old_name)
                .ok_or(FileSystemError::NotFound)?
                .clone()
        };
        let moves_directory = matches!(child.body, Body::Directory);
        if moves_directory {
            // 目录不能被移入它自己的子树：沿 `new_parent` 的祖先链向上找 `child`。
            let mut ancestor = new_parent.inode;
            loop {
                if ancestor == child.inode {
                    return Err(FileSystemError::InvalidOperation);
                }
                if ancestor == ROOT_INODE {
                    break;
                }
                ancestor = Self::resolve(&namespace, ancestor)
                    .ok_or(FileSystemError::NotFound)?
                    .parent
                    .load(Ordering::Relaxed);
            }
        }
        let reserved = Directory::reserve(new_name, child.clone())
            .map_err(|_| FileSystemError::OutOfMemory)?;

        let mut parents = Parents::lock(self, &new_parent)?;
        new_parent.require_live_directory(parents.destination())?;
        let replaced = parents.destination().entries.get(new_name).cloned();
        let replaced_directory =
            Self::check_replacement(replaced.as_ref(), moves_directory, no_replace)?;
        if replaced
            .as_ref()
            .is_some_and(|target| target.inode == self.inode)
        {
            // 被覆盖的目录正是源目录自己（它包含 `child`，必然非空）；它的锁已被持有。
            return Err(FileSystemError::DirectoryNotEmpty);
        }
        let mut replaced_state = match &replaced {
            Some(target) => Some(target.state()?),
            None => None,
        };
        if replaced_directory
            && replaced_state
                .as_ref()
                .is_some_and(|state| state.entries.len() != 0)
        {
            return Err(FileSystemError::DirectoryNotEmpty);
        }
        // 先完成全部可能失败的检查，再修改。
        if moves_directory && !parents.same_directory() {
            let links = parents
                .destination()
                .links
                .checked_add(1)
                .ok_or(FileSystemError::TooManyLinks)?;
            parents.destination().links = links;
            parents.source().links -= 1;
        }
        let now = now_seconds();
        if let (Some(target), Some(target_state)) = (&replaced, replaced_state.as_mut()) {
            if replaced_directory {
                parents.destination().links -= 1;
            }
            Self::retire(&mut namespace, target, target_state, now);
            parents.destination().entries.remove(new_name);
        }
        parents.source().entries.remove(old_name);
        parents.destination().entries.insert(reserved);
        if moves_directory {
            child.parent.store(new_parent.inode, Ordering::Relaxed);
        }
        Self::stamp(parents.source(), now);
        Self::stamp(parents.destination(), now);
        child.state()?.ctime = now;
        Ok(())
    }
}

/// rename 涉及的两个目录状态锁；源与目标是同一目录时只有一把。
enum Parents<'a> {
    Same(TaskMutexGuard<'a, State>),
    Two {
        old: TaskMutexGuard<'a, State>,
        new: TaskMutexGuard<'a, State>,
    },
}

impl<'a> Parents<'a> {
    /// 按 inode 编号顺序取锁，使不同方向的并发 rename 取锁顺序一致。
    fn lock(old: &'a TmpInode, new: &'a TmpInode) -> Result<Self, FileSystemError> {
        if old.inode == new.inode {
            return Ok(Self::Same(old.state()?));
        }
        Ok(if old.inode < new.inode {
            let old = old.state()?;
            Self::Two {
                new: new.state()?,
                old,
            }
        } else {
            let new = new.state()?;
            Self::Two {
                old: old.state()?,
                new,
            }
        })
    }

    fn same_directory(&self) -> bool {
        matches!(self, Self::Same(_))
    }

    fn source(&mut self) -> &mut State {
        match self {
            Self::Same(state) => state,
            Self::Two { old, .. } => old,
        }
    }

    fn destination(&mut self) -> &mut State {
        match self {
            Self::Same(state) => state,
            Self::Two { new, .. } => new,
        }
    }
}

impl TmpInode {
    fn stamp(state: &mut State, now: u64) {
        state.mtime = now;
        state.ctime = now;
    }

    /// rename 的目标存在时的类型规则。
    ///
    /// # Returns
    ///
    /// 目标是否为将被替换的目录。
    fn check_replacement(
        replaced: Option<&Arc<Self>>,
        moves_directory: bool,
        no_replace: bool,
    ) -> Result<bool, FileSystemError> {
        let Some(target) = replaced else {
            return Ok(false);
        };
        if no_replace {
            return Err(FileSystemError::AlreadyExists);
        }
        match (moves_directory, matches!(target.body, Body::Directory)) {
            (true, false) => Err(FileSystemError::NotDirectory),
            (false, true) => Err(FileSystemError::IsDirectory),
            (_, replaced_directory) => Ok(replaced_directory),
        }
    }

    /// 被 rename 覆盖的 inode 失去这个 link；最后一个 link 消失时移出 inode 表。
    fn retire(namespace: &mut Namespace, target: &Arc<Self>, state: &mut State, now: u64) {
        if matches!(target.body, Body::Directory) {
            state.dead = true;
            state.links = 0;
        } else {
            state.links -= 1;
        }
        state.ctime = now;
        if state.links == 0 {
            namespace.inodes.remove(&target.inode);
        }
    }
}

/// 一个 tmpfs 实例。
pub(crate) struct TmpFileSystem {
    shared: Arc<Shared>,
    root: Arc<TmpInode>,
    /// 物理页总数，`remount` 的 `size=N%` 基数。
    ram_pages: u64,
}

impl TmpFileSystem {
    fn new(options: &options::Options, ram_pages: u64) -> Result<Arc<Self>, FileSystemError> {
        let shared = Arc::try_new(Shared {
            filesystem_id: super::allocate_filesystem_id(),
            budget: PageBudget::new(options.blocks)?,
            inode_limit: AtomicU64::new(options.inodes.unwrap_or(u64::MAX)),
            next_inode: AtomicU64::new(ROOT_INODE + 1),
            live_inodes: AtomicU64::new(0),
            namespace: TaskMutex::new(Namespace {
                inodes: FallibleMap::new(),
            }),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let root = TmpInode::new(
            &shared,
            ROOT_INODE,
            Body::Directory,
            S_IFDIR | options.mode as u16,
            (options.uid, options.gid),
            2,
            ROOT_INODE,
        )?;
        let table = FallibleMap::try_prepare(ROOT_INODE, Arc::downgrade(&root))
            .map_err(|_| FileSystemError::OutOfMemory)?;
        shared.namespace()?.inodes.commit_vacant(table);
        Arc::try_new(Self {
            shared,
            root,
            ram_pages,
        })
        .map_err(|_| FileSystemError::OutOfMemory)
    }
}

impl FileSystem for TmpFileSystem {
    fn root_inode(&self) -> Result<Arc<dyn Inode>, FileSystemError> {
        Ok(self.root.clone())
    }

    /// 重新配置 `size=`、`nr_blocks=`、`nr_inodes=` 与根目录 `mode=`/`uid=`/`gid=`。
    ///
    /// 未在 `options` 中出现的参数保持当前值；新的页数或 inode 上限小于当前占用返回
    /// `InvalidOperation`（Linux：`Cannot retroactively limit size`），失败时不改变任何状态。
    fn remount(&self, requested: &[u8]) -> Result<(), FileSystemError> {
        let shared = &self.shared;
        let mut root = self.root.state()?;
        let current = options::Options {
            blocks: shared.budget.limit(),
            inodes: Some(shared.inode_limit.load(Ordering::Relaxed)).filter(|l| *l != u64::MAX),
            mode: u32::from(root.mode & 0o7777),
            uid: root.uid,
            gid: root.gid,
        };
        let wanted = options::parse(requested, self.ram_pages, current)
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let live = shared.live_inodes.load(Ordering::Acquire);
        if wanted.inodes.is_some_and(|limit| limit < live) {
            return Err(FileSystemError::InvalidOperation);
        }
        shared.budget.set_limit(wanted.blocks)?;
        shared
            .inode_limit
            .store(wanted.inodes.unwrap_or(u64::MAX), Ordering::Release);
        root.mode = root.mode & !0o7777 | wanted.mode as u16;
        root.uid = wanted.uid;
        root.gid = wanted.gid;
        root.ctime = now_seconds();
        Ok(())
    }

    fn statistics(&self) -> Result<FileSystemStatistics, FileSystemError> {
        let shared = &self.shared;
        let (blocks, free) = match shared.budget.limit() {
            Some(limit) => (limit, limit.saturating_sub(shared.budget.used())),
            None => (0, 0),
        };
        let (files, files_free) = match shared.inode_limit.load(Ordering::Relaxed) {
            u64::MAX => (0, 0),
            limit => (
                limit,
                limit.saturating_sub(shared.live_inodes.load(Ordering::Relaxed)),
            ),
        };
        Ok(FileSystemStatistics {
            type_name: "tmpfs",
            magic: TMPFS_MAGIC,
            block_size: BLOCK_SIZE,
            blocks,
            blocks_free: free,
            blocks_available: free,
            files,
            files_free,
            fsid: [shared.filesystem_id as u32, 0],
            name_length: NAME_MAX as u64,
            fragment_size: BLOCK_SIZE,
            flags: 0,
        })
    }
}

/// tmpfs 类型：`size=`、`nr_blocks=`、`nr_inodes=`、`mode=`、`uid=`、`gid=`。
pub(super) struct TmpFileSystemType;

impl FileSystemType for TmpFileSystemType {
    fn name(&self) -> &'static str {
        "tmpfs"
    }

    fn create(&self, request: &MountRequest<'_>) -> Result<Arc<dyn FileSystem>, FileSystemError> {
        let ram_pages = request.environment.proc_source.snapshot()?.total_pages as u64;
        let options = options::parse(request.options, ram_pages, options::defaults(ram_pages))
            .map_err(|_| FileSystemError::InvalidOperation)?;
        let filesystem = TmpFileSystem::new(&options, ram_pages)?;
        Ok(filesystem)
    }
}
