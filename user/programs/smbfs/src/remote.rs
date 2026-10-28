//! SMB 会话与 FUSE 节点/句柄的唯一 owner；不把不确定是否完成的写操作自动重放。
use fuser::{Errno, FileAttr, FileType, INodeNo};
use network_drive::Credentials;
use smb::{
    Client, ClientConfig, ConnectionConfig, Dialect, FileAccessMask, FileAllInformation,
    FileCreateArgs, Resource, ResourceHandle, UncPath, connection::config::EncryptionMode,
};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    time::{Duration, Instant, SystemTime},
};

type Result<T> = std::result::Result<T, Errno>;

pub(crate) struct Remote(pub Resource);
impl Remote {
    pub(crate) fn handle(&self) -> &ResourceHandle {
        match &self.0 {
            Resource::File(value) => value,
            Resource::Directory(value) => value,
            Resource::Pipe(value) => value,
        }
    }
    pub(crate) fn attr(&self, ino: INodeNo, uid: u32, gid: u32) -> Result<FileAttr> {
        let info: FileAllInformation = self.handle().query_info().map_err(errno)?;
        let directory = info.basic.file_attributes.directory();
        Ok(FileAttr {
            ino,
            size: info.standard.end_of_file,
            blocks: info.standard.allocation_size.div_ceil(512),
            atime: info.basic.last_access_time.into(),
            mtime: info.basic.last_write_time.into(),
            ctime: info.basic.change_time.into(),
            crtime: info.basic.creation_time.into(),
            kind: if directory {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if directory {
                0o700
            } else if info.basic.file_attributes.readonly() {
                0o500
            } else {
                0o700
            },
            nlink: info.standard.number_of_links.max(1),
            uid,
            gid,
            rdev: 0,
            blksize: 65536,
            flags: 0,
        })
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        let _ = self.handle().close();
    }
}

pub(crate) struct Node {
    pub(crate) path: String,
    pub(crate) lookups: u64,
    pub(crate) linked: bool,
}
pub(crate) struct Opened {
    pub(crate) ino: INodeNo,
    pub(crate) remote: Remote,
    /// 实际 OPEN 的写入权限；MAXIMAL_ACCESS 只是 ACL 上限，误向只读句柄 FLUSH 会让 close 被服务器拒绝。
    pub(crate) writable: bool,
    pub(crate) entries: Option<DirectorySnapshot>,
}
pub(crate) struct DirectoryEntry {
    pub(crate) attr: FileAttr,
    pub(crate) name: String,
}
pub(crate) struct DirectorySnapshot {
    pub(crate) entries: Vec<DirectoryEntry>,
    /// QUERY_DIRECTORY 的属性只在该期限内有效；旧目录句柄不能重新延长远端属性的有效期。
    pub(crate) valid_until: Instant,
}
pub(crate) struct State {
    client: Option<Client>,
    share: UncPath,
    credentials: Credentials,
    encrypt: bool,
    port: Option<u16>,
    pub(crate) nodes: BTreeMap<INodeNo, Node>,
    pub(crate) files: BTreeMap<u64, Opened>,
    next_node: u64,
    next_file: u64,
}
impl State {
    pub(crate) fn new(
        source: &str,
        credentials: Credentials,
        encrypt: bool,
    ) -> std::io::Result<Self> {
        let source = network_drive::normalize_source(source)?;
        let (server, name) = source[2..]
            .split_once('/')
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        let (server, port) = match server.rsplit_once(':') {
            Some((host, port)) => (
                host,
                Some(
                    port.parse::<u16>()
                        .ok()
                        .filter(|port| *port != 0)
                        .ok_or(std::io::ErrorKind::InvalidInput)?,
                ),
            ),
            None => (server, None),
        };
        let unc = format!("//{server}/{name}");
        let share: UncPath = unc
            .replace('/', "\\")
            .parse()
            .map_err(std::io::Error::other)?;
        let mut state = Self {
            client: None,
            share,
            credentials,
            encrypt,
            port,
            nodes: BTreeMap::from([(
                INodeNo(1),
                Node {
                    path: String::new(),
                    lookups: 1,
                    linked: true,
                },
            )]),
            files: BTreeMap::new(),
            next_node: 2,
            next_file: 1,
        };
        state
            .connect()
            .map_err(|error| std::io::Error::from_raw_os_error(error.code()))?;
        Ok(state)
    }
    fn connect(&mut self) -> Result<()> {
        if self.client.is_some() {
            return Ok(());
        }
        let client = Client::new(ClientConfig {
            dfs: false,
            connection: ConnectionConfig {
                port: self.port,
                min_dialect: Some(Dialect::Smb021),
                max_dialect: Some(Dialect::Smb0311),
                encryption_mode: if self.encrypt {
                    EncryptionMode::Required
                } else {
                    EncryptionMode::Allowed
                },
                timeout: Some(Duration::from_secs(10)),
                smb2_only_negotiate: true,
                disable_notifications: true,
                client_name: Some("LITEOS".into()),
                ..ConnectionConfig::default()
            },
            ..ClientConfig::default()
        });
        let username = if self.credentials.domain.is_empty() {
            self.credentials.username.clone()
        } else {
            format!("{}\\{}", self.credentials.domain, self.credentials.username)
        };
        if let Err(error) =
            client.share_connect(&self.share, &username, self.credentials.password.clone())
        {
            let _ = client.close();
            return Err(errno(error));
        }
        // 挂载只接收磁盘共享的目录根；打印/IPC 共享不能在创建文件时落入上游不支持的资源分支。
        let mut args = read_attributes();
        args.options = smb::CreateOptions::new().with_directory_file(true);
        match client.create_file(&self.share, &args) {
            Ok(resource) => {
                let root = Remote(resource);
                if !root.0.is_dir() {
                    let _ = client.close();
                    return Err(Errno::ENOTDIR);
                }
            }
            Err(error) => {
                let _ = client.close();
                return Err(errno(error));
            }
        }
        self.client = Some(client);
        Ok(())
    }
    pub(crate) fn path(&self, ino: INodeNo) -> Result<String> {
        self.nodes
            .get(&ino)
            .filter(|node| node.linked)
            .map(|node| node.path.clone())
            .ok_or(Errno::ESTALE)
    }
    pub(crate) fn child(&self, parent: INodeNo, name: &OsStr) -> Result<String> {
        let name = name.to_str().ok_or(Errno::EILSEQ)?;
        // SMB 分隔符与 ADS 不能通过单个 Linux 目录项逃逸父路径或打开命名流。
        if name.is_empty()
            || matches!(name, "." | "..")
            || name.chars().any(|c| matches!(c, '/' | '\\' | ':' | '\0'))
        {
            return Err(Errno::EINVAL);
        }
        let parent = self.path(parent)?;
        Ok(if parent.is_empty() {
            name.to_owned()
        } else {
            format!("{parent}\\{name}")
        })
    }
    pub(crate) fn open(&mut self, path: &str, args: &FileCreateArgs) -> Result<Remote> {
        self.connect()?;
        let client = self.client.as_ref().ok_or(Errno::ENOTCONN)?;
        client
            .create_file(&self.share.clone().with_path(path), args)
            .map(Remote)
            .map_err(errno)
    }
    pub(crate) fn metadata(&mut self, ino: INodeNo, uid: u32, gid: u32) -> Result<FileAttr> {
        if let Some(file) = self.files.values().find(|file| file.ino == ino) {
            return file.remote.attr(ino, uid, gid);
        }
        let path = self.path(ino)?;
        self.open(&path, &read_attributes())?.attr(ino, uid, gid)
    }
    pub(crate) fn node(&mut self, path: String) -> Result<INodeNo> {
        if let Some((&ino, _)) = self
            .nodes
            .iter()
            .find(|(_, node)| node.linked && node.path == path)
        {
            return Ok(ino);
        }
        let ino = INodeNo(self.next_node);
        self.next_node = self.next_node.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        self.nodes.insert(
            ino,
            Node {
                path,
                lookups: 0,
                linked: true,
            },
        );
        Ok(ino)
    }
    /// 只在成功回复 entry 前发布 lookup；远程属性读取失败不能留下内核从未获得的引用。
    pub(crate) fn publish_lookup(&mut self, ino: INodeNo) -> Result<()> {
        let node = self.nodes.get_mut(&ino).ok_or(Errno::ESTALE)?;
        node.lookups = node.lookups.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        Ok(())
    }
    pub(crate) fn forget(&mut self, ino: INodeNo, count: u64) {
        if let Some(node) = self.nodes.get_mut(&ino) {
            node.lookups = node.lookups.saturating_sub(count);
        }
        self.collect();
    }
    pub(crate) fn install(
        &mut self,
        ino: INodeNo,
        remote: Remote,
        entries: Option<DirectorySnapshot>,
        writable: bool,
    ) -> Result<u64> {
        let id = self.next_file;
        self.next_file = id.checked_add(1).ok_or(Errno::EOVERFLOW)?;
        self.files.insert(
            id,
            Opened {
                ino,
                remote,
                entries,
                writable,
            },
        );
        Ok(id)
    }
    pub(crate) fn file(&self, ino: INodeNo, fh: u64) -> Result<&Opened> {
        self.files
            .get(&fh)
            .filter(|file| file.ino == ino)
            .ok_or(Errno::ESTALE)
    }
    pub(crate) fn collect(&mut self) {
        self.nodes.retain(|ino, node| {
            ino.0 == 1
                || node.lookups != 0
                || self.files.values().any(|file| {
                    file.ino == *ino
                        || file.entries.as_ref().is_some_and(|snapshot| {
                            snapshot.entries.iter().any(|entry| entry.attr.ino == *ino)
                        })
                })
        });
    }
    pub(crate) fn removed(&mut self, path: &str) {
        for node in self.nodes.values_mut().filter(|node| node.path == path) {
            node.linked = false;
        }
    }
    pub(crate) fn renamed(&mut self, old: &str, new: &str) {
        self.removed(new);
        let prefix = format!("{old}\\");
        for node in self.nodes.values_mut().filter(|node| node.linked) {
            if node.path == old {
                node.path = new.into();
            } else if let Some(tail) = node.path.strip_prefix(&prefix) {
                node.path = format!("{new}\\{tail}");
            }
        }
    }
    pub(crate) fn shutdown(&mut self) {
        self.files.clear();
        if let Some(client) = self.client.take() {
            let _ = client.close();
        }
    }
    pub(crate) fn failed(&mut self, error: Errno) {
        // 1. 传输/完整性失败使整代远程句柄失效；旧 fh 绝不能静默绑定到新文件。
        // 2. 释放 Client 关闭传输，新路径操作才重新认证；当前操作始终把原错误返回调用方。
        if [
            Errno::EIO,
            Errno::ENOTCONN,
            Errno::ETIMEDOUT,
            Errno::ECONNRESET,
        ]
        .iter()
        .any(|candidate| candidate.code() == error.code())
        {
            if let Some(client) = self.client.take() {
                let _ = client.close();
            }
            self.files.clear();
        }
        self.collect();
    }
}
pub(crate) fn read_attributes() -> FileCreateArgs {
    FileCreateArgs::make_open_existing(FileAccessMask::new().with_file_read_attributes(true))
}
pub(crate) fn errno(error: smb::Error) -> Errno {
    match error {
        smb::Error::ReceivedErrorMessage(status, _)
        | smb::Error::UnexpectedMessageStatus(status) => match status {
            0xc000000f | 0xc0000034 | 0xc000003a => Errno::ENOENT,
            0xc0000022 | 0xc000006d | 0xc000006a | 0xc0000072 | 0xc0000234 => Errno::EACCES,
            0xc0000035 => Errno::EEXIST,
            0xc0000101 => Errno::ENOTEMPTY,
            0xc000007f | 0xc0000044 => Errno::ENOSPC,
            0xc00000a2 => Errno::EROFS,
            0xc0000043 | 0xc0000056 => Errno::EBUSY,
            0xc0000103 => Errno::ENOTDIR,
            0xc00000ba => Errno::EISDIR,
            0xc0000008 | 0xc0000128 => Errno::ESTALE,
            0xc00000bb => Errno::EOPNOTSUPP,
            0xc000000d => Errno::EINVAL,
            0xc00000b5 => Errno::ETIMEDOUT,
            0xc00000c9 | 0xc000020c => Errno::ENOTCONN,
            _ => Errno::EIO,
        },
        smb::Error::IoError(error) => {
            if error
                .get_ref()
                .is_some_and(|inner| inner.is::<smb::Error>())
            {
                match error
                    .into_inner()
                    .and_then(|inner| inner.downcast::<smb::Error>().ok())
                {
                    Some(inner) => errno(*inner),
                    None => Errno::EIO,
                }
            } else if let Some(code) = error.raw_os_error() {
                Errno::from_i32(code)
            } else {
                match error.kind() {
                    std::io::ErrorKind::PermissionDenied => Errno::EACCES,
                    std::io::ErrorKind::NotFound => Errno::ENOENT,
                    std::io::ErrorKind::AlreadyExists => Errno::EEXIST,
                    std::io::ErrorKind::InvalidInput => Errno::EINVAL,
                    std::io::ErrorKind::Unsupported => Errno::EOPNOTSUPP,
                    std::io::ErrorKind::TimedOut => Errno::ETIMEDOUT,
                    std::io::ErrorKind::NotConnected => Errno::ENOTCONN,
                    _ => Errno::EIO,
                }
            }
        }
        smb::Error::OperationTimeout(..) => Errno::ETIMEDOUT,
        smb::Error::ConnectionStopped | smb::Error::TransportError(_) => Errno::ENOTCONN,
        smb::Error::SspiError(_) => Errno::EACCES,
        smb::Error::SignatureVerificationFailed => Errno::EIO,
        smb::Error::UnsupportedOperation(_) => Errno::EOPNOTSUPP,
        _ => Errno::EIO,
    }
}
pub(crate) fn timestamp(value: fuser::TimeOrNow) -> SystemTime {
    match value {
        fuser::TimeOrNow::SpecificTime(time) => time,
        fuser::TimeOrNow::Now => SystemTime::now(),
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.shutdown();
    }
}
