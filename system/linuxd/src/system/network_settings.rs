//! 网络设置的 Linux 适配：把 netcfg 的 `liteos.network.Settings` 交给 netdev 组的系统程序（控制面板、resolvd）。
//!
//! 1. 状态端点只接受 GET；设置端点限 root:netdev，接受 SET、DHCP 操作和 resolvd 的 WAIT；
//! 2. 请求 datagram 为 `u32` 序号后接载荷，回复为 `i32` 状态后接载荷；载荷是 `netcfg_protocol` 的编码：
//!    GET 无载荷、回复快照；SET 载荷由 netcfg 解码并提交；WAIT 载荷为 `u64` 代际、回复新代际；
//! 3. GET/SET 在服务线程上转发（netcfg 重启后重连一次）；WAIT 与 DHCP 操作各用独立线程和连接，
//!    共享 [`WAIT_LIMIT`] 个在途请求。DHCP 操作断线后不重放。

use crate::{net::Socket, task};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};
use netcfg_protocol::{Decoder, bridge, ordinal};

/// 同时在途的 WAIT 与 DHCP 操作上限。
const WAIT_LIMIT: usize = 4;
// OWNER: 阻塞请求的在途计数，在线程派生前递增、回复后递减。否则客户端能创建无界数量的内核线程。
static WAITS: AtomicUsize = AtomicUsize::new(0);

/// 发布端点并启动服务线程；与其他授权端点相同，须在 devtmpfs 挂载之后、init 发布之前调用。
///
/// # Panics
///
/// 端点或线程创建失败。
pub(crate) fn start() {
    let settings = super::endpoint::publish(bridge::PATH, bridge::GROUP, 0o660);
    let status = super::endpoint::publish(bridge::STATUS_PATH, 0, 0o666);
    task::spawn_kernel_thread("network-settings", Box::new(move || serve(settings, true)))
        .expect("network settings worker");
    task::spawn_kernel_thread("network-status", Box::new(move || serve(status, false)))
        .expect("network status worker");
}

fn connect() -> Result<Channel, Status> {
    crate::handles::services().connect(service_directory::NETWORK_SETTINGS)
}

fn frame(result: Result<Vec<u8>, Status>) -> Vec<u8> {
    let (status, payload) = match result {
        Ok(payload) => (native_abi::OK, payload),
        Err(status) => (status, Vec::new()),
    };
    let mut reply = Vec::with_capacity(bridge::HEADER_BYTES + payload.len());
    reply.extend_from_slice(&status.to_le_bytes());
    reply.extend_from_slice(&payload);
    reply
}

fn serve(socket: Arc<Socket>, writable: bool) {
    let mut settings: Option<Channel> = None;
    let mut storage = alloc::vec![0u8; bridge::DATAGRAM_MAX];
    loop {
        let (count, full_length, source) = match super::endpoint::receive(&socket, &mut storage) {
            Ok(received) => received,
            Err(error) => {
                error!("network settings receive: {error:?}");
                return;
            }
        };
        if full_length != count || count < bridge::HEADER_BYTES {
            super::endpoint::reply(&socket, source, &frame(Err(native_abi::ERR_INVALID_ARGS)));
            continue;
        }
        let ordinal = u32::from_le_bytes(storage[..4].try_into().expect("four header bytes"));
        let payload = &storage[bridge::HEADER_BYTES..count];
        if (ordinal == ordinal::GET) == writable {
            super::endpoint::reply(&socket, source, &frame(Err(native_abi::ERR_ACCESS_DENIED)));
            continue;
        }
        if matches!(ordinal, ordinal::WAIT | ordinal::DHCP) {
            forward_waiting(&socket, source, ordinal, payload);
            continue;
        }
        // netcfg 重启后旧连接 PEER_CLOSED：重连一次再转发。
        let mut result = Err(native_abi::ERR_PEER_CLOSED);
        for _ in 0..2 {
            let channel = match settings.take().map_or_else(connect, Ok) {
                Ok(channel) => channel,
                Err(status) => {
                    result = Err(status);
                    break;
                }
            };
            result = forward(&channel, ordinal, payload);
            if result != Err(native_abi::ERR_PEER_CLOSED) {
                settings = Some(channel);
                break;
            }
        }
        super::endpoint::reply(&socket, source, &frame(result));
    }
}

/// 把 Linux 载荷翻译为原生请求并转发。
fn forward(channel: &Channel, ordinal: u32, payload: &[u8]) -> Result<Vec<u8>, Status> {
    let invalid = |_| native_abi::ERR_INVALID_ARGS;
    let mut request = service_rpc::Writer::request(ordinal);
    let mut decoder = Decoder::new(payload);
    match ordinal {
        ordinal::GET => decoder.finish().map_err(invalid)?,
        ordinal::SET => {
            request.bytes(payload);
        }
        ordinal::WAIT => {
            let generation = decoder.u64().map_err(invalid)?;
            decoder.finish().map_err(invalid)?;
            request.u64(generation);
        }
        ordinal::DHCP => {
            let name = decoder.bytes().map_err(invalid)?;
            let action = decoder.u8().map_err(invalid)?;
            decoder.finish().map_err(invalid)?;
            request.bytes(name).u8(action);
        }
        ordinal::RENAME => {
            let interface = decoder.bytes().map_err(invalid)?;
            let name = decoder.bytes().map_err(invalid)?;
            decoder.finish().map_err(invalid)?;
            request.bytes(interface).bytes(name);
        }
        _ => return Err(native_abi::ERR_NOT_SUPPORTED),
    }
    let reply = service_rpc::call(channel, &request, Vec::new())?;
    let mut reader = reply.reader();
    match ordinal {
        ordinal::GET => Ok(reader.bytes()?.to_vec()),
        ordinal::WAIT => Ok(reader.u64()?.to_le_bytes().to_vec()),
        _ => Ok(Vec::new()),
    }
}

/// 在独立内核线程上经新连接转发一个可能等待的请求。
fn forward_waiting(
    socket: &Arc<Socket>,
    source: crate::net::SocketAddress,
    ordinal: u32,
    payload: &[u8],
) {
    if WAITS.fetch_add(1, Ordering::AcqRel) >= WAIT_LIMIT {
        WAITS.fetch_sub(1, Ordering::AcqRel);
        super::endpoint::reply(socket, source, &frame(Err(native_abi::ERR_NO_RESOURCES)));
        return;
    }
    let worker_socket = socket.clone();
    let worker_source = source.clone();
    let payload = payload.to_vec();
    let worker = Box::new(move || {
        let result = connect().and_then(|channel| forward(&channel, ordinal, &payload));
        super::endpoint::reply(&worker_socket, worker_source, &frame(result));
        WAITS.fetch_sub(1, Ordering::AcqRel);
    });
    if task::spawn_kernel_thread("network-settings-wait", worker).is_err() {
        WAITS.fetch_sub(1, Ordering::AcqRel);
        super::endpoint::reply(socket, source, &frame(Err(native_abi::ERR_NO_RESOURCES)));
    }
}
