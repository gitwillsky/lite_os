//! Socket facade 的唯一未发布 allocation；backend 创建或 accept 前准备，提交时不再分配。
use alloc::sync::Arc;
use core::mem::MaybeUninit;

use super::{InetProtocol, InetSocket, KobjectSocket, PacketSocket, UnixSocket};
use super::{Socket, SocketBackend, SocketDomain, SocketError, SocketTimeouts, SocketType};
use crate::ipc::PipeEnd;

/// 唯一已验证 endpoint 请求；decode 在 backend allocation 前完成，backend 不再重复 domain/type/protocol policy。
#[derive(Clone, Copy)]
enum BackendRequest {
    Unix(SocketType),
    Ipv4(InetProtocol),
    Packet(SocketType, usize),
    Uevent(SocketType),
}
/// 经 decode 唯一构造的请求；private backend value 防止 caller 绕过协议验证。
#[derive(Clone, Copy)]
pub(crate) struct SocketRequest(BackendRequest);
impl SocketRequest {
    /// 验证 native socket 组合；unsupported 返回 ProtocolNotSupported，尚无 allocation/backend side effect。
    pub(crate) fn decode(
        domain: SocketDomain,
        kind: SocketType,
        protocol: usize,
    ) -> Result<Self, SocketError> {
        match (domain, kind, protocol) {
            (
                SocketDomain::Unix,
                SocketType::Stream | SocketType::Datagram | SocketType::SeqPacket,
                0,
            ) => Ok(BackendRequest::Unix(kind)),
            (SocketDomain::Inet, SocketType::Datagram, 0 | 17) => {
                Ok(BackendRequest::Ipv4(InetProtocol::Udp))
            }
            (SocketDomain::Inet, SocketType::Stream, 0 | 6) => {
                Ok(BackendRequest::Ipv4(InetProtocol::Tcp))
            }
            (SocketDomain::Inet, SocketType::Raw, 255) => {
                Ok(BackendRequest::Ipv4(InetProtocol::Raw))
            }
            (SocketDomain::Inet, SocketType::Raw, 1) => {
                Ok(BackendRequest::Ipv4(InetProtocol::Icmp))
            }
            (SocketDomain::Packet, SocketType::Datagram | SocketType::Raw, _) => {
                Ok(BackendRequest::Packet(kind, protocol))
            }
            (SocketDomain::Netlink, SocketType::Datagram | SocketType::Raw, 15) => {
                Ok(BackendRequest::Uevent(kind))
            }
            _ => Err(SocketError::ProtocolNotSupported),
        }
        .map(Self)
    }
    fn domain(self) -> SocketDomain {
        match self.0 {
            BackendRequest::Unix(_) => SocketDomain::Unix,
            BackendRequest::Packet(_, _) => SocketDomain::Packet,
            BackendRequest::Uevent(_) => SocketDomain::Netlink,
            _ => SocketDomain::Inet,
        }
    }
    fn kind(self) -> SocketType {
        match self.0 {
            BackendRequest::Unix(kind)
            | BackendRequest::Packet(kind, _)
            | BackendRequest::Uevent(kind) => kind,
            BackendRequest::Ipv4(InetProtocol::Tcp) => SocketType::Stream,
            BackendRequest::Ipv4(InetProtocol::Icmp | InetProtocol::Raw) => SocketType::Raw,
            _ => SocketType::Datagram,
        }
    }
}

/// 只持有尚未建立 endpoint lifecycle 的 allocation 与稳定匿名 inode identity。
/// Token 不 clone、不生成 Weak；任何失败只释放未初始化 storage。
pub(crate) struct PreparedSocket {
    allocation: Arc<MaybeUninit<Socket>>,
    object_id: u64,
    timeouts: Arc<SocketTimeouts>,
    memory: Arc<super::SocketMemory>,
}
impl PreparedSocket {
    /// 在 endpoint 副作用前分配 facade；OOM 返回 NoMemory。
    pub(crate) fn try_new(namespace: Arc<super::NetNamespace>) -> Result<Self, SocketError> {
        let allocation = Arc::try_new_uninit().map_err(|_| SocketError::NoMemory)?;
        let timeouts = Arc::try_new(SocketTimeouts::new()).map_err(|_| SocketError::NoMemory)?;
        let memory = super::SocketMemory::try_new(namespace)?;
        Ok(Self {
            allocation,
            object_id: crate::id::next_runtime_object_id(),
            timeouts,
            memory,
        })
    }

    /// 返回本次创建/accept 的实际 facade identity，用于预先准备 socket inode。
    pub(crate) fn object_id(&self) -> u64 {
        self.object_id
    }

    /// 消费准备资源创建 endpoint；不支持的组合或 backend OOM 返回原 socket error。
    pub(crate) fn create(
        self,
        request: SocketRequest,
        notify: (Arc<PipeEnd>, Arc<PipeEnd>),
    ) -> Result<Arc<Socket>, SocketError> {
        let backend = match request.0 {
            BackendRequest::Unix(kind) => SocketBackend::Unix(UnixSocket::new(
                kind,
                notify,
                self.object_id,
                self.memory.namespace(),
            )?),
            BackendRequest::Ipv4(protocol) => SocketBackend::Inet(InetSocket::new(
                protocol,
                notify,
                self.timeouts.clone(),
                self.memory.clone(),
            )?),
            BackendRequest::Packet(kind, protocol) => SocketBackend::Packet(PacketSocket::new(
                kind,
                protocol,
                notify,
                self.memory.clone(),
            )?),
            BackendRequest::Uevent(_) => {
                SocketBackend::Kobject(KobjectSocket::new(notify, self.memory.clone())?)
            }
        };
        Ok(self.initialize(request.domain(), request.kind(), backend))
    }

    /// 从 listener 取连接。失败归还完整 token，EAGAIN 等待后可复用同一 allocation。
    /// backend 只有在自身准备完成后才消费连接；返回后 facade 提交无分配。
    pub(crate) fn accept(
        self,
        listener: &Socket,
        notify: Option<(Arc<PipeEnd>, Arc<PipeEnd>)>,
    ) -> Result<Arc<Socket>, (SocketError, Self)> {
        // Linux unix_create1 使用已准备的默认 policy；TCP 从实际 pending handle 取握手快照。
        let result = match &listener.backend {
            SocketBackend::Unix(socket) => socket.accept().map(SocketBackend::Unix),
            SocketBackend::Inet(socket) => match notify {
                Some(notify) => socket
                    .accept(notify, self.timeouts.clone(), self.memory.clone())
                    .map(SocketBackend::Inet),
                None => Err(SocketError::NoMemory),
            },
            _ => Err(SocketError::OperationNotSupported),
        };
        match result {
            Ok(backend) => Ok(self.initialize(listener.domain, listener.socket_type, backend)),
            Err(error) => Err((error, self)),
        }
    }

    fn initialize(
        mut self,
        domain: SocketDomain,
        socket_type: SocketType,
        backend: SocketBackend,
    ) -> Arc<Socket> {
        Arc::get_mut(&mut self.allocation)
            .expect("unpublished Socket allocation must be unique")
            .write(Socket {
                domain,
                socket_type,
                backend,
                timeouts: self.timeouts,
                memory: self.memory,
            });
        // SAFETY: token 从未 clone/Weak/publication；上面唯一 write 完整初始化全部 fields，
        // write 与 assume_init 之间没有 fallible operation，只有此处建立实际 Socket lifecycle。
        unsafe { self.allocation.assume_init() }
    }
}
