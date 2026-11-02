//! 系统任务观测的 Linux transport adapter：只传递只读数据，不把 native capability 交给应用。
use crate::{
    ipc::{Pipe, PipeDirection, ReceiveBuffer},
    net::{
        self, PreparedSocket, Socket, SocketAddress, SocketDomain, SocketRequest,
        SocketSendDestination, SocketType, UnixAddress, UnixCredentials, UnixSendCredentials,
    },
    task::{self, PollWaitKey},
};
use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
use native_runtime::{Process, Startup};
use task_observer_protocol::{HEADER_BYTES, QUERY, RECORD_BYTES, Record, UNAVAILABLE};

/// 取走根服务交付的 INSPECT-only 观察句柄；不新增管理权限。
pub(crate) fn take(startup: &mut Startup) -> [Process; launch_protocol::linuxd::OBSERVER_COUNT] {
    core::array::from_fn(|index| {
        Process::from_handle(startup.take(launch_protocol::linuxd::OBSERVERS + index))
    })
}

/// 安装只读 datagram endpoint，线程随 linuxd 生命周期结束；失败不保留半发布状态。
pub(crate) fn start(observers: [Process; launch_protocol::linuxd::OBSERVER_COUNT]) {
    let notify = Pipe::notification_pair().expect("task observer notifications");
    let socket = PreparedSocket::try_new(net::initial_net())
        .expect("task observer socket allocation")
        .create(
            SocketRequest::decode(SocketDomain::Unix, SocketType::Datagram, 0)
                .expect("task observer transport"),
            notify,
        )
        .expect("task observer socket");
    let mut address = vec![0];
    address.extend_from_slice(task_observer_protocol::ADDRESS);
    socket
        .bind(
            SocketAddress::Unix(UnixAddress::new(&address).expect("task observer address")),
            0,
        )
        .expect("task observer bind");
    // OWNER: 工作线程独占观察句柄与 socket；仅在请求到来时查询真实 owner，不保留资源统计副本。
    task::spawn_kernel_thread("task-observer", Box::new(move || serve(socket, observers)))
        .expect("task observer worker");
}

fn serve(socket: Arc<Socket>, observers: [Process; launch_protocol::linuxd::OBSERVER_COUNT]) {
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
                let reply = snapshot(&observers);
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

fn snapshot(observers: &[Process]) -> Vec<u8> {
    let count = observers.len() + 1;
    let mut output = vec![0; HEADER_BYTES + count * RECORD_BYTES];
    output[..8].copy_from_slice(QUERY);
    output[8..12].copy_from_slice(&(count as u32).to_le_bytes());
    for (process, bytes) in observers
        .iter()
        .chain(core::iter::once(native_runtime::self_process()))
        .zip(
            output[HEADER_BYTES..]
                .as_chunks_mut::<RECORD_BYTES>()
                .0
                .iter_mut(),
        )
    {
        let mut record = Record {
            koid: process
                .info::<native_runtime::native_abi::HandleBasic>(
                    native_runtime::native_abi::info::HANDLE_BASIC,
                )
                .map_or(0, |basic| basic.koid),
            cpu_ns: UNAVAILABLE,
            resident_bytes: UNAVAILABLE,
            mapped_bytes: UNAVAILABLE,
            threads: UNAVAILABLE,
            state: 2,
            name: [0; 32],
        };
        let _ = process.get_info(native_runtime::native_abi::info::NAME, &mut record.name);
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
        record.encode(bytes);
    }
    output
}
