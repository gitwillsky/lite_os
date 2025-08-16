use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use spin::Mutex;

use crate::task;

use super::{FileSystem, FileSystemError, Inode};
use crate::ipc::{open_fifo, create_fifo};

pub struct VirtualFileSystem {
    filesystems: Mutex<BTreeMap<String, Arc<dyn FileSystem>>>,
    root_fs: Mutex<Option<Arc<dyn FileSystem>>>,
}

impl VirtualFileSystem {
    pub fn new() -> Self {
        Self {
            filesystems: Mutex::new(BTreeMap::new()),
            root_fs: Mutex::new(None),
        }
    }

    /// 将相对路径转换为绝对路径
    pub fn resolve_relative_path(&self, path: &str) -> String {
        if path.starts_with('/') {
            // 已经是绝对路径
            self.canonicalize_path(path)
        } else {
            // 相对路径：结合当前工作目录
            let cwd = task::current_cwd();
            let combined = if cwd.ends_with('/') {
                format!("{}{}", cwd, path)
            } else {
                format!("{}/{}", cwd, path)
            };
            self.canonicalize_path(&combined)
        }
    }

    /// 规范化路径，解析 . 和 .. 组件
    fn canonicalize_path(&self, path: &str) -> String {
        let mut components = Vec::new();

        for component in path.split('/') {
            match component {
                "" | "." => {
                    // 跳过空组件和当前目录引用
                    continue;
                }
                ".." => {
                    // 父目录引用：弹出最后一个组件（如果有的话）
                    components.pop();
                }
                _ => {
                    // 普通目录组件
                    components.push(component);
                }
            }
        }

        // 重新构建路径
        let canonical = if components.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", components.join("/"))
        };

        canonical
    }

    pub fn mount(&self, path: &str, fs: Arc<dyn FileSystem>) -> Result<(), FileSystemError> {
        let mut filesystems = self.filesystems.lock();

        if path == "/" {
            *self.root_fs.lock() = Some(fs.clone());
        }

        filesystems.insert(path.to_string(), fs);
        Ok(())
    }

    pub fn unmount(&self, path: &str) -> Result<(), FileSystemError> {
        let mut filesystems = self.filesystems.lock();

        if path == "/" {
            *self.root_fs.lock() = None;
        }

        filesystems.remove(path);
        Ok(())
    }

    pub fn open(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.open_with_flags(path, 0)
    }

    pub fn open_with_flags(
        &self,
        path: &str,
        flags: u32,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);

        // Extract access mode from flags
        const O_RDONLY: u32 = 0o0;
        const O_WRONLY: u32 = 0o1;
        const O_RDWR: u32 = 0o2;
        let access_mode = flags & 0o3;

        // 若路径在 FIFO 注册表中，直接返回 FIFO 端点
        if let Ok(fifo) = open_fifo(&abs_path) {
            let nonblock = (flags & 0o4000) != 0; // O_NONBLOCK
            return match access_mode {
                O_RDONLY => Ok(if nonblock { fifo.open_read_with_flags(true) } else { fifo.open_read() } as Arc<dyn Inode>),
                O_WRONLY => Ok(if nonblock { fifo.open_write_with_flags(true) } else { fifo.open_write() } as Arc<dyn Inode>),
                O_RDWR => Ok(fifo as Arc<dyn Inode>),
                _ => Ok(if nonblock { fifo.open_read_with_flags(true) } else { fifo.open_read() } as Arc<dyn Inode>),
            };
        }

        // Input device nodes
        if abs_path.starts_with("/dev/input/") {
            if let Ok(node) = crate::drivers::open_input_device(&abs_path) {
                return Ok(node);
            }
        }

        if abs_path == "/" {
            let root_fs = self.root_fs.lock();
            let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;
            return Ok(fs.root_inode());
        }

        // 解析路径；若为底层文件系统中的 FIFO 节点，则确保在注册表中创建对应的命名管道对象
        match self.resolve_path(&abs_path) {
            Ok(inode) => {
                if matches!(inode.inode_type(), super::InodeType::Fifo) {
                    // 在注册表中创建（若已存在则忽略），随后按访问模式返回 FIFO 端点
                    let _ = create_fifo(&abs_path);
                    if let Ok(fifo) = open_fifo(&abs_path) {
                        // 将底层权限信息携带到 FIFO 对象，保持 stat/权限检查一致
                        let perm = inode.mode();
                        let uid = inode.uid();
                        let gid = inode.gid();
                        fifo.maybe_init_meta(perm, uid, gid);
                        let nonblock = (flags & 0o4000) != 0; // O_NONBLOCK
                        return match access_mode {
                            O_RDONLY => Ok(if nonblock { fifo.open_read_with_flags(true) } else { fifo.open_read() } as Arc<dyn Inode>),
                            O_WRONLY => Ok(if nonblock { fifo.open_write_with_flags(true) } else { fifo.open_write() } as Arc<dyn Inode>),
                            O_RDWR => Ok(fifo as Arc<dyn Inode>),
                            _ => Ok(if nonblock { fifo.open_read_with_flags(true) } else { fifo.open_read() } as Arc<dyn Inode>),
                        };
                    }
                }
                Ok(inode)
            }
            Err(e) => Err(e)
        }
    }

    fn resolve_path(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        // 先检查挂载点前缀匹配（最长匹配）
        let filesystems = self.filesystems.lock();
        let mut best_match_len: isize = -1;
        let mut best_fs: Option<Arc<dyn FileSystem>> = None;
        for (mpath, fs) in filesystems.iter() {
            if mpath == "/" {
                continue; // 根挂载由 root_fs 负责
            }
            if path == mpath {
                // 精确匹配挂载点
                best_match_len = mpath.len() as isize;
                best_fs = Some(fs.clone());
                break;
            } else if path.starts_with(mpath) && path.as_bytes().get(mpath.len()) == Some(&b'/') {
                // 前缀匹配，且边界为 '/'
                let len = mpath.len() as isize;
                if len > best_match_len { best_match_len = len; best_fs = Some(fs.clone()); }
            }
        }
        drop(filesystems);

        if let Some(fs) = best_fs {
            let mut current = fs.root_inode();
            if best_match_len as usize == path.len() {
                return Ok(current);
            }
            let mut remain = &path[best_match_len as usize + 1..]; // skip the '/'
            if remain.starts_with('/') { remain = &remain[1..]; }
            if remain.is_empty() { return Ok(current); }
            for component in remain.split('/') {
                if component.is_empty() { continue; }
                current = current.find_child(component)?;
            }
            return Ok(current);
        }

        // 否则走根文件系统
        let root_fs = self.root_fs.lock();
        let fs = root_fs.as_ref().ok_or(FileSystemError::NotFound)?;

        let mut current = fs.root_inode();

        let path = if path.starts_with('/') { &path[1..] } else { path };
        if path.is_empty() { return Ok(current); }
        for component in path.split('/') {
            if component.is_empty() { continue; }
            current = current.find_child(component)?;
        }
        Ok(current)
    }

    pub fn create_file(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, filename) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_file(&filename)
    }

    pub fn create_directory(&self, path: &str) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, dirname) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.create_directory(&dirname)
    }

    /// Create a named pipe (FIFO) persistently in the underlying filesystem
    pub fn create_fifo(&self, path: &str, mode: u32) -> Result<Arc<dyn Inode>, FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, name) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        // If already exists
        match parent.find_child(&name) {
            Ok(inode) => {
                return if matches!(inode.inode_type(), super::InodeType::Fifo) {
                    Ok(inode)
                } else {
                    Err(FileSystemError::AlreadyExists)
                };
            }
            Err(FileSystemError::NotFound) => {}
            Err(e) => return Err(e),
        }
        // 在底层文件系统中创建一个 FIFO 节点：采用 create_file 再 set_mode 的方式设置 S_IFIFO
        let fifo = parent.create_file(&name)?;
        // S_IFIFO: 0010000 (八进制) => 0x1000；ext2 使用 i_mode 的高位表示类型
        let fifo_type_bits = 0x1000u32;
        let perm_bits = mode & 0o777;
        fifo.set_mode(fifo_type_bits | perm_bits)?;
        Ok(fifo)
    }

    pub fn remove(&self, path: &str) -> Result<(), FileSystemError> {
        let abs_path = self.resolve_relative_path(path);
        let (parent_path, filename) = self.split_path(&abs_path)?;
        let parent = self.resolve_path(&parent_path)?;
        parent.remove(&filename)
    }

    fn split_path(&self, path: &str) -> Result<(String, String), FileSystemError> {
        if !path.starts_with('/') {
            return Err(FileSystemError::InvalidPath);
        }

        let path = &path[1..];
        if path.is_empty() {
            return Err(FileSystemError::InvalidPath);
        }

        if let Some(pos) = path.rfind('/') {
            let parent_path = format!("/{}", &path[..pos]);
            let filename = path[pos + 1..].to_string();
            if filename.is_empty() {
                return Err(FileSystemError::InvalidPath);
            }
            Ok((parent_path, filename))
        } else {
            Ok(("/".to_string(), path.to_string()))
        }
    }
}

use spin::Once;

pub static VFS_MANAGER: Once<VirtualFileSystem> = Once::new();

pub fn init() {
    VFS_MANAGER.call_once(|| VirtualFileSystem::new());
}

pub fn vfs() -> &'static VirtualFileSystem {
    VFS_MANAGER.wait()
}
