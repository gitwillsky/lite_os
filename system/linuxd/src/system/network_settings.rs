//! 网络设置的 Linux 适配：把 router_config 的 `liteos.network.Settings` 交给 netdev 组的系统程序（控制面板、resolvd）。
//!
//! 1. 状态端点只接受 GET；设置端点限 root:netdev，接受 SET、DHCP 操作和 resolvd 的 WAIT；
//! 2. 请求 datagram 为 `u32` 序号后接载荷，回复为 `i32` 状态后接载荷；载荷是 `router_protocol` 的编码：
//!    GET 无载荷、回复快照；SET 载荷由 router_config 解码并提交；WAIT 载荷为 `u64` 代际、回复新代际；
//! 3. 只读查询重连后可重放，修改结果不确定时不自动重放；WAIT/DHCP 共用 [`WAIT_LIMIT`] 个在途请求。
//! 4. 原生配置管理的控制帧和文档分离，正文经 SCM_RIGHTS ↔ 独立只读 VMO，不使用内联旁路。

use crate::{net::Socket, task};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};
use router_protocol::{Decoder, bridge, ordinal};

mod configuration;

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
        let mut message = match super::endpoint::receive_message(&socket, &mut storage, true) {
            Ok(message) => message,
            Err(error) => {
                error!("network settings receive: {error:?}");
                return;
            }
        };
        let (count, full_length) = (message.count, message.full_length);
        let source = message.source.take().expect("endpoint source");
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
        let configuration = ordinal::is_configuration(ordinal);
        let input = if configuration {
            match configuration::prepare(ordinal, payload, message.rights.take()) {
                Ok(input) => input,
                Err(error) => {
                    super::endpoint::reply(&socket, source, &frame(Err(error)));
                    continue;
                }
            }
        } else if message.rights.is_some() {
            super::endpoint::reply(&socket, source, &frame(Err(native_abi::ERR_INVALID_ARGS)));
            continue;
        } else {
            None
        };
        if matches!(ordinal, ordinal::WAIT | ordinal::DHCP) {
            forward_waiting(&socket, source, ordinal, payload);
            continue;
        }
        // 只读查询可在重连后重放。修改请求的 PEER_CLOSED 可能发生在持久提交后，不能自动重复。
        let replayable = matches!(
            ordinal,
            ordinal::GET
                | ordinal::EXPORT
                | ordinal::SCHEMA
                | ordinal::VALIDATE
                | ordinal::DIFF
                | ordinal::BACKUPS
                | ordinal::BACKUP_EXPORT
        );
        let mut result = Err(native_abi::ERR_PEER_CLOSED);
        for _ in 0..2 {
            let channel = match settings.take().map_or_else(connect, Ok) {
                Ok(channel) => channel,
                Err(status) => {
                    result = Err(status);
                    break;
                }
            };
            result = if configuration {
                configuration::forward(&channel, ordinal, payload, input.as_ref())
            } else {
                forward(&channel, ordinal, payload).map(|bytes| (bytes, None))
            };
            if !matches!(result, Err(native_abi::ERR_PEER_CLOSED)) {
                settings = Some(channel);
                break;
            }
            if !replayable {
                break;
            }
        }
        match result {
            Ok((payload, rights)) => {
                super::endpoint::reply_with_rights(&socket, source, &frame(Ok(payload)), rights)
            }
            Err(error) => super::endpoint::reply(&socket, source, &frame(Err(error))),
        }
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
    if !reply.handles.is_empty() {
        return Err(native_abi::ERR_INVALID_ARGS);
    }
    let mut reader = reply.reader();
    let result = match ordinal {
        ordinal::GET => reader.bytes()?.to_vec(),
        ordinal::WAIT => reader.u64()?.to_le_bytes().to_vec(),
        _ => Vec::new(),
    };
    reader.finish()?;
    if result.len() > bridge::DATAGRAM_MAX - bridge::HEADER_BYTES {
        return Err(native_abi::ERR_OUT_OF_RANGE);
    }
    Ok(result)
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
