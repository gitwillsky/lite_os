//! 标准 FUSE 操作翻译；节点和打开句柄分别计数，目录 offset 绑定本次 opendir 的快照。
use crate::remote::{DirectoryEntry, DirectorySnapshot, State, errno, read_attributes, timestamp};
use fuser::*;
use smb::{
    CreateDisposition, CreateOptions, FileAccessMask, FileAttributes, FileBasicInformation,
    FileCreateArgs, FileDirectoryInformation, FileDispositionInformation, FileEndOfFileInformation,
    FileFsSizeInformation, FileRenameInformation, ReadAt, WriteAt,
};
use std::{
    ffi::OsStr,
    sync::Mutex,
    time::{Duration, Instant, SystemTime},
};

// 内核独占标准 FUSE 名字/属性缓存，最多 1 秒；零 TTL 会让每次 stat 都重新 OPEN/QUERY_INFO/CLOSE。
// 本地修改由内核作废相应缓存；外部修改在此有界窗口后重新读取，不维护第二份持久属性缓存。
const TTL: Duration = Duration::from_secs(1);
pub(crate) struct SmbFilesystem {
    pub(crate) state: Mutex<State>,
    pub(crate) ready: std::sync::mpsc::SyncSender<()>,
}
impl SmbFilesystem {
    fn call<T>(&self, operation: impl FnOnce(&mut State) -> Result<T, Errno>) -> Result<T, Errno> {
        let mut state = self.state.lock().map_err(|_| Errno::EIO)?;
        let result = operation(&mut state);
        if let Err(error) = result {
            state.failed(error);
        }
        result
    }
    fn delete(&self, parent: INodeNo, name: &OsStr, directory: bool) -> Result<(), Errno> {
        self.call(|state| {
            let path = state.child(parent, name)?;
            let mut args =
                FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true));
            args.options = if directory {
                CreateOptions::new().with_directory_file(true)
            } else {
                CreateOptions::new().with_non_directory_file(true)
            };
            let remote = state.open(&path, &args)?;
            remote
                .handle()
                .set_info(FileDispositionInformation::default())
                .map_err(errno)?;
            remote.handle().close().map_err(errno)?;
            state.removed(&path);
            Ok(())
        })
    }
    fn make(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        directory: bool,
        flags: i32,
    ) -> Result<(FileAttr, u64), Errno> {
        self.call(|state| {
            let path = state.child(parent, name)?;
            let mut args = open_args(OpenFlags(flags));
            args.disposition = if flags & 0x80 != 0 {
                CreateDisposition::Create
            } else if flags & 0x200 != 0 {
                CreateDisposition::OverwriteIf
            } else {
                CreateDisposition::OpenIf
            };
            args.options = if directory {
                CreateOptions::new().with_directory_file(true)
            } else {
                CreateOptions::new().with_non_directory_file(true)
            };
            args.attributes = FileAttributes::new().with_directory(directory);
            let remote = state.open(&path, &args)?;
            let ino = state.node(path)?;
            let attr = remote.attr(ino, req.uid(), req.gid())?;
            state.publish_lookup(ino)?;
            match state.install(ino, remote, None, flags & 3 != 0) {
                Ok(handle) => Ok((attr, handle)),
                Err(error) => {
                    state.forget(ino, 1);
                    Err(error)
                }
            }
        })
    }
}
impl Filesystem for SmbFilesystem {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        config
            .set_max_write(65536)
            .map_err(|_| std::io::ErrorKind::Unsupported)?;
        config
            .set_max_readahead(65536)
            .map_err(|_| std::io::ErrorKind::Unsupported)?;
        config
            .add_capabilities(InitFlags::FUSE_AUTO_INVAL_DATA | InitFlags::FUSE_DO_READDIRPLUS)
            .map_err(|_| std::io::ErrorKind::Unsupported)?;
        self.ready.send(()).map_err(std::io::Error::other)?;
        Ok(())
    }
    fn destroy(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            state.shutdown();
        }
    }
    fn access(&self, _req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let result = self.call(|state| {
            let path = state.path(ino)?;
            let mut access = FileAccessMask::new().with_file_read_attributes(true);
            if mask.bits() & 4 != 0 {
                access = access.with_generic_read(true);
            }
            if mask.bits() & 2 != 0 {
                access = access.with_generic_write(true);
            }
            if mask.bits() & 1 != 0 {
                access = access.with_file_execute(true);
            }
            state.open(&path, &FileCreateArgs::make_open_existing(access))?;
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }
    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let result = self.call(|state| {
            let path = state.child(parent, name)?;
            let remote = state.open(&path, &read_attributes())?;
            let ino = state.node(path)?;
            let attr = remote.attr(ino, req.uid(), req.gid())?;
            state.publish_lookup(ino)?;
            Ok(attr)
        });
        match result {
            Ok(attr) => reply.entry(&TTL, &attr, Generation(1)),
            Err(e) => reply.error(e),
        }
    }
    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        let _ = self.call(|state| {
            state.forget(ino, nlookup);
            Ok(())
        });
    }
    fn getattr(&self, req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let result = self.call(|state| match fh {
            Some(fh) => state
                .file(ino, fh.0)?
                .remote
                .attr(ino, req.uid(), req.gid()),
            None => state.metadata(ino, req.uid(), req.gid()),
        });
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let result = self.call(|state| {
            let path = state.path(ino)?;
            let remote = state.open(&path, &open_args(flags))?;
            if !remote.0.is_file() {
                return Err(Errno::EISDIR);
            }
            state.install(ino, remote, None, flags.acc_mode() != OpenAccMode::O_RDONLY)
        });
        match result {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }
    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        match self.make(req, parent, name, false, flags) {
            Ok((attr, fh)) => reply.created(
                &TTL,
                &attr,
                Generation(1),
                FileHandle(fh),
                FopenFlags::empty(),
            ),
            Err(e) => reply.error(e),
        }
    }
    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        match self.make(req, parent, name, true, 0x82) {
            Ok((attr, fh)) => {
                let _ = self.call(|state| {
                    state.files.remove(&fh);
                    Ok(())
                });
                reply.entry(&TTL, &attr, Generation(1));
            }
            Err(e) => reply.error(e),
        }
    }
    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _rdev: u32,
        reply: ReplyEntry,
    ) {
        if mode & 0o170000 != 0o100000 {
            reply.error(Errno::EOPNOTSUPP);
            return;
        }
        match self.make(req, parent, name, false, 0x82) {
            Ok((attr, fh)) => {
                let _ = self.call(|state| {
                    state.files.remove(&fh);
                    Ok(())
                });
                reply.entry(&TTL, &attr, Generation(1));
            }
            Err(e) => reply.error(e),
        }
    }
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let result = self.call(|state| {
            let file = state
                .file(ino, fh.0)?
                .remote
                .0
                .as_file()
                .ok_or(Errno::EISDIR)?;
            let mut bytes = vec![0; size as usize];
            // FUSE 的缓存读取把短回复视为 EOF；不能将 SMB 单包/credit 边界误报成文件结束。
            let mut length = 0;
            while length < bytes.len() {
                let end = (length + 65536).min(bytes.len());
                let position = offset.checked_add(length as u64).ok_or(Errno::EOVERFLOW)?;
                let read = file
                    .read_at(&mut bytes[length..end], position)
                    .map_err(errno)?;
                if read == 0 {
                    break;
                }
                length += read;
            }
            bytes.truncate(length);
            Ok(bytes)
        });
        match result {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e),
        }
    }
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let result = self.call(|state| {
            state
                .file(ino, fh.0)?
                .remote
                .0
                .as_file()
                .ok_or(Errno::EISDIR)?
                .write_at(data, offset)
                .map_err(errno)
        });
        match result {
            Ok(length) => reply.written(length as u32),
            Err(e) => reply.error(e),
        }
    }
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let result = self.call(|state| {
            let opened = state.file(ino, fh.0)?;
            if !opened.writable {
                return Ok(());
            }
            opened
                .remote
                .0
                .as_file()
                .ok_or(Errno::EISDIR)?
                .flush()
                .map_err(|error| errno(smb::Error::IoError(error)))
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn fsync(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.flush(req, ino, fh, LockOwner(0), reply);
    }
    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let result = self.call(|state| {
            state.file(ino, fh.0)?;
            let file = state.files.remove(&fh.0).ok_or(Errno::ESTALE)?;
            let result = file.remote.handle().close().map_err(errno);
            state.collect();
            result
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn opendir(&self, req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let result = self.call(|state| {
            let path = state.path(ino)?;
            let mut args =
                FileCreateArgs::make_open_existing(FileAccessMask::new().with_generic_read(true));
            args.options = CreateOptions::new().with_directory_file(true);
            let remote = state.open(&path, &args)?;
            let directory = remote.0.as_dir().ok_or(Errno::ENOTDIR)?;
            let mut entries = vec![DirectoryEntry {
                attr: remote.attr(ino, req.uid(), req.gid())?,
                name: ".".into(),
            }];
            let parent_path = path.rsplit_once('\\').map_or("", |(parent, _)| parent);
            let parent = state.node(parent_path.to_owned())?;
            entries.push(DirectoryEntry {
                attr: state.metadata(parent, req.uid(), req.gid())?,
                name: "..".into(),
            });
            let valid_until = Instant::now() + TTL;
            for item in directory
                .query::<FileDirectoryInformation>("*")
                .map_err(errno)?
            {
                let item = item.map_err(errno)?;
                let name = item.file_name.to_string();
                if matches!(name.as_str(), "." | "..") {
                    continue;
                }
                let child = state.child(ino, OsStr::new(&name))?;
                let child_ino = state.node(child)?;
                let directory = item.file_attributes.directory();
                entries.push(DirectoryEntry {
                    name,
                    attr: FileAttr {
                        ino: child_ino,
                        size: item.end_of_file,
                        blocks: item.allocation_size.div_ceil(512),
                        atime: item.last_access_time.into(),
                        mtime: item.last_write_time.into(),
                        ctime: item.change_time.into(),
                        crtime: item.creation_time.into(),
                        kind: if directory {
                            FileType::Directory
                        } else {
                            FileType::RegularFile
                        },
                        perm: if directory || !item.file_attributes.readonly() {
                            0o700
                        } else {
                            0o500
                        },
                        // SMB 的目录记录没有 POSIX nlink，投影为 1；GETATTR 仍查询服务器实际链接数。
                        nlink: 1,
                        uid: req.uid(),
                        gid: req.gid(),
                        rdev: 0,
                        blksize: 65536,
                        flags: 0,
                    },
                });
            }
            state.install(
                ino,
                remote,
                Some(DirectorySnapshot {
                    entries,
                    valid_until,
                }),
                false,
            )
        });
        match result {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e),
        }
    }
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let result = self.call(|state| {
            let entries = state
                .file(ino, fh.0)?
                .entries
                .as_ref()
                .ok_or(Errno::ENOTDIR)?;
            let offset = usize::try_from(offset).map_err(|_| Errno::EINVAL)?;
            for (index, entry) in entries.entries.iter().enumerate().skip(offset) {
                if reply.add(
                    entry.attr.ino,
                    index as u64 + 1,
                    entry.attr.kind,
                    &entry.name,
                ) {
                    break;
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let result = self.call(|state| {
            let opened = state
                .files
                .get(&fh.0)
                .filter(|file| file.ino == ino)
                .ok_or(Errno::ESTALE)?;
            let snapshot = opened.entries.as_ref().ok_or(Errno::ENOTDIR)?;
            let begin = usize::try_from(offset)
                .map_err(|_| Errno::EINVAL)?
                .min(snapshot.entries.len());
            let ttl = snapshot
                .valid_until
                .saturating_duration_since(Instant::now());
            let nodes = &mut state.nodes;
            let mut end = begin;
            let result = (|| {
                for (index, entry) in snapshot.entries.iter().enumerate().skip(begin) {
                    let lookup = !matches!(entry.name.as_str(), "." | "..");
                    let node = nodes.get_mut(&entry.attr.ino).ok_or(Errno::ESTALE)?;
                    let next = if lookup {
                        node.lookups.checked_add(1).ok_or(Errno::EOVERFLOW)?
                    } else {
                        node.lookups
                    };
                    if reply.add(
                        entry.attr.ino,
                        index as u64 + 1,
                        &entry.name,
                        &ttl,
                        &entry.attr,
                        Generation(1),
                    ) {
                        break;
                    }
                    // 只为实际进入回复的非点目录项发布 lookup；满缓冲区中的候选项没有获得引用。
                    node.lookups = next;
                    end = index + 1;
                }
                Ok(())
            })();
            if result.is_err() {
                // 整个回复会被 errno 替代，撤销此前为本回复取得的所有引用。
                for entry in &snapshot.entries[begin..end] {
                    if !matches!(entry.name.as_str(), "." | "..")
                        && let Some(node) = nodes.get_mut(&entry.attr.ino)
                    {
                        node.lookups -= 1;
                    }
                }
            }
            result
        });
        match result {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }
    fn releasedir(
        &self,
        req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.release(req, ino, fh, flags, None, false, reply);
    }
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.delete(parent, name, false) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        match self.delete(parent, name, true) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let result = self.call(|state| {
            if flags.bits() & !1 != 0 {
                return Err(Errno::EOPNOTSUPP);
            }
            let old = state.child(parent, name)?;
            let new = state.child(newparent, newname)?;
            if old == new {
                return Ok(());
            }
            let remote = state.open(
                &old,
                &FileCreateArgs::make_open_existing(FileAccessMask::new().with_delete(true)),
            )?;
            remote
                .handle()
                .set_info(FileRenameInformation {
                    replace_if_exists: (flags.bits() & 1 == 0).into(),
                    root_directory: 0,
                    file_name: new.as_str().into(),
                })
                .map_err(errno)?;
            state.renamed(&old, &new);
            Ok(())
        });
        match result {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e),
        }
    }
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let result = self.call(|state| {
            // Windows ACL 是权限事实；不能把 chmod/chown 成功伪装成已持久更新 POSIX 权限。
            if mode.is_some() || uid.is_some() || gid.is_some() || flags.is_some() {
                return Err(Errno::EOPNOTSUPP);
            }
            let temporary;
            let remote = if let Some(fh) = fh {
                &state.file(ino, fh.0)?.remote
            } else {
                let path = state.path(ino)?;
                temporary = state.open(
                    &path,
                    &FileCreateArgs::make_open_existing(
                        FileAccessMask::new()
                            .with_generic_write(true)
                            .with_file_read_attributes(true),
                    ),
                )?;
                &temporary
            };
            if let Some(end_of_file) = size {
                remote
                    .handle()
                    .set_info(FileEndOfFileInformation { end_of_file })
                    .map_err(errno)?;
            }
            if atime.is_some() || mtime.is_some() {
                let mut info: FileBasicInformation = remote.handle().query_info().map_err(errno)?;
                if let Some(value) = atime {
                    info.last_access_time = file_time(timestamp(value))?;
                }
                if let Some(value) = mtime {
                    info.last_write_time = file_time(timestamp(value))?;
                }
                remote.handle().set_info(info).map_err(errno)?;
            }
            remote.attr(ino, req.uid(), req.gid())
        });
        match result {
            Ok(attr) => reply.attr(&TTL, &attr),
            Err(e) => reply.error(e),
        }
    }
    fn statfs(&self, _req: &Request, ino: INodeNo, reply: ReplyStatfs) {
        let result = self.call(|state| {
            let path = state.path(ino)?;
            state
                .open(&path, &read_attributes())?
                .handle()
                .query_fs_info::<FileFsSizeInformation>()
                .map_err(errno)
        });
        match result {
            Ok(info) => {
                let size = info
                    .sectors_per_allocation_unit
                    .saturating_mul(info.bytes_per_sector);
                reply.statfs(
                    info.total_allocation_units,
                    info.available_allocation_units,
                    info.available_allocation_units,
                    0,
                    0,
                    size,
                    255,
                    size,
                );
            }
            Err(e) => reply.error(e),
        }
    }
    fn getxattr(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _name: &OsStr,
        _size: u32,
        reply: ReplyXattr,
    ) {
        reply.error(if _name == OsStr::new("security.capability") {
            Errno::NO_XATTR
        } else {
            Errno::EOPNOTSUPP
        });
    }
}
fn open_args(flags: OpenFlags) -> FileCreateArgs {
    let mode = flags.0 & 3;
    let mut args = FileCreateArgs::make_open_existing(
        FileAccessMask::new()
            .with_generic_read(mode != 1)
            .with_generic_write(mode != 0)
            .with_file_read_attributes(true),
    );
    args.options = CreateOptions::new().with_non_directory_file(true);
    if flags.0 & 0x200 != 0 {
        args.disposition = CreateDisposition::Overwrite;
    }
    args
}

fn file_time(time: SystemTime) -> Result<smb::binrw_util::prelude::FileTime, Errno> {
    let epoch = SystemTime::UNIX_EPOCH
        .checked_sub(Duration::from_secs(11_644_473_600))
        .ok_or(Errno::EOVERFLOW)?;
    let duration = time.duration_since(epoch).map_err(|_| Errno::EOVERFLOW)?;
    let ticks = duration
        .as_secs()
        .checked_mul(10_000_000)
        .and_then(|value| value.checked_add(u64::from(duration.subsec_nanos() / 100)))
        .ok_or(Errno::EOVERFLOW)?;
    Ok(ticks.into())
}
