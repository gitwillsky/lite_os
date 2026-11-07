use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

mod attributes;
mod identity;
mod ipc_description;
use identity::ProcIdentities;
mod link;
mod maps;
pub(crate) use maps::{ProcMapping, ProcMemoryMaps};
mod directory;
mod lookup;
mod lookup_cache;
mod mount;
mod networking;
mod node;
pub(crate) use networking::{ProcNetworkSnapshot, ProcRouteSnapshot};
mod opened_operations;
mod partitions;
mod process;
mod snapshot;
mod sysctl_limit;
mod sysctl_permissions;
mod system;
mod text;
mod time_offsets;
use lookup::{decimal_name, find_process, find_thread};
use node::{ProcMapFormat, ProcNode, ProcTaskDirectory, ProcThreadDirectory};
use partitions::format_partitions;
use process::{
    format_io, format_process_comm, format_process_stat, format_process_statm,
    format_process_status, format_thread_comm, format_thread_stat, format_thread_statm,
    format_thread_status,
};
pub(crate) use snapshot::{
    ProcCpuSnapshot, ProcCredentialSnapshot, ProcFileDescriptorSnapshot, ProcIoSnapshot,
    ProcProcessSnapshot, ProcSnapshot, ProcThreadSnapshot,
};
use system::{
    format_buddyinfo, format_cpu_stat, format_loadavg, format_meminfo, format_uptime, format_vmstat,
};
use text::{ProcText, proc_text};

use super::{
    DirectoryEntry, DirectoryRead, DirectoryVisitor, FileSystem, FileSystemError,
    FileSystemStatistics, IndexedDirectory, Inode, InodeMetadata, InodeType, OpenedFile, vfs,
};

/// 本次 proc 读取需要的投影；System 不收集每进程 VMA/comm/credentials。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcSnapshotScope {
    /// 全局 allocator、CPU、reclaim 与 scheduler 数量。
    System,
    /// 全局字段以及指定 stable TGID 的 Process/Thread 明细；不读取其他进程资源锁。
    Process(usize),
}

/// Linux proc main-Thread fs directory identity。
#[derive(Clone, Copy)]
pub(crate) enum ProcDirectory {
    Root,
    Cwd,
}

/// mounted PID view 精确解析出的实际 Process/Thread identity，不取得 MM、credentials 或 scheduler 明细。
pub(crate) struct ProcTaskIdentity {
    pub(crate) process: Arc<crate::namespace::PidIdentity>,
    pub(crate) thread: Arc<crate::namespace::PidIdentity>,
}

/// procfs 读取 kernel 状态的窄接口；状态仍由 task、memory 与 processor 唯一拥有。
pub(crate) trait ProcSource: Send + Sync {
    /// 上次启动留下的只读崩溃摘要；无记录时为 None，procfs 不访问诊断 owner 的实现。
    fn previous_panic(&self) -> Option<&'static [u8]>;

    /// stable TGID key 的实际 graph association，只返回 PID observer；目录打开不构造完整快照。
    fn process_identity(&self, key: usize) -> Option<Arc<crate::namespace::PidIdentity>>;
    /// proc root getdents 只枚举可见 TGID；不为目录读取构造全部 process resource snapshots。
    fn process_identities(
        &self,
        view: &crate::namespace::PidNamespace,
    ) -> Result<Vec<Arc<crate::namespace::PidIdentity>>, FileSystemError>;
    /// 精确可见编号在实际 PID index 和 graph 中解析；包含 zombie leader 与 live nonleader TID。
    fn task_identity(
        &self,
        view: &crate::namespace::PidNamespace,
        number: u32,
    ) -> Option<ProcTaskIdentity>;
    /// maps open 按 READ_FSCREDS 授权并绑定当时的 MM；观察引用不得阻止最后 user 退出。
    fn open_memory_maps(&self, tid: usize) -> Result<Arc<dyn ProcMemoryMaps>, FileSystemError>;
    /// `/proc/meminfo` 的映射分类（跨进程去重）；只在读取 meminfo 时扫描全部地址空间。
    fn mapped_memory(&self) -> Result<crate::memory::MappedMemory, FileSystemError>;
    /// 在一次读取边界取得自洽的只读快照。
    fn snapshot(
        &self,
        scope: ProcSnapshotScope,
        view: &crate::namespace::PidNamespace,
    ) -> Result<ProcSnapshot, FileSystemError>;
    /// mount 捕获此 owner；以后 setns/unshare 不重定向既有 proc filesystem。
    fn current_pid_namespace(&self) -> Arc<crate::namespace::PidNamespace>;
    /// IPC sysctl lookup pin calling Thread 的实际 owner；以后 setns 不重定向既有 fd。
    fn current_ipc_namespace(&self) -> Result<Arc<crate::ipc::IpcNamespace>, FileSystemError>;
    /// net sysctl lookup 固定调用者网络域；inode key/metadata 与已打开 fd 保持同一对象。
    fn current_net_namespace(&self) -> Result<Arc<crate::net::NetNamespace>, FileSystemError>;

    /// /proc/TID/net 每次 lookup/readdir 从实际 Thread 的 nsproxy 捕获 NET；已退休/kernel 无替代域。
    fn thread_net_namespace(&self, tid: usize) -> Option<Arc<crate::net::NetNamespace>>;
    /// 网络文件只读取 open 捕获的实际 NET 全接口快照；不收集全局 CPU/MM/Process 状态。
    fn network_snapshot(
        &self,
        target: &crate::net::NetNamespace,
    ) -> Result<Vec<ProcNetworkSnapshot>, FileSystemError>;

    /// 网络路由文件读取捕获NET的实际MAIN FIB，不从地址快照合成route。
    fn route_snapshot(
        &self,
        target: &crate::net::NetNamespace,
    ) -> Result<Vec<ProcRouteSnapshot>, FileSystemError>;

    /// user sysctl lookup 绑定实际 caller namespace；打开 fd 不随以后 setns/unshare 重定向。
    fn current_namespace_authority(
        &self,
    ) -> Result<crate::namespace::NamespaceAuthority, FileSystemError>;

    /// 返回正在解析 `/proc/self` 的 calling process TGID。
    ///
    /// # Returns
    ///
    /// user process context 返回 TGID；无 current task 返回 None。
    fn current_pid(&self, view: &crate::namespace::PidNamespace) -> Option<usize>;

    /// calling viewer 的 root；lower VFS 只接受显式 opened identity。
    fn current_root(&self) -> Option<Arc<OpenedFile>>;
    /// proc mounts open 同时 pin 实际 target topology 与 target root；普通 0444 mounts 不使用 ns magic-link 的 ptrace gate。
    fn thread_mount_snapshot(
        &self,
        tid: usize,
    ) -> Option<(Arc<super::MountNamespace>, Arc<OpenedFile>)>;
    /// 指定 Thread 的 root/cwd；main TGID 对应已退休 Thread 时不可由 sibling 替代。
    fn process_directory(
        &self,
        tid: usize,
        kind: ProcDirectory,
    ) -> Result<Option<Arc<OpenedFile>>, FileSystemError>;

    /// task_dump_owner 的唯一 scalar projection；不存在返回 None，FS 不反查 MM/dumpability。
    fn thread_inode_owner(
        &self,
        tid: usize,
        mode: u32,
    ) -> Result<Option<(u32, u32)>, FileSystemError>;

    /// uid_map/gid_map 的 target user namespace；不要求 ptrace，但 inode write/map policy 仍授权。
    fn thread_credentials(
        &self,
        tid: usize,
    ) -> Result<Option<ProcCredentialSnapshot>, FileSystemError>;
    /// ns magic link 的实际 Thread membership；必须通过与 proc cwd 相同的 FSCREDS 授权。
    fn thread_namespace(
        &self,
        tid: usize,
        kind: crate::namespace::NamespaceSelector,
    ) -> Result<Option<crate::namespace::NamespaceReference>, FileSystemError>;

    /// timens_offsets 每次 IO 重新捕获 target 的 for_children，不使用 magic-link 的 ptrace gate。
    fn thread_time_for_children(&self, tid: usize) -> Option<Arc<crate::namespace::TimeNamespace>>;
    /// 本次 proc IO 调用者的 clock projection，不冻结在 opener 的 f_cred 中。
    fn current_time_offsets(&self) -> Result<crate::namespace::TimeOffsets, FileSystemError>;

    /// firmware 交付的 kernel command line（不含结尾换行），`/proc/cmdline` 的内容来源。
    fn kernel_command_line(&self) -> &[u8];

    /// 按 actual stable TID 从该 Thread 的 MemorySet argument range 读取实时 argv bytes。
    ///
    /// # Parameters
    ///
    /// - `pid`: actual Thread key；已退休 leader 有空 cmdline，不借 sibling 的 MM。
    ///
    /// # Returns
    ///
    /// 存在且 argument range 可读时返回 NUL 分隔 bytes；不存在返回 None。
    ///
    /// # Errors
    ///
    /// kernel snapshot buffer OOM 返回明确文件系统错误。
    fn thread_arguments(&self, pid: usize) -> Result<Option<Vec<u8>>, FileSystemError>;

    /// 按实际 stable TID 投影该 Thread 的 fd/OFD；已退休 leader 保留空目录，不借 sibling。
    ///
    /// # Parameters
    ///
    /// - `pid`: mounted view 已解析的 stable Thread key。
    ///
    /// # Returns
    ///
    /// Thread 或 zombie leader 存在时返回按 fd 排序的 targets；实体已回收返回 None。
    fn thread_file_descriptors(
        &self,
        pid: usize,
    ) -> Result<Option<Vec<ProcFileDescriptorSnapshot>>, FileSystemError>;
    /// 单个实际 fd 的标准 fdinfo；同一 table transaction 捕获 OFD/CLOEXEC，锁外格式化。
    fn thread_file_information(
        &self,
        tid: usize,
        fd: usize,
        view: &crate::namespace::PidNamespace,
    ) -> Result<Option<Vec<u8>>, FileSystemError>;

    /// 按实际 stable TID 投影该 Thread 的 MM executable opened-entry identity。
    ///
    /// # Parameters
    ///
    /// - `pid`: mounted view 已解析的 stable Thread key。
    ///
    /// # Returns
    ///
    /// process 存在且访问允许时返回 executable opened entry；否则返回 None。
    ///
    /// # Errors
    ///
    /// 权限或内存失败返回明确文件系统错误。
    fn thread_executable(&self, pid: usize) -> Result<Option<Arc<OpenedFile>>, FileSystemError>;
}

struct ProcInode {
    // 原生 proc TGID base file operations 保留实际 PID；目录 fd 不被编号重用或标签重定向。
    process_identity: Option<Arc<crate::namespace::PidIdentity>>,
    number: u64,
    key: (ProcNode, u64),
    filesystem_id: usize,
    source: Arc<dyn ProcSource>,
    node: ProcNode,
    pid_namespace: Arc<crate::namespace::PidNamespace>,
    sysctl_pid_namespace: Option<Weak<crate::namespace::PidNamespace>>,
    sysctl_ipc_namespace: Option<Weak<crate::ipc::IpcNamespace>>,
    net_namespace: Option<Weak<crate::net::NetNamespace>>,
    user_namespace: Option<Weak<crate::namespace::UserNamespace>>,
    identities: Arc<ProcIdentities<Self>>,
    attributes: attributes::ProcAttributes,
    /// 只保留向下的实际 child inode，不缓存 . / ..，因此不形成 inode/identity 循环。
    lookup_cache: Option<lookup_cache::ProcLookupCache>,
}

impl ProcInode {
    fn new(
        filesystem_id: usize,
        source: Arc<dyn ProcSource>,
        node: ProcNode,
        pid_namespace: Arc<crate::namespace::PidNamespace>,
        namespace: Option<Arc<crate::namespace::UserNamespace>>,
        network: Option<Arc<crate::net::NetNamespace>>,
        identities: Arc<ProcIdentities<Self>>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let user_namespace = if matches!(
            node,
            ProcNode::UserSysctlDir | ProcNode::UserResourceLimit(_)
        ) {
            Some(match namespace {
                Some(namespace) => namespace,
                None => source.current_namespace_authority()?.namespace,
            })
        } else {
            None
        };
        let sysctl_pid_namespace = if matches!(node, ProcNode::KernelSysctlDir | ProcNode::PidLimit)
        {
            Some(source.current_pid_namespace())
        } else {
            None
        };
        let sysctl_ipc_namespace = if matches!(node, ProcNode::IpcLimit(_)) {
            Some(source.current_ipc_namespace()?)
        } else {
            None
        };
        let net_namespace = if matches!(node, ProcNode::NetDev | ProcNode::NetRoute) {
            Some(network.ok_or(FileSystemError::NotFound)?)
        } else if matches!(
            node,
            ProcNode::NetSysctlDir
                | ProcNode::CoreNetSysctlDir
                | ProcNode::CoreBufferLimit(_)
                | ProcNode::OptmemLimit
        ) {
            Some(source.current_net_namespace()?)
        } else {
            None
        };
        let owner = Self::task_owner(&*source, node)?;
        if node.owner_tid().is_some() && owner.is_none() {
            return Err(FileSystemError::NotFound);
        }
        let scope = sysctl_pid_namespace.as_ref().map_or_else(
            || {
                user_namespace
                    .as_ref()
                    .map_or(0, |namespace| namespace.id())
            },
            |namespace| namespace.id(),
        );
        let scope = sysctl_ipc_namespace.as_ref().map_or(scope, |namespace| {
            crate::namespace::NamespaceOwner::id(namespace.as_ref())
        });
        let scope = net_namespace
            .as_ref()
            .map_or(scope, |namespace| namespace.id());
        let key = (node, scope);
        if let Some(selected) = identities.live.get(key) {
            if let Some((uid, gid)) = owner {
                selected.attributes.refresh_owner(uid, gid);
            }
            return Ok(selected);
        }
        let number = identities.allocate_number()?;
        let process_identity = if let ProcNode::ProcessDir(key) = node {
            Some(
                source
                    .process_identity(key)
                    .ok_or(FileSystemError::NotFound)?,
            )
        } else {
            None
        };
        let mut initial = Self::initial_metadata(filesystem_id, node, number);
        if let Some(namespace) = &sysctl_pid_namespace {
            let user = namespace.user_namespace();
            initial.uid = user.to_kernel(true, 0).unwrap_or(0);
            initial.gid = user.to_kernel(false, 0).unwrap_or(0);
        }
        if let Some(namespace) = &sysctl_ipc_namespace {
            let user = namespace.user_namespace();
            initial.uid = user.to_kernel(true, 0).unwrap_or(0);
            initial.gid = user.to_kernel(false, 0).unwrap_or(0);
        }
        if let Some(namespace) = &net_namespace {
            let user = namespace.user_namespace();
            initial.uid = user.to_kernel(true, 0).unwrap_or(0);
            initial.gid = user.to_kernel(false, 0).unwrap_or(0);
            // Linux netns_core_table 非initial域的global buffer项只读，属性和准入必须来自同一scope。
            if matches!(node, ProcNode::CoreBufferLimit(_)) && !namespace.is_initial() {
                initial.mode &= !0o222;
            }
        }
        let candidate = Arc::try_new(Self {
            process_identity,
            number,
            key,
            filesystem_id,
            source,
            node,
            pid_namespace,
            sysctl_pid_namespace: sysctl_pid_namespace.as_ref().map(Arc::downgrade),
            sysctl_ipc_namespace: sysctl_ipc_namespace.as_ref().map(Arc::downgrade),
            net_namespace: net_namespace.as_ref().map(Arc::downgrade),
            user_namespace: user_namespace.as_ref().map(Arc::downgrade),
            identities: identities.clone(),
            attributes: attributes::ProcAttributes::new(node, initial)?,
            lookup_cache: (node.kind() == InodeType::Directory)
                .then(lookup_cache::ProcLookupCache::new),
        })
        .map_err(|_| FileSystemError::OutOfMemory)?;
        let selected = identities.live.admit(key, &candidate)?;
        if let Some((uid, gid)) = owner {
            selected.attributes.refresh_owner(uid, gid);
        }
        Ok(selected)
    }

    fn shared(&self) -> Arc<Self> {
        self.identities
            .live
            .get(self.key)
            .expect("borrowed live proc inode identity")
    }
    fn task_owner(
        source: &dyn ProcSource,
        node: ProcNode,
    ) -> Result<Option<(u32, u32)>, FileSystemError> {
        let Some(tid) = node.owner_tid() else {
            return Ok(None);
        };
        source.thread_inode_owner(tid, node.metadata_mode())
    }
    fn attribute_metadata(&self) -> InodeMetadata {
        self.attributes.snapshot(Self::initial_metadata(
            self.filesystem_id,
            self.node,
            self.inode_number(),
        ))
    }
    fn initial_metadata(filesystem_id: usize, node: ProcNode, number: u64) -> InodeMetadata {
        let kind = node.kind();
        InodeMetadata {
            filesystem: filesystem_id as u64,
            inode: number,
            kind,
            mode: node.metadata_mode(),
            links: if kind == InodeType::Directory { 2 } else { 1 },
            uid: 0,
            gid: 0,
            size: node.metadata_size(),
            blocks: 0,
            block_size: 4096,
            atime: 0,
            mtime: 0,
            ctime: 0,
            fine_times: Default::default(),
            device: None,
        }
    }

    fn inode_number(&self) -> u64 {
        self.number
    }

    /// lookup/readdir 共享实际 inode admission；getdents 的 d_ino 不再编码 task key。
    fn child_inode(
        &self,
        node: ProcNode,
        descendant: bool,
        network: Option<Arc<crate::net::NetNamespace>>,
    ) -> Result<Arc<Self>, FileSystemError> {
        let child = Self::new(
            self.filesystem_id,
            self.source.clone(),
            node,
            self.pid_namespace.clone(),
            match &self.user_namespace {
                Some(namespace) => Some(namespace.upgrade().ok_or(FileSystemError::NotFound)?),
                None => None,
            },
            network,
            self.identities.clone(),
        )?;
        if descendant {
            // sysctl inode 仅持 Weak namespace；只有实际 open backend 才 pin target，不滞留 quota。
            self.lookup_cache
                .as_ref()
                .expect("directory child lookup")
                .remember(&child)?;
        }
        Ok(child)
    }

    fn file_contents(
        &self,
        viewer: &crate::namespace::UserNamespace,
    ) -> Result<Vec<u8>, FileSystemError> {
        if matches!(self.node, ProcNode::Partitions) {
            return format_partitions();
        }
        if matches!(self.node, ProcNode::LastPanic) {
            // 上次启动正常结束时文件为空，与 Linux pstore 无记录时的表现一致。
            let dump = self.source.previous_panic().unwrap_or_default();
            let mut contents = Vec::new();
            contents
                .try_reserve_exact(dump.len())
                .map_err(|_| FileSystemError::OutOfMemory)?;
            contents.extend_from_slice(dump);
            return Ok(contents);
        }
        if matches!(self.node, ProcNode::Cmdline) {
            let line = self.source.kernel_command_line();
            let mut contents = Vec::new();
            contents
                .try_reserve_exact(line.len() + 1)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            contents.extend_from_slice(line);
            contents.push(b'\n');
            return Ok(contents);
        }
        if let ProcNode::ProcessCmdline(tid) | ProcNode::ThreadCmdline(_, tid) = self.node {
            return self
                .source
                .thread_arguments(tid)?
                .ok_or(FileSystemError::NotFound);
        }
        if let ProcNode::TaskFdInfo(tid, fd) = self.node {
            return self
                .source
                .thread_file_information(tid, fd, &self.pid_namespace)?
                .ok_or(FileSystemError::NotFound);
        }
        let scope = match self.node {
            ProcNode::Stat
            | ProcNode::MemInfo
            | ProcNode::BuddyInfo
            | ProcNode::VmStat
            | ProcNode::LoadAvg
            | ProcNode::Uptime => ProcSnapshotScope::System,
            _ => self
                .node
                .process_key()
                .map_or(ProcSnapshotScope::System, ProcSnapshotScope::Process),
        };
        let snapshot = self.source.snapshot(scope, &self.pid_namespace)?;
        match self.node {
            ProcNode::Stat => format_cpu_stat(&snapshot),
            ProcNode::MemInfo => format_meminfo(&snapshot, &self.source.mapped_memory()?),
            ProcNode::BuddyInfo => format_buddyinfo(&snapshot),
            ProcNode::VmStat => format_vmstat(&snapshot),
            ProcNode::LoadAvg => format_loadavg(&snapshot),
            ProcNode::Uptime => format_uptime(&snapshot, self.source.current_time_offsets()?),

            ProcNode::Mounts | ProcNode::Partitions | ProcNode::Cmdline | ProcNode::LastPanic => {
                unreachable!(
                    "mount table, partitions and command line handled before task snapshot"
                )
            }
            ProcNode::ProcessStat(pid) => format_process_stat(
                find_process(&snapshot, pid)?,
                self.source.current_time_offsets()?,
                &self.pid_namespace,
            ),
            ProcNode::ProcessStatus(pid) => {
                format_process_status(find_process(&snapshot, pid)?, viewer, &self.pid_namespace)
            }
            ProcNode::ProcessComm(pid) => format_process_comm(find_process(&snapshot, pid)?),
            ProcNode::ProcessStatm(pid) => format_process_statm(find_process(&snapshot, pid)?),
            ProcNode::ProcessIo(pid) => format_io(&find_process(&snapshot, pid)?.io),
            ProcNode::ThreadStat(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_stat(
                    process,
                    find_thread(process, tid)?,
                    self.source.current_time_offsets()?,
                    &self.pid_namespace,
                )
            }
            ProcNode::ThreadStatus(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_status(
                    process,
                    find_thread(process, tid)?,
                    viewer,
                    &self.pid_namespace,
                )
            }
            ProcNode::ThreadComm(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_comm(find_thread(process, tid)?)
            }
            ProcNode::ThreadStatm(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_thread_statm(process, find_thread(process, tid)?)
            }
            ProcNode::ThreadIo(tgid, tid) => {
                let process = find_process(&snapshot, tgid)?;
                format_io(&find_thread(process, tid)?.io)
            }
            ProcNode::TimeOffsets(_)
            | ProcNode::UserMap(_, _)
            | ProcNode::UserResourceLimit(_)
            | ProcNode::MountLimit
            | ProcNode::InotifyLimit(_)
            | ProcNode::PidLimit
            | ProcNode::IpcLimit(_)
            | ProcNode::CoreBufferLimit(_)
            | ProcNode::OptmemLimit
            | ProcNode::IpcTable(_)
            | ProcNode::NetDev
            | ProcNode::NetRoute => Err(FileSystemError::InvalidOperation),
            ProcNode::Root
            | ProcNode::SysDir
            | ProcNode::NetSysctlDir
            | ProcNode::CoreNetSysctlDir
            | ProcNode::SysvipcDir
            | ProcNode::FsSysctlDir
            | ProcNode::InotifySysctlDir
            | ProcNode::KernelSysctlDir
            | ProcNode::UserSysctlDir
            | ProcNode::NamespaceDir(_, _, _)
            | ProcNode::NamespaceLink(_, _)
            | ProcNode::TaskNetDir(_, _, _)
            | ProcNode::NetLink
            | ProcNode::ProcessMounts(_)
            | ProcNode::ProcessMountInfo(_)
            | ProcNode::ProcessMountStats(_)
            | ProcNode::ThreadMounts(_)
            | ProcNode::ThreadMountInfo(_)
            | ProcNode::ThreadMountStats(_)
            | ProcNode::SelfLink
            | ProcNode::ProcessDir(_)
            | ProcNode::ProcessTaskDir(_)
            | ProcNode::TaskFdDir(_, _, _)
            | ProcNode::TaskFdInfoDir(_, _, _)
            | ProcNode::ThreadDir(_, _, _)
            | ProcNode::ProcessRoot(_)
            | ProcNode::ProcessCwd(_)
            | ProcNode::ThreadRoot(_)
            | ProcNode::ThreadCwd(_)
            | ProcNode::TaskExe(_)
            | ProcNode::TaskFd(_, _) => Err(FileSystemError::IsDirectory),
            ProcNode::ProcessCmdline(_) | ProcNode::ThreadCmdline(_, _) => {
                unreachable!("cmdline handled as binary data")
            }
            ProcNode::ProcessMaps(_, _) | ProcNode::ThreadMaps(_, _, _) => {
                unreachable!("maps uses per-open MM observation")
            }
            ProcNode::TaskFdInfo(_, _) => unreachable!("fdinfo handled without a task snapshot"),
        }
    }
}

impl Inode for ProcInode {
    fn process_directory_identity(&self) -> Option<&Arc<crate::namespace::PidIdentity>> {
        self.process_identity.as_ref()
    }

    fn filesystem_id(&self) -> usize {
        self.filesystem_id
    }

    fn metadata(&self) -> Result<InodeMetadata, FileSystemError> {
        // pid_getattr 投影 live Task 的 owner；授权仍使用 inode 的唯一 live attribute state。
        let owner = Self::task_owner(&*self.source, self.node)?;
        let mut metadata = self.attribute_metadata();
        if self.node.owner_tid().is_some() {
            // pid_getattr 在 Task 退休后仍返回 live inode 属性，UID/GID 投影为 global root。
            let (uid, gid) = owner.unwrap_or((0, 0));
            metadata.uid = uid;
            metadata.gid = gid;
        }
        Ok(metadata)
    }

    fn require_access(
        &self,
        identity: &super::AccessIdentity,
        requested: u8,
    ) -> Result<(), FileSystemError> {
        if matches!(
            self.node,
            ProcNode::SysDir
                | ProcNode::NetSysctlDir
                | ProcNode::CoreNetSysctlDir
                | ProcNode::UserSysctlDir
                | ProcNode::FsSysctlDir
                | ProcNode::InotifySysctlDir
                | ProcNode::KernelSysctlDir
        ) {
            return if requested & 2 != 0 {
                Err(FileSystemError::AccessDenied)
            } else {
                Ok(())
            };
        }
        if matches!(self.node, ProcNode::PidLimit) {
            let target = self
                .sysctl_pid_namespace
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(FileSystemError::NotFound)?;
            let user = target.user_namespace();
            if requested & 1 != 0
                || requested & 2 != 0
                    && user.to_kernel(true, 0) != Some(identity.effective_uid())
                    && !identity
                        .namespace_capable(&user, linux_abi::capability::Capability::SYS_ADMIN)
            {
                return Err(FileSystemError::AccessDenied);
            }
            return Ok(());
        }
        if matches!(
            self.node,
            ProcNode::CoreBufferLimit(_) | ProcNode::OptmemLimit
        ) {
            let target = self
                .net_namespace
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(FileSystemError::NotFound)?;
            if requested & 1 != 0
                || requested & 2 != 0
                    && (matches!(self.node, ProcNode::CoreBufferLimit(_)) && !target.is_initial()
                        || identity.effective_uid() != 0
                            && !identity.namespace_capable(
                                &target.user_namespace(),
                                linux_abi::capability::Capability::NET_ADMIN,
                            ))
            {
                return Err(FileSystemError::AccessDenied);
            }
            return Ok(());
        }
        if matches!(self.node, ProcNode::IpcLimit(_)) {
            let target = self
                .sysctl_ipc_namespace
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(FileSystemError::NotFound)?;
            if requested & 1 != 0
                || requested & 2 != 0
                    && identity.effective_uid()
                        != target.user_namespace().to_kernel(true, 0).unwrap_or(0)
            {
                return Err(FileSystemError::AccessDenied);
            }
            return Ok(());
        }
        if matches!(self.node, ProcNode::MountLimit | ProcNode::InotifyLimit(_)) {
            return sysctl_permissions::require(identity.effective_uid(), requested);
        }
        if matches!(self.node, ProcNode::UserResourceLimit(_)) {
            let target = self
                .user_namespace
                .as_ref()
                .and_then(Weak::upgrade)
                .ok_or(FileSystemError::NotFound)?;
            if requested & 1 != 0
                || requested & 2 != 0
                    && !identity
                        .namespace_capable(&target, linux_abi::capability::Capability::SYS_RESOURCE)
            {
                return Err(FileSystemError::AccessDenied);
            }
            Ok(())
        } else {
            identity.require(self.attribute_metadata(), requested)
        }
    }
    fn inode_type(&self) -> InodeType {
        self.node.kind()
    }
    fn size(&self) -> u64 {
        self.node.metadata_size()
    }
    fn is_executable(&self) -> bool {
        false
    }

    fn data_backing(&self) -> super::inode::DataBacking {
        super::inode::DataBacking::Snapshot
    }

    fn open_backend(
        &self,
        flags: u32,
        _: &super::AccessIdentity,
        authority: &crate::namespace::NamespaceAuthority,
    ) -> Result<super::InodeOpen, super::InodeOpenError> {
        if self.inode_type() != InodeType::File {
            return Ok(super::InodeOpen::Storage(None));
        }
        opened_operations::open(self, flags, authority)
            .map(super::InodeOpen::Device)
            .map_err(Into::into)
    }

    // proc 只允许经打开时绑定的操作读取；不能在 storage fallback 补取当前 reader 的凭据。
    fn read_storage(&self, _: u64, _: &mut [u8]) -> Result<usize, FileSystemError> {
        Err(FileSystemError::InvalidOperation)
    }

    fn read_link(&self) -> Result<Vec<u8>, FileSystemError> {
        self.readable_link()
    }

    fn link_target(&self) -> Result<super::LinkTarget, FileSystemError> {
        self.resolved_link()
    }

    fn write_storage(
        &self,
        _offset: u64,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<usize, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn append_storage(
        &self,
        _buf: &[u8],
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(u64, usize), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn truncate_storage(
        &self,
        _size: u64,
        _identity: Option<&crate::fs::AccessIdentity>,
    ) -> Result<(), FileSystemError> {
        self.attributes.truncate();
        Ok(())
    }
    fn touch_access(&self, now: u64, relatime: bool) -> Result<(), FileSystemError> {
        self.attributes.touch_access(now, relatime);
        Ok(())
    }
    fn set_times(&self, atime: Option<u64>, mtime: Option<u64>) -> Result<(), FileSystemError> {
        self.attributes.set_times(atime, mtime);
        Ok(())
    }
    fn change_owner_mode(&self, change: super::OwnerModeChange) -> Result<(), FileSystemError> {
        self.attributes.change(self.node, &change)
    }
    fn sync_storage(&self) -> Result<(), FileSystemError> {
        Ok(())
    }

    fn read_directory(
        &self,
        cursor: u64,
        visitor: &mut dyn DirectoryVisitor,
    ) -> Result<DirectoryRead, FileSystemError> {
        self.read_entries(cursor, visitor)
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        self.lookup_child(name)
    }

    fn create(
        &self,
        _name: &[u8],
        _kind: InodeType,
        _metadata: super::CreateMetadata,
    ) -> Result<Arc<dyn Inode>, FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn unlink(&self, _name: &[u8], _remove_directory: bool) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
    fn rename(
        &self,
        _old_name: &[u8],
        _new_parent_inode: u64,
        _new_name: &[u8],
        _no_replace: bool,
    ) -> Result<(), FileSystemError> {
        Err(FileSystemError::ReadOnly)
    }
}

impl Drop for ProcInode {
    fn drop(&mut self) {
        self.identities.live.retire(self.key, self);
    }
}

pub(crate) struct ProcFileSystem {
    root: Arc<ProcInode>,
}

/// procfs 类型：不接受选项（`hidepid=`/`gid=` 尚未支持）。
pub(super) struct ProcFileSystemType;
