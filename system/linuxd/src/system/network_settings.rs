//! 网络设置的 Linux 适配：把 netcfg 的 `liteos.network.Settings` 交给 netdev 组的系统程序（控制面板、resolvd）。
//!
//! 1. 端点是 [`netcfg_protocol::bridge::PATH`]（root:netdev 0660，发布与授权见 [`super::endpoint`]）；
//! 2. 请求 datagram 为 `u32` 序号后接载荷，回复为 `i32` 状态后接载荷；载荷是 `netcfg_protocol` 的编码：
//!    GET 无载荷、回复快照；SET 载荷为接口名与配置；WAIT 载荷为 `u64` 代际、回复新代际；
//! 3. GET/SET 在服务线程上经一条到 netcfg 的连接转发（netcfg 重启后重连一次）；WAIT 会阻塞到网络状态变化，
//!    每个 WAIT 在独立内核线程上经新连接转发，同时在途至多 [`WAIT_LIMIT`] 个，超出返回资源不足。

use crate::{net::Socket, task};
use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};
use native_runtime::Channel;
use native_runtime::native_abi::{self, Status};
use netcfg_protocol::{Decoder, Encoder, bridge, ordinal};

/// 同时在途的 WAIT 上限。
const WAIT_LIMIT: usize = 4;
// OWNER: 在途 WAIT 的唯一计数（递增于派生线程前，递减于回复后）；缺失时不断发 WAIT 的客户端可以让
// linuxd 创建无界数量的内核线程。
static WAITS: AtomicUsize = AtomicUsize::new(0);

/// 发布端点并启动服务线程；与其他授权端点相同，须在 devtmpfs 挂载之后、init 发布之前调用。
///
/// # Panics
///
/// 端点或线程创建失败。
pub(crate) fn start() {
    let socket = super::endpoint::publish(bridge::PATH, bridge::GROUP);
    task::spawn_kernel_thread("network-settings", Box::new(move || serve(socket)))
        .expect("network settings worker");
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

fn serve(socket: Arc<Socket>) {
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
        if ordinal == ordinal::WAIT {
            wait(&socket, source, payload);
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
            let name = decoder.bytes().map_err(invalid)?;
            let config = decoder.config().map_err(invalid)?;
            decoder.finish().map_err(invalid)?;
            let mut encoded = Encoder::default();
            encoded.config(&config);
            request.bytes(name).bytes(&encoded.0);
        }
        ordinal::WAIT => {
            let generation = decoder.u64().map_err(invalid)?;
            decoder.finish().map_err(invalid)?;
            request.u64(generation);
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

/// 在独立内核线程上经新连接转发一个 WAIT。
fn wait(socket: &Arc<Socket>, source: crate::net::SocketAddress, payload: &[u8]) {
    if WAITS.fetch_add(1, Ordering::AcqRel) >= WAIT_LIMIT {
        WAITS.fetch_sub(1, Ordering::AcqRel);
        super::endpoint::reply(socket, source, &frame(Err(native_abi::ERR_NO_RESOURCES)));
        return;
    }
    let socket = socket.clone();
    let payload = payload.to_vec();
    let worker = Box::new(move || {
        let result = connect().and_then(|channel| forward(&channel, ordinal::WAIT, &payload));
        super::endpoint::reply(&socket, source, &frame(result));
        WAITS.fetch_sub(1, Ordering::AcqRel);
    });
    if task::spawn_kernel_thread("network-settings-wait", worker).is_err() {
        WAITS.fetch_sub(1, Ordering::AcqRel);
    }
}
