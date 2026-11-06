//! 系统任务观测的 Linux transport adapter：只传递只读数据，不把 native capability 交给应用。
//!
//! 1. 根服务交付根 Job 的 `INSPECT | ENUMERATE` 句柄；每次请求遍历任务树，新增服务自动可见，
//!    不保留统计副本。
//! 2. 端点是 [`task_observer_protocol::PATH`] 上的 pathname socket，属主 root、组
//!    [`task_observer_protocol::GROUP`]、权限 0660（发布与授权见 [`super::endpoint`]）。
use crate::{net::Socket, task};
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use native_runtime::native_abi::{HandleBasic, info, rights};
use native_runtime::{Job, Process, Startup};
use task_observer_protocol::{HEADER_BYTES, QUERY, RECORD_BYTES, Record, UNAVAILABLE};

/// 取走根服务交付的根 Job 观察句柄；它不能终止、改名或读写任何任务。
pub(crate) fn take(startup: &mut Startup) -> Job {
    Job::from_handle(startup.take(launch_protocol::linuxd::ROOT_JOB_OBSERVER))
}

/// 发布端点并启动服务线程；必须在根文件系统与 devtmpfs 挂载之后、init 发布之前调用（见
/// [`super::endpoint`]）。
///
/// # Panics
///
/// 端点或线程创建失败时 fail-stop：端点缺失会让任务管理器静默丢失系统服务。
pub(crate) fn start(observer: Job) {
    let socket = super::endpoint::publish(
        task_observer_protocol::PATH,
        task_observer_protocol::GROUP,
        0o660,
    );
    // OWNER: 工作线程独占根 Job 观察句柄与 socket；仅在请求到来时查询真实 owner，不保留统计副本。
    task::spawn_kernel_thread("task-observer", Box::new(move || serve(socket, observer)))
        .expect("task observer worker");
}

fn serve(socket: Arc<Socket>, observer: Job) {
    loop {
        let mut storage = [0; 8];
        match super::endpoint::receive(&socket, &mut storage) {
            Ok((_, full_length, source)) if full_length == QUERY.len() && storage == *QUERY => {
                super::endpoint::reply(&socket, source, &snapshot(&observer));
            }
            Ok(_) => {}
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
/// Job 树深度由启动结构决定（根 Job → 服务 Job，driver-manager 下另有驱动宿主 Job 与每个宿主的 Job，
/// 至多四层），递归不随负载增长。
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
