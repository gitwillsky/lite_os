use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

mod attributes;
mod identity;
mod ipc_description;
use identity::ProcIdentities;
mod link;
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
use lookup::{decimal_name, find_process, find_thread, parse_pid};
use node::{ProcNode, ProcTaskDirectory, ProcThreadDirectory};
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
    fn current_net_namespace(&self) -> Result<Arc<crate::socket::NetNamespace>, FileSystemError>;

    /// /proc/TID/net 每次 lookup/readdir 从实际 Thread 的 nsproxy 捕获 NET；已退休/kernel 无替代域。
    fn thread_net_namespace(&self, tid: usize) -> Option<Arc<crate::socket::NetNamespace>>;
    /// 网络文件只读取 open 捕获的实际 NET 全接口快照；不收集全局 CPU/MM/Process 状态。
    fn network_snapshot(
        &self,
        target: &crate::socket::NetNamespace,
    ) -> Result<Vec<ProcNetworkSnapshot>, FileSystemError>;

    /// 网络路由文件读取捕获NET的实际MAIN FIB，不从地址快照合成route。
    fn route_snapshot(
        &self,
        target: &crate::socket::NetNamespace,
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
    net_namespace: Option<Weak<crate::socket::NetNamespace>>,
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
        network: Option<Arc<crate::socket::NetNamespace>>,
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
        network: Option<Arc<crate::socket::NetNamespace>>,
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
        if matches!(self.node, ProcNode::Swaps) {
            let mut output = ProcText::new();
            crate::memory::swap::write_snapshot(&mut output)
                .map_err(|_| FileSystemError::OutOfMemory)?;
            return Ok(output.finish());
        }
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
            ProcNode::MemInfo => format_meminfo(&snapshot),
            ProcNode::BuddyInfo => format_buddyinfo(&snapshot),
            ProcNode::VmStat => format_vmstat(&snapshot),
            ProcNode::LoadAvg => format_loadavg(&snapshot),
            ProcNode::Uptime => format_uptime(&snapshot, self.source.current_time_offsets()?),

            ProcNode::Mounts
            | ProcNode::Partitions
            | ProcNode::Cmdline
            | ProcNode::LastPanic
            | ProcNode::Swaps => {
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
                        .namespace_capable(&user, syscall_abi::capability::Capability::SYS_ADMIN)
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
                                syscall_abi::capability::Capability::NET_ADMIN,
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
                    && !identity.namespace_capable(
                        &target,
                        syscall_abi::capability::Capability::SYS_RESOURCE,
                    )
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
        let parent_node = match self.node {
            ProcNode::ProcessTaskDir(pid) => ProcNode::ProcessDir(pid),
            ProcNode::SysDir => ProcNode::Root,
            ProcNode::UserSysctlDir
            | ProcNode::FsSysctlDir
            | ProcNode::KernelSysctlDir
            | ProcNode::NetSysctlDir => ProcNode::SysDir,
            ProcNode::CoreNetSysctlDir => ProcNode::NetSysctlDir,
            ProcNode::ThreadDir(tgid, _, directory) => directory.parent(tgid),
            ProcNode::TaskFdDir(tgid, tid, directory)
            | ProcNode::TaskFdInfoDir(tgid, tid, directory)
            | ProcNode::NamespaceDir(tgid, tid, directory)
            | ProcNode::TaskNetDir(tgid, tid, directory) => directory.parent(tgid, tid),
            _ => ProcNode::Root,
        };
        // 原生 proc_net_readdir 在同一次目录读取固定 task 的实际 NET；nsproxy 消失返回 EINVAL。
        let network = if let ProcNode::TaskNetDir(_, tid, _) = self.node {
            Some(
                self.source
                    .thread_net_namespace(tid)
                    .ok_or(FileSystemError::InvalidOperation)?,
            )
        } else {
            None
        };
        let mut stream = IndexedDirectory::new(cursor, visitor);
        let mut index = 0usize;
        macro_rules! emit {
            ($node:expr, $kind:expr, $name:expr) => {{
                let entry_index = index;
                index += 1;
                if entry_index >= stream.start_index() {
                    let name: &[u8] = $name;
                    let inode =
                        self.child_inode($node, name != b"." && name != b"..", network.clone())?;
                    if !stream.emit(
                        entry_index,
                        DirectoryEntry {
                            inode: inode.inode_number(),
                            kind: $kind,
                            name,
                        },
                    )? {
                        return Ok(stream.finish());
                    }
                }
            }};
        }
        emit!(self.node, InodeType::Directory, b".");
        emit!(parent_node, InodeType::Directory, b"..");
        match self.node {
            ProcNode::Root => {
                for (node, kind, name) in [
                    (ProcNode::Stat, InodeType::File, &b"stat"[..]),
                    (ProcNode::MemInfo, InodeType::File, &b"meminfo"[..]),
                    (ProcNode::BuddyInfo, InodeType::File, &b"buddyinfo"[..]),
                    (ProcNode::VmStat, InodeType::File, &b"vmstat"[..]),
                    (ProcNode::LoadAvg, InodeType::File, &b"loadavg"[..]),
                    (ProcNode::Uptime, InodeType::File, &b"uptime"[..]),
                    (ProcNode::Mounts, InodeType::SymLink, &b"mounts"[..]),
                    (ProcNode::Partitions, InodeType::File, &b"partitions"[..]),
                    (ProcNode::Swaps, InodeType::File, &b"swaps"[..]),
                    (ProcNode::Cmdline, InodeType::File, &b"cmdline"[..]),
                    (ProcNode::LastPanic, InodeType::File, &b"last_panic"[..]),
                    (ProcNode::NetLink, InodeType::SymLink, &b"net"[..]),
                    (ProcNode::SysDir, InodeType::Directory, &b"sys"[..]),
                    (ProcNode::SysvipcDir, InodeType::Directory, &b"sysvipc"[..]),
                    (ProcNode::SelfLink, InodeType::SymLink, &b"self"[..]),
                ] {
                    emit!(node, kind, name);
                }
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for process in self
                    .source
                    .process_identities(&self.pid_namespace)?
                    .into_iter()
                    .skip(start)
                {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ProcessDir(process.key()),
                        InodeType::Directory,
                        decimal_name(process.number_in(&self.pid_namespace) as usize, &mut name)
                    );
                }
            }
            ProcNode::ProcessDir(pid) => {
                emit!(
                    ProcNode::TimeOffsets(pid),
                    InodeType::File,
                    b"timens_offsets"
                );
                for (node, kind, name) in [
                    (
                        ProcNode::NamespaceDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"ns"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Uid),
                        InodeType::File,
                        &b"uid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Gid),
                        InodeType::File,
                        &b"gid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Setgroups),
                        InodeType::File,
                        &b"setgroups"[..],
                    ),
                    (
                        ProcNode::TaskNetDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"net"[..],
                    ),
                    (ProcNode::ProcessStat(pid), InodeType::File, &b"stat"[..]),
                    (
                        ProcNode::ProcessStatus(pid),
                        InodeType::File,
                        &b"status"[..],
                    ),
                    (
                        ProcNode::ProcessCmdline(pid),
                        InodeType::File,
                        &b"cmdline"[..],
                    ),
                    (ProcNode::ProcessComm(pid), InodeType::File, &b"comm"[..]),
                    (ProcNode::ProcessStatm(pid), InodeType::File, &b"statm"[..]),
                    (ProcNode::ProcessIo(pid), InodeType::File, &b"io"[..]),
                    (
                        ProcNode::ProcessMounts(pid),
                        InodeType::File,
                        &b"mounts"[..],
                    ),
                    (
                        ProcNode::ProcessMountInfo(pid),
                        InodeType::File,
                        &b"mountinfo"[..],
                    ),
                    (
                        ProcNode::ProcessMountStats(pid),
                        InodeType::File,
                        &b"mountstats"[..],
                    ),
                    (ProcNode::TaskExe(pid), InodeType::SymLink, &b"exe"[..]),
                    (ProcNode::ProcessRoot(pid), InodeType::SymLink, &b"root"[..]),
                    (ProcNode::ProcessCwd(pid), InodeType::SymLink, &b"cwd"[..]),
                    (
                        ProcNode::ProcessTaskDir(pid),
                        InodeType::Directory,
                        &b"task"[..],
                    ),
                    (
                        ProcNode::TaskFdDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"fd"[..],
                    ),
                    (
                        ProcNode::TaskFdInfoDir(pid, pid, ProcTaskDirectory::Process),
                        InodeType::Directory,
                        &b"fdinfo"[..],
                    ),
                ] {
                    emit!(node, kind, name);
                }
            }
            ProcNode::NamespaceDir(_, tid, _) => {
                for kind in crate::namespace::NamespaceSelector::ALL {
                    let node = ProcNode::NamespaceLink(tid, kind);
                    emit!(node, InodeType::SymLink, kind.name());
                }
            }
            ProcNode::ProcessTaskDir(pid) => {
                let snapshot = self
                    .source
                    .snapshot(ProcSnapshotScope::Process(pid), &self.pid_namespace)?;
                let process = find_process(&snapshot, pid)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for thread in process.threads.iter().skip(start) {
                    let mut name = [0u8; 20];
                    emit!(
                        ProcNode::ThreadDir(pid, thread.tid.key(), ProcThreadDirectory::Group),
                        InodeType::Directory,
                        decimal_name(
                            thread.tid.number_in(&self.pid_namespace) as usize,
                            &mut name
                        )
                    );
                }
            }
            ProcNode::ThreadDir(tgid, tid, directory) => {
                emit!(
                    ProcNode::TimeOffsets(tid),
                    InodeType::File,
                    b"timens_offsets"
                );
                let snapshot = self
                    .source
                    .snapshot(ProcSnapshotScope::Process(tgid), &self.pid_namespace)?;
                let process = find_process(&snapshot, tgid)?;
                let _ = find_thread(process, tid)?;
                for (node, name) in [
                    (
                        ProcNode::NamespaceDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"ns"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Uid),
                        &b"uid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Gid),
                        &b"gid_map"[..],
                    ),
                    (
                        ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Setgroups),
                        &b"setgroups"[..],
                    ),
                    (
                        ProcNode::TaskNetDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"net"[..],
                    ),
                    (ProcNode::ThreadStat(tgid, tid), &b"stat"[..]),
                    (ProcNode::ThreadStatus(tgid, tid), &b"status"[..]),
                    (ProcNode::ThreadCmdline(tgid, tid), &b"cmdline"[..]),
                    (ProcNode::ThreadComm(tgid, tid), &b"comm"[..]),
                    (ProcNode::ThreadStatm(tgid, tid), &b"statm"[..]),
                    (ProcNode::ThreadIo(tgid, tid), &b"io"[..]),
                    (ProcNode::ThreadMounts(tid), &b"mounts"[..]),
                    (ProcNode::ThreadMountInfo(tid), &b"mountinfo"[..]),
                    (ProcNode::ThreadMountStats(tid), &b"mountstats"[..]),
                    (ProcNode::ThreadRoot(tid), &b"root"[..]),
                    (ProcNode::ThreadCwd(tid), &b"cwd"[..]),
                    (ProcNode::TaskExe(tid), &b"exe"[..]),
                    (
                        ProcNode::TaskFdDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"fd"[..],
                    ),
                    (
                        ProcNode::TaskFdInfoDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                        &b"fdinfo"[..],
                    ),
                ] {
                    emit!(node, node.kind(), name);
                }
            }
            ProcNode::TaskFdDir(_, pid, _) | ProcNode::TaskFdInfoDir(_, pid, _) => {
                let descriptors = self
                    .source
                    .thread_file_descriptors(pid)?
                    .ok_or(FileSystemError::NotFound)?;
                let start = stream.start_index().saturating_sub(index);
                index += start;
                for entry in descriptors.into_iter().skip(start) {
                    let mut name = [0u8; 20];
                    let node = if matches!(self.node, ProcNode::TaskFdInfoDir(..)) {
                        ProcNode::TaskFdInfo(pid, entry.fd)
                    } else {
                        ProcNode::TaskFd(pid, entry.fd)
                    };
                    emit!(node, node.kind(), decimal_name(entry.fd, &mut name));
                }
            }
            ProcNode::SysDir => {
                emit!(ProcNode::KernelSysctlDir, InodeType::Directory, b"kernel");
                emit!(ProcNode::FsSysctlDir, InodeType::Directory, b"fs");
                emit!(ProcNode::UserSysctlDir, InodeType::Directory, b"user");
                emit!(ProcNode::NetSysctlDir, InodeType::Directory, b"net");
            }
            ProcNode::NetSysctlDir => {
                emit!(ProcNode::CoreNetSysctlDir, InodeType::Directory, b"core");
            }
            ProcNode::CoreNetSysctlDir => {
                emit!(ProcNode::OptmemLimit, InodeType::File, b"optmem_max");
                for resource in crate::socket::CoreBufferLimit::ALL {
                    emit!(
                        ProcNode::CoreBufferLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::KernelSysctlDir => {
                emit!(ProcNode::PidLimit, InodeType::File, b"pid_max");
                for resource in crate::ipc::IpcLimit::ALL {
                    emit!(
                        ProcNode::IpcLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::FsSysctlDir => {
                emit!(ProcNode::MountLimit, InodeType::File, b"mount-max");
                emit!(ProcNode::InotifySysctlDir, InodeType::Directory, b"inotify");
            }
            ProcNode::InotifySysctlDir => {
                for resource in crate::fs::InotifyLimit::ALL {
                    emit!(
                        ProcNode::InotifyLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::UserSysctlDir => {
                for resource in [
                    crate::namespace::UserResource::User,
                    crate::namespace::UserResource::Uts,
                    crate::namespace::UserResource::Mount,
                    crate::namespace::UserResource::Time,
                    crate::namespace::UserResource::Pid,
                    crate::namespace::UserResource::Ipc,
                    crate::namespace::UserResource::Net,
                    crate::namespace::UserResource::InotifyInstances,
                    crate::namespace::UserResource::InotifyWatches,
                ] {
                    emit!(
                        ProcNode::UserResourceLimit(resource),
                        InodeType::File,
                        resource.name()
                    );
                }
            }
            ProcNode::SysvipcDir => {
                for table in crate::ipc::IpcTable::ALL {
                    emit!(ProcNode::IpcTable(table), InodeType::File, table.name());
                }
            }
            ProcNode::TaskNetDir(_, _, _) => {
                emit!(ProcNode::NetDev, InodeType::File, b"dev");
                emit!(ProcNode::NetRoute, InodeType::File, b"route");
            }
            _ => return Err(FileSystemError::NotDirectory),
        }
        let _ = index;
        Ok(stream.finish())
    }

    fn find_child(&self, name: &[u8]) -> Result<Arc<dyn Inode>, FileSystemError> {
        let mut network = None;
        let node = match self.node {
            ProcNode::Root => match name {
                b"." | b".." => ProcNode::Root,
                b"stat" => ProcNode::Stat,
                b"meminfo" => ProcNode::MemInfo,
                b"swaps" => ProcNode::Swaps,
                b"buddyinfo" => ProcNode::BuddyInfo,
                b"vmstat" => ProcNode::VmStat,
                b"loadavg" => ProcNode::LoadAvg,
                b"uptime" => ProcNode::Uptime,
                b"mounts" => ProcNode::Mounts,
                b"partitions" => ProcNode::Partitions,
                b"cmdline" => ProcNode::Cmdline,
                b"last_panic" => ProcNode::LastPanic,
                b"net" => ProcNode::NetLink,
                b"self" => ProcNode::SelfLink,
                b"sys" => ProcNode::SysDir,
                b"sysvipc" => ProcNode::SysvipcDir,
                _ => {
                    let pid = parse_pid(name)
                        .and_then(|number| u32::try_from(number).ok())
                        .ok_or(FileSystemError::NotFound)?;
                    let identity = self
                        .source
                        .task_identity(&self.pid_namespace, pid)
                        .ok_or(FileSystemError::NotFound)?;
                    if identity.process.key() == identity.thread.key() {
                        ProcNode::ProcessDir(identity.process.key())
                    } else {
                        ProcNode::ThreadDir(
                            identity.process.key(),
                            identity.thread.key(),
                            ProcThreadDirectory::Root,
                        )
                    }
                }
            },
            ProcNode::ProcessDir(pid) => match name {
                b"." => ProcNode::ProcessDir(pid),
                b".." => ProcNode::Root,
                b"stat" => ProcNode::ProcessStat(pid),
                b"status" => ProcNode::ProcessStatus(pid),
                b"cmdline" => ProcNode::ProcessCmdline(pid),
                b"comm" => ProcNode::ProcessComm(pid),
                b"statm" => ProcNode::ProcessStatm(pid),
                b"io" => ProcNode::ProcessIo(pid),
                b"mounts" => ProcNode::ProcessMounts(pid),
                b"mountinfo" => ProcNode::ProcessMountInfo(pid),
                b"mountstats" => ProcNode::ProcessMountStats(pid),
                b"exe" => ProcNode::TaskExe(pid),
                b"root" => ProcNode::ProcessRoot(pid),
                b"cwd" => ProcNode::ProcessCwd(pid),
                b"ns" => ProcNode::NamespaceDir(pid, pid, ProcTaskDirectory::Process),
                b"net" => ProcNode::TaskNetDir(pid, pid, ProcTaskDirectory::Process),
                b"timens_offsets" => ProcNode::TimeOffsets(pid),
                b"uid_map" => ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Uid),
                b"gid_map" => ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Gid),
                b"setgroups" => {
                    ProcNode::UserMap(pid, crate::namespace::UserNamespaceMap::Setgroups)
                }
                b"task" => ProcNode::ProcessTaskDir(pid),
                b"fd" => ProcNode::TaskFdDir(pid, pid, ProcTaskDirectory::Process),
                b"fdinfo" => ProcNode::TaskFdInfoDir(pid, pid, ProcTaskDirectory::Process),
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::ProcessTaskDir(tgid) => match name {
                b"." => ProcNode::ProcessTaskDir(tgid),
                b".." => ProcNode::ProcessDir(tgid),
                _ => {
                    let tid = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    let snapshot = self
                        .source
                        .snapshot(ProcSnapshotScope::Process(tgid), &self.pid_namespace)?;
                    let process = find_process(&snapshot, tgid)?;
                    let thread = process
                        .threads
                        .iter()
                        .find(|thread| thread.tid.number_in(&self.pid_namespace) as usize == tid)
                        .ok_or(FileSystemError::NotFound)?;
                    ProcNode::ThreadDir(tgid, thread.tid.key(), ProcThreadDirectory::Group)
                }
            },
            ProcNode::ThreadDir(tgid, tid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid),
                b"ns" => ProcNode::NamespaceDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"net" => ProcNode::TaskNetDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"timens_offsets" => ProcNode::TimeOffsets(tid),
                b"uid_map" => ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Uid),
                b"gid_map" => ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Gid),
                b"setgroups" => {
                    ProcNode::UserMap(tid, crate::namespace::UserNamespaceMap::Setgroups)
                }
                b"stat" => ProcNode::ThreadStat(tgid, tid),
                b"status" => ProcNode::ThreadStatus(tgid, tid),
                b"cmdline" => ProcNode::ThreadCmdline(tgid, tid),
                b"comm" => ProcNode::ThreadComm(tgid, tid),
                b"statm" => ProcNode::ThreadStatm(tgid, tid),
                b"io" => ProcNode::ThreadIo(tgid, tid),
                b"mounts" => ProcNode::ThreadMounts(tid),
                b"mountinfo" => ProcNode::ThreadMountInfo(tid),
                b"mountstats" => ProcNode::ThreadMountStats(tid),
                b"root" => ProcNode::ThreadRoot(tid),
                b"cwd" => ProcNode::ThreadCwd(tid),
                b"exe" => ProcNode::TaskExe(tid),
                b"fd" => ProcNode::TaskFdDir(tgid, tid, ProcTaskDirectory::Thread(directory)),
                b"fdinfo" => {
                    ProcNode::TaskFdInfoDir(tgid, tid, ProcTaskDirectory::Thread(directory))
                }
                _ => return Err(FileSystemError::NotFound),
            },
            ProcNode::NamespaceDir(tgid, tid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid, tid),
                _ => ProcNode::NamespaceLink(
                    tid,
                    crate::namespace::NamespaceSelector::ALL
                        .into_iter()
                        .find(|selector| selector.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::TaskFdDir(tgid, pid, directory)
            | ProcNode::TaskFdInfoDir(tgid, pid, directory) => match name {
                b"." => self.node,
                b".." => directory.parent(tgid, pid),
                _ => {
                    let fd = parse_pid(name).ok_or(FileSystemError::NotFound)?;
                    if !self
                        .source
                        .thread_file_descriptors(pid)?
                        .is_some_and(|entries| entries.iter().any(|entry| entry.fd == fd))
                    {
                        return Err(FileSystemError::NotFound);
                    }
                    if matches!(self.node, ProcNode::TaskFdInfoDir(..)) {
                        ProcNode::TaskFdInfo(pid, fd)
                    } else {
                        ProcNode::TaskFd(pid, fd)
                    }
                }
            },
            ProcNode::SysvipcDir => match name {
                b"." => ProcNode::SysvipcDir,
                b".." => ProcNode::Root,
                _ => ProcNode::IpcTable(
                    crate::ipc::IpcTable::ALL
                        .into_iter()
                        .find(|table| table.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::SysDir if name == b"kernel" => ProcNode::KernelSysctlDir,
            ProcNode::SysDir if name == b"user" => ProcNode::UserSysctlDir,
            ProcNode::SysDir if name == b"fs" => ProcNode::FsSysctlDir,
            ProcNode::SysDir if name == b"net" => ProcNode::NetSysctlDir,
            ProcNode::SysDir => return Err(FileSystemError::NotFound),
            ProcNode::NetSysctlDir if name == b"core" => ProcNode::CoreNetSysctlDir,
            ProcNode::NetSysctlDir => return Err(FileSystemError::NotFound),
            ProcNode::CoreNetSysctlDir if name == b"optmem_max" => ProcNode::OptmemLimit,
            ProcNode::CoreNetSysctlDir => ProcNode::CoreBufferLimit(
                crate::socket::CoreBufferLimit::ALL
                    .into_iter()
                    .find(|resource| resource.name() == name)
                    .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::KernelSysctlDir if name == b"pid_max" => ProcNode::PidLimit,
            ProcNode::KernelSysctlDir => ProcNode::IpcLimit(
                crate::ipc::IpcLimit::ALL
                    .into_iter()
                    .find(|resource| resource.name() == name)
                    .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::FsSysctlDir if name == b"mount-max" => ProcNode::MountLimit,
            ProcNode::FsSysctlDir if name == b"inotify" => ProcNode::InotifySysctlDir,
            ProcNode::FsSysctlDir => return Err(FileSystemError::NotFound),
            ProcNode::InotifySysctlDir => match name {
                b"." => ProcNode::InotifySysctlDir,
                b".." => ProcNode::FsSysctlDir,
                _ => ProcNode::InotifyLimit(
                    crate::fs::InotifyLimit::ALL
                        .into_iter()
                        .find(|resource| resource.name() == name)
                        .ok_or(FileSystemError::NotFound)?,
                ),
            },
            ProcNode::UserSysctlDir => ProcNode::UserResourceLimit(
                [
                    crate::namespace::UserResource::User,
                    crate::namespace::UserResource::Uts,
                    crate::namespace::UserResource::Mount,
                    crate::namespace::UserResource::Time,
                    crate::namespace::UserResource::Pid,
                    crate::namespace::UserResource::Ipc,
                    crate::namespace::UserResource::Net,
                    crate::namespace::UserResource::InotifyInstances,
                    crate::namespace::UserResource::InotifyWatches,
                ]
                .into_iter()
                .find(|resource| resource.name() == name)
                .ok_or(FileSystemError::NotFound)?,
            ),
            ProcNode::TaskNetDir(tgid, tid, directory) => {
                network = Some(
                    self.source
                        .thread_net_namespace(tid)
                        .ok_or(FileSystemError::NotFound)?,
                );
                match name {
                    b"." => self.node,
                    b".." => directory.parent(tgid, tid),
                    b"dev" => ProcNode::NetDev,
                    b"route" => ProcNode::NetRoute,
                    _ => return Err(FileSystemError::NotFound),
                }
            }
            _ => return Err(FileSystemError::NotDirectory),
        };
        Ok(self.child_inode(node, name != b"." && name != b"..", network)?)
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
