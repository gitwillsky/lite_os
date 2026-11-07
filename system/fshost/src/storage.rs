//! 原生数据卷与各服务的私有目录（Fuchsia 的 `data` 分区与 `storage` 能力）。
//!
//! 1. 数据卷是卷标为 [`DATA_LABEL`] 的 ext4 块设备；它不交给 Linux，由 fshost 经 ext4 读写挂载；
//! 2. fshost 作为该挂载唯一的 FUSE 客户端：请求串行发出（一次一个在途请求），只用到 LOOKUP、MKDIR、
//!    CREATE、OPEN、READ、WRITE、FSYNC、RELEASE、RENAME 与 FORGET；
//! 3. 每个服务的私有目录是数据卷根下的同名目录；写入先写临时文件并 fsync，再 rename 覆盖，
//!    读者只会看到完整的旧内容或新内容。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use device_model::BlockDevice;
use filesystem_protocol::{RemoteMount, STORAGE_FILE_MAX, StorageService};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status, signals};
use native_runtime::sync::Mutex;

/// 数据卷的 ext4 卷标（`mkfs.ext4 -L`）。
pub(crate) const DATA_LABEL: &[u8] = b"liteos-data";
/// ext4 超级块起点、魔数与卷标在超级块中的偏移。
const SUPERBLOCK: usize = 1024;
const MAGIC_OFFSET: usize = 0x38;
const LABEL_OFFSET: usize = 0x78;
const EXT4_MAGIC: u16 = 0xef53;

const FUSE_KERNEL_VERSION: u32 = 7;
const FUSE_KERNEL_MINOR: u32 = 45;
const LOOKUP: u32 = 1;
const FORGET: u32 = 2;
const MKDIR: u32 = 9;
const RENAME: u32 = 12;
const OPEN: u32 = 14;
const READ: u32 = 15;
const WRITE: u32 = 16;
const RELEASE: u32 = 18;
const FSYNC: u32 = 20;
const INIT: u32 = 26;
const CREATE: u32 = 35;
const ROOT_ID: u64 = 1;
const IN_HEADER: usize = 40;
const OUT_HEADER: usize = 16;
/// `fuse_entry_out` 的长度；CREATE 的文件句柄紧随其后。
const ENTRY_OUT: usize = 128;
const ENOENT: i32 = 2;
/// 一次 READ/WRITE 的数据量，不超过 ext4 协商的单条消息上限。
const CHUNK: usize = 32 * 1024;
const O_WRONLY: u32 = 1;
const O_TRUNC: u32 = 0o1000;

/// 判断块设备是否为原生数据卷（读取超级块，不改变设备状态）。
pub(crate) fn is_data_volume(device: &dyn BlockDevice) -> bool {
    let block_size = device.block_size();
    if block_size == 0 {
        return false;
    }
    let first = SUPERBLOCK / block_size;
    let span = (SUPERBLOCK + LABEL_OFFSET + 16).div_ceil(block_size) - first;
    let mut bytes = alloc::vec![0u8; span * block_size];
    for (index, chunk) in bytes.chunks_mut(block_size).enumerate() {
        if device.read_blocks(first + index, chunk).is_err() {
            return false;
        }
    }
    let base = SUPERBLOCK - first * block_size;
    let magic = u16::from_le_bytes([bytes[base + MAGIC_OFFSET], bytes[base + MAGIC_OFFSET + 1]]);
    let label = &bytes[base + LABEL_OFFSET..base + LABEL_OFFSET + 16];
    let length = label.iter().position(|byte| *byte == 0).unwrap_or(16);
    magic == EXT4_MAGIC && &label[..length] == DATA_LABEL
}

/// 数据卷的 FUSE 会话。
pub(crate) struct DataVolume {
    // OWNER: 唯一在途请求的串行化；FUSE 回复按 unique 匹配，单在途请求让回复总是下一条消息。
    fuse: Mutex<Channel>,
    unique: AtomicU64,
    _control: RemoteMount,
}

/// 请求体编码。
struct Body(Vec<u8>);

impl Body {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn u32(mut self, value: u32) -> Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }
    fn u64(mut self, value: u64) -> Self {
        self.0.extend_from_slice(&value.to_ne_bytes());
        self
    }
    fn name(mut self, name: &[u8]) -> Self {
        self.0.extend_from_slice(name);
        self.0.push(0);
        self
    }
    fn bytes(mut self, bytes: &[u8]) -> Self {
        self.0.extend_from_slice(bytes);
        self
    }
}

fn u64_at(bytes: &[u8], offset: usize) -> Result<u64, Status> {
    bytes
        .get(offset..offset + 8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u64::from_ne_bytes)
        .ok_or(native_abi::ERR_IO)
}

/// FUSE 回复的错误：errno 或传输失败。
enum Failure {
    Errno(i32),
    Transport(Status),
}

impl From<Failure> for Status {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Errno(_) => native_abi::ERR_IO,
            Failure::Transport(status) => status,
        }
    }
}

impl DataVolume {
    /// 在专属 ext4 实例上读写挂载数据卷并完成 FUSE INIT。
    ///
    /// 数据卷是原生服务的持久状态：实例终止时 fshost 失败停机（关键组件，根服务随之关机），
    /// 不能在数据卷状态未知时继续提供存储能力。
    ///
    /// # Errors
    ///
    /// 实例启动、设备共享、挂载或 INIT 失败。
    pub(crate) fn mount(
        launcher: &crate::volumes::Launcher,
        device: &dyn BlockDevice,
    ) -> Result<Arc<Self>, Status> {
        let block = device
            .share_range(0, device.block_count())
            .ok_or(native_abi::ERR_NOT_SUPPORTED)?;
        let instance = launcher.launch()?;
        let (fuse, control) = instance.control.mount(block, false)?;
        let watched = instance.process;
        let watcher = native_runtime::thread::spawn(b"data-volume-watch", move || {
            let _ = watched.wait_one(signals::TERMINATED, native_abi::DEADLINE_INFINITE);
            panic!("data volume filesystem terminated");
        })?;
        drop(watcher);
        let volume = Arc::try_new(Self {
            fuse: Mutex::new(fuse),
            unique: AtomicU64::new(1),
            _control: control,
        })
        .map_err(|_| native_abi::ERR_NO_MEMORY)?;
        let init = Body::new()
            .u32(FUSE_KERNEL_VERSION)
            .u32(FUSE_KERNEL_MINOR)
            .u32(0)
            .u32(0)
            .u32(0)
            .bytes(&[0; 44]);
        volume.call(INIT, 0, &init.0).map_err(Status::from)?;
        Ok(volume)
    }

    fn call(&self, opcode: u32, nodeid: u64, body: &[u8]) -> Result<Vec<u8>, Failure> {
        let unique = self.unique.fetch_add(1, Ordering::Relaxed);
        let length = IN_HEADER + body.len();
        let mut message = Vec::with_capacity(length);
        message.extend_from_slice(&(length as u32).to_ne_bytes());
        message.extend_from_slice(&opcode.to_ne_bytes());
        message.extend_from_slice(&unique.to_ne_bytes());
        message.extend_from_slice(&nodeid.to_ne_bytes());
        // uid、gid、pid：fshost 以 root 身份访问自己的数据卷。
        message.extend_from_slice(&[0; 12]);
        message.extend_from_slice(&[0; 4]);
        message.extend_from_slice(body);
        let fuse = self.fuse.lock();
        filesystem_protocol::send_fuse(&fuse, &message).map_err(Failure::Transport)?;
        if opcode == FORGET {
            return Ok(Vec::new());
        }
        loop {
            let reply = fuse
                .read_wait(native_abi::DEADLINE_INFINITE)
                .and_then(filesystem_protocol::receive_fuse)
                .map_err(Failure::Transport)?;
            if reply.len() < OUT_HEADER || u64_at(&reply, 8).map_err(Failure::Transport)? != unique
            {
                continue;
            }
            let error = i32::from_ne_bytes([reply[4], reply[5], reply[6], reply[7]]);
            if error != 0 {
                return Err(Failure::Errno(-error));
            }
            return Ok(reply[OUT_HEADER..].to_vec());
        }
    }

    fn forget(&self, nodeid: u64) {
        let _ = self.call(FORGET, nodeid, &Body::new().u64(1).0);
    }

    fn lookup(&self, parent: u64, name: &[u8]) -> Result<Option<u64>, Status> {
        match self.call(LOOKUP, parent, &Body::new().name(name).0) {
            Ok(entry) => u64_at(&entry, 0).map(Some),
            Err(Failure::Errno(ENOENT)) => Ok(None),
            Err(failure) => Err(failure.into()),
        }
    }

    /// 服务 `name` 的私有目录节点；不存在时创建（权限 0700，属主 root）。
    pub(crate) fn directory(&self, name: &[u8]) -> Result<u64, Status> {
        if let Some(nodeid) = self.lookup(ROOT_ID, name)? {
            return Ok(nodeid);
        }
        let entry = self
            .call(
                MKDIR,
                ROOT_ID,
                &Body::new().u32(0o40700).u32(0).name(name).0,
            )
            .map_err(Status::from)?;
        u64_at(&entry, 0)
    }

    fn release(&self, nodeid: u64, handle: u64) {
        let _ = self.call(
            RELEASE,
            nodeid,
            &Body::new().u64(handle).u32(0).u32(0).u64(0).0,
        );
        self.forget(nodeid);
    }

    fn read_file(&self, directory: u64, name: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        let Some(nodeid) = self.lookup(directory, name)? else {
            return Ok(None);
        };
        let opened = self.call(OPEN, nodeid, &Body::new().u32(0).u32(0).0);
        let handle = match opened {
            Ok(reply) => u64_at(&reply, 0)?,
            Err(failure) => {
                self.forget(nodeid);
                return Err(failure.into());
            }
        };
        let mut contents = Vec::new();
        let result = loop {
            let request = Body::new()
                .u64(handle)
                .u64(contents.len() as u64)
                .u32(CHUNK as u32)
                .u32(0)
                .u64(0)
                .u32(0)
                .u32(0);
            match self.call(READ, nodeid, &request.0) {
                Ok(data) if data.is_empty() => break Ok(()),
                Ok(data) if contents.len() + data.len() > STORAGE_FILE_MAX => {
                    break Err(native_abi::ERR_OUT_OF_RANGE);
                }
                Ok(data) => contents.extend_from_slice(&data),
                Err(failure) => break Err(failure.into()),
            }
        };
        self.release(nodeid, handle);
        result.map(|()| Some(contents))
    }

    fn write_file(&self, directory: u64, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        // 临时文件名以 `.` 开头且带 `.tmp` 后缀，不会与合法的服务文件名冲突地被读取。
        let mut temporary = Vec::with_capacity(name.len() + 5);
        temporary.push(b'.');
        temporary.extend_from_slice(name);
        temporary.extend_from_slice(b".tmp");
        let created = self
            .call(
                CREATE,
                directory,
                &Body::new()
                    .u32(O_WRONLY | O_TRUNC)
                    .u32(0o100600)
                    .u32(0)
                    .u32(0)
                    .name(&temporary)
                    .0,
            )
            .map_err(Status::from)?;
        let nodeid = u64_at(&created, 0)?;
        let handle = u64_at(&created, ENTRY_OUT)?;
        let mut result = Ok(());
        for (index, chunk) in bytes.chunks(CHUNK).enumerate() {
            let request = Body::new()
                .u64(handle)
                .u64((index * CHUNK) as u64)
                .u32(chunk.len() as u32)
                .u32(0)
                .u64(0)
                .u32(0)
                .u32(0)
                .bytes(chunk);
            if let Err(failure) = self.call(WRITE, nodeid, &request.0) {
                result = Err(failure.into());
                break;
            }
        }
        if result.is_ok() {
            result = self
                .call(FSYNC, nodeid, &Body::new().u64(handle).u32(0).u32(0).0)
                .map(drop)
                .map_err(Status::from);
        }
        self.release(nodeid, handle);
        result?;
        self.call(
            RENAME,
            directory,
            &Body::new().u64(directory).name(&temporary).name(name).0,
        )
        .map(drop)
        .map_err(Status::from)?;
        // rename 之后同步目录，使替换在断电后同样可见。
        self.call(FSYNC, directory, &Body::new().u64(0).u32(0).u32(0).0)
            .map(drop)
            .map_err(Status::from)
    }
}

/// 一个服务的私有目录。
pub(crate) struct ServiceStorage {
    pub(crate) volume: Arc<DataVolume>,
    pub(crate) directory: u64,
}

impl StorageService for ServiceStorage {
    fn read(&self, name: &[u8]) -> Result<Option<Vec<u8>>, Status> {
        self.volume.read_file(self.directory, name)
    }

    fn write(&self, name: &[u8], bytes: &[u8]) -> Result<(), Status> {
        self.volume.write_file(self.directory, name, bytes)
    }
}
