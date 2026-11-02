//! 系统任务观测的 Linux transport adapter：只传递只读数据，不把 native capability 交给应用。
//!
//! 1. 根服务交付根 Job 的 `INSPECT | ENUMERATE` 句柄；每次请求遍历任务树，新增服务自动可见，
//!    不保留统计副本。
//! 2. 端点是 [`task_observer_protocol::PATH`] 上的 pathname socket，属主 root、组
//!    [`task_observer_protocol::GROUP`]、权限 0660；授权由 VFS 的 socket inode 写权限检查完成。
use crate::{
    fs::{AccessIdentity, OwnerModeChange, vfs},
    ipc::{Pipe, PipeDirection, ReceiveBuffer},
    net::{
        self, PreparedSocket, Socket, SocketDomain, SocketRequest, SocketSendDestination,
        SocketType, UnixAddress, UnixCredentials, UnixSendCredentials,
    },
    task::{self, PollWaitKey},
};
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use native_runtime::native_abi::{HandleBasic, info, rights};
use native_runtime::{Job, Process, Startup};
use task_observer_protocol::{HEADER_BYTES, QUERY, RECORD_BYTES, Record, UNAVAILABLE};

/// 取走根服务交付的根 Job 观察句柄；它不能终止、改名或读写任何任务。
pub(crate) fn take(startup: &mut Startup) -> Job {
    Job::from_handle(startup.take(launch_protocol::linuxd::ROOT_JOB_OBSERVER))
}

/// 在 VFS 上发布端点并启动服务线程；必须在根文件系统与 devtmpfs 挂载之后、init 发布之前调用，
/// 这样任何 Linux 进程都看不到权限尚未收紧的端点，也无法抢先占用路径。
///
/// # Panics
///
/// socket、路径或线程创建失败时 fail-stop：端点缺失会让任务管理器静默丢失系统服务。
pub(crate) fn start(observer: Job) {
    let notify = Pipe::notification_pair().expect("task observer notifications");
    let socket = PreparedSocket::try_new(net::initial_net())
        .expect("task observer socket allocation")
        .create(
            SocketRequest::decode(SocketDomain::Unix, SocketType::Datagram, 0)
                .expect("task observer transport"),
            notify,
        )
        .expect("task observer socket");
    let root = AccessIdentity::root();
    let origin = vfs().initial_origin().expect("task observer lookup origin");
    let path = task_observer_protocol::PATH;
    // 1. 以 root:root 0660 创建：在改组之前只有 root 能连接，不存在过宽的窗口。
    let status = crate::syscall::bind_unix_pathname(
        &socket,
        UnixAddress::new(path).expect("task observer address"),
        origin.clone(),
        0o660,
        &root,
    );
    assert!(status == 0, "task observer bind: {status}");
    // 2. 再把组交给监视组；组内成员由 socket inode 的组写权限放行。
    vfs()
        .open_at(origin, path, &root)
        .and_then(|inode| {
            inode.change_owner_mode(OwnerModeChange::chown(
                root,
                None,
                Some(task_observer_protocol::GROUP),
            ))
        })
        .expect("task observer ownership");
    // OWNER: 工作线程独占根 Job 观察句柄与 socket；仅在请求到来时查询真实 owner，不保留统计副本。
    task::spawn_kernel_thread("task-observer", Box::new(move || serve(socket, observer)))
        .expect("task observer worker");
}

fn serve(socket: Arc<Socket>, observer: Job) {
    let credentials = UnixSendCredentials {
        sender: UnixCredentials {
            pid: task::current_task()
                .expect("task observer execution")
                .tid_identity(),
            uid: 0,
            gid: 0,
        },
        claimed: None,
    };
    loop {
        let mut storage = [0; 8];
        let mut buffer = ReceiveBuffer::from_slice(&mut storage);
        match socket.receive_message(&mut buffer, false, false, true) {
            Ok(message) if message.full_length == QUERY.len() && storage == *QUERY => {
                let Some(source) = message.source else {
                    continue;
                };
                let reply = snapshot(&observer);
                // 不等待缓慢或已关闭的客户端；一次请求最多一个快照，收发均受现有 socket 队列限额约束。
                let _ = socket.send_to_with_ancillary(
                    &reply,
                    Some(SocketSendDestination::Address(source)),
                    &mut None,
                    &credentials,
                    None,
                    false,
                );
            }
            Ok(_) => {}
            Err(net::SocketError::Again) => {
                let (sources, _guard) = socket.wait_sources(1);
                let pipes: Vec<_> = sources
                    .into_iter()
                    .flatten()
                    .map(|source| source.into_notification())
                    .collect();
                let keys = pipes
                    .iter()
                    .map(|pipe| PollWaitKey::pipe(pipe, PipeDirection::Read, 1, false, None))
                    .collect();
                task::wait_for_poll(keys, None, || socket.poll_state().readable);
            }
            Err(error) => {
                error!("task observer receive: {error:?}");
                return;
            }
        }
    }
}

fn koid(handle: &native_runtime::Handle) -> u64 {
    handle
        .info::<HandleBasic>(info::HANDLE_BASIC)
        .map_or(0, |basic| basic.koid)
}

/// 遍历任务树，收集原生进程记录（父 Job 的进程先于子 Job，兄弟按 KOID 即创建顺序）。
fn snapshot(observer: &Job) -> Vec<u8> {
    let mut records = Vec::new();
    collect(
        observer,
        koid(crate::handles::job()),
        koid(native_runtime::self_process()),
        &mut records,
    );
    let mut output = vec![0; HEADER_BYTES + records.len() * RECORD_BYTES];
    output[..8].copy_from_slice(QUERY);
    output[8..12].copy_from_slice(&(records.len() as u32).to_le_bytes());
    for (record, bytes) in records.iter().zip(
        output[HEADER_BYTES..]
            .as_chunks_mut::<RECORD_BYTES>()
            .0
            .iter_mut(),
    ) {
        record.encode(bytes);
    }
    output
}

/// linuxd 所在 Job 里除 linuxd 自身外的成员是 Linux 进程（共享进程的成员）或承载映像的共享进程：
/// 前者已由 /proc 呈现，后者没有线程，两者都不进入快照。遍历期间结束的成员按 `ERR_NOT_FOUND` 跳过；
/// Job 树深度由根服务的启动结构决定（目前两层），递归不随负载增长。
fn collect(job: &Job, linux_job: u64, linuxd: u64, records: &mut Vec<Record>) {
    let current = koid(job);
    for process in job.member_koids(false).unwrap_or_default() {
        if current == linux_job && process != linuxd {
            continue;
        }
        if let Ok(handle) = job.member(process, rights::SAME) {
            records.push(record(&Process::from_handle(handle)));
        }
    }
    for child in job.member_koids(true).unwrap_or_default() {
        if let Ok(handle) = job.member(child, rights::SAME) {
            collect(&Job::from_handle(handle), linux_job, linuxd, records);
        }
    }
}

fn record(process: &Process) -> Record {
    let mut record = Record {
        koid: koid(process),
        cpu_ns: UNAVAILABLE,
        resident_bytes: UNAVAILABLE,
        mapped_bytes: UNAVAILABLE,
        threads: UNAVAILABLE,
        state: 2,
        name: [0; 32],
    };
    let _ = process.get_info(info::NAME, &mut record.name);
    if let Ok(basic) = process.basic() {
        record.state = u64::from(basic.exited != 0);
    }
    if let Ok(runtime) = process.runtime() {
        record.cpu_ns = runtime.cpu_time_ns;
    }
    if let Ok(stats) = process.stats() {
        record.resident_bytes = stats.resident_bytes;
        record.mapped_bytes = stats.mapped_bytes;
        record.threads = stats.threads;
    }
    record
}
