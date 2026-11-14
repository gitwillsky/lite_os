//! Linux PACKET_MMAP 接收选项与映射租约。实际帧由 netstack 的链路镜像点发布。

use alloc::sync::Arc;
use device_model::DeviceBacking;
use native_runtime::native_abi::rights;
use network_protocol::{FrameRingLayout, PacketKind, SocketError};

use super::PacketSocket;
use crate::{
    fs::chardev::MapRequest,
    memory::{DeviceMappingSource, PAGE_SIZE},
    net::{Socket, SocketBackend},
};

const TPACKET_V2: i32 = 1;
const TPACKET2_HEADER_BYTES: i32 = 32;
const ETHERNET_HEADER_BYTES: u16 = 14;
const TPACKET2_NETWORK_OFFSET: u32 = 80;

#[derive(Default)]
pub(super) struct ReceiveRing {
    // 0 是 Linux socket 默认版本。普通 recvmsg 可用，启用 mmap 时需显式选 V2。
    version: i32,
    reserve: u32,
    backing: Option<Arc<DeviceBacking>>,
}

impl PacketSocket {
    fn configure_ring(
        &self,
        [block_bytes, blocks, frame_bytes, frames]: [u32; 4],
    ) -> Result<(), SocketError> {
        let mut ring = self.ring.lock();
        if blocks == 0 {
            if frames != 0 {
                return Err(SocketError::Invalid);
            }
            if let Some(backing) = &ring.backing {
                // VMA 与准备中的 mmap 持同一 Arc；netstack 只持重复的 VMO 句柄。
                if Arc::strong_count(backing) != 1 {
                    return Err(SocketError::Busy);
                }
                self.remote.set_receive_ring(None)?;
                ring.backing = None;
            }
            return Ok(());
        }
        if ring.backing.is_some() {
            return Err(SocketError::Busy);
        }
        if ring.version != TPACKET_V2 {
            return Err(SocketError::Invalid);
        }

        // TPACKET_ALIGN(32 + sizeof(sockaddr_ll) + max(16, 14)) + tp_reserve。
        let network_offset = TPACKET2_NETWORK_OFFSET
            .checked_add(ring.reserve)
            .and_then(|offset| u16::try_from(offset).ok())
            .ok_or(SocketError::Invalid)?;
        let link_offset = match self.kind {
            PacketKind::Raw => network_offset - ETHERNET_HEADER_BYTES,
            PacketKind::Datagram => network_offset,
        };
        let layout = FrameRingLayout {
            block_bytes,
            blocks,
            frame_bytes,
            frames,
            link_offset,
            network_offset,
            address_family: 17, // AF_PACKET
            user: 1,
            losing: 1 << 2,
            checksum_pending: 1 << 3,
            checksum_verified: 1 << 7,
        };
        let pages = layout.byte_len()? / PAGE_SIZE;
        let backing =
            Arc::try_new(DeviceBacking::try_allocate(pages).ok_or(SocketError::NoMemory)?)
                .map_err(|_| SocketError::NoMemory)?;
        let remote = backing
            .vmo()
            .duplicate(rights::SAME)
            .map_err(|_| SocketError::NoMemory)?;
        self.remote.set_receive_ring(Some((layout, remote)))?;
        ring.backing = Some(backing);
        Ok(())
    }
}

impl Socket {
    /// 启用或关闭本文件的 PACKET_RX_RING。
    /// # Errors
    /// 无效请求、内存不足、已有环或映射仍在使用。
    pub(crate) fn configure_packet_ring(&self, geometry: [u32; 4]) -> Result<(), SocketError> {
        self.packet_socket()?.configure_ring(geometry)
    }

    /// 保存后续接收环的版本或报文预留空间。
    /// # Errors
    /// 无效版本或预留量，或接收环已在使用。
    pub(crate) fn set_packet_ring_option(
        &self,
        version: bool,
        value: i32,
    ) -> Result<(), SocketError> {
        let mut ring = self.packet_socket()?.ring.lock();
        if (version && value != TPACKET_V2) || (!version && value < 0) {
            return Err(SocketError::Invalid);
        }
        if ring.backing.is_some() {
            return Err(SocketError::Busy);
        }
        if version {
            ring.version = value;
        } else {
            ring.reserve = value as u32;
        }
        Ok(())
    }

    /// 查询本文件的接收环版本或报文预留空间。
    /// # Errors
    /// 文件不是 packet socket。
    pub(crate) fn packet_ring_option(&self, version: bool) -> Result<i32, SocketError> {
        let ring = self.packet_socket()?.ring.lock();
        Ok(if version {
            ring.version
        } else {
            ring.reserve as i32
        })
    }

    /// 查询实现的 PACKET_HDRLEN 版本，供 libpcap 选择记录格式。
    /// # Errors
    /// 不支持指定版本。
    pub(crate) fn packet_header_length(version: i32) -> Result<i32, SocketError> {
        if version != TPACKET_V2 {
            return Err(SocketError::Invalid);
        }
        Ok(TPACKET2_HEADER_BYTES)
    }

    /// 为共享的 socket 映射保留接收环；最后一份 VMA 租约释放后可关闭环。
    /// # Errors
    /// 非 packet socket、映射参数无效或无法准备映射来源。
    pub(crate) fn map_packet_ring(
        &self,
        offset: usize,
        length: usize,
        request: MapRequest,
    ) -> Result<DeviceMappingSource, isize> {
        use linux_abi::errno;

        let socket = self.packet_socket().map_err(|_| errno::ENODEV)?;
        let ring = socket.ring.lock();
        let backing = ring.backing.as_ref().ok_or(errno::EINVAL)?;
        if offset != 0 || !request.shared || length != backing.pages() * PAGE_SIZE {
            return Err(errno::EINVAL);
        }
        if request.executable || !request.fd_readable || request.writable && !request.fd_writable {
            return Err(errno::EACCES);
        }
        DeviceMappingSource::new(backing.clone()).map_err(|_| errno::ENOMEM)
    }

    fn packet_socket(&self) -> Result<&PacketSocket, SocketError> {
        match &self.backend {
            SocketBackend::Packet(socket) => Ok(socket),
            _ => Err(SocketError::ProtocolOptionUnavailable),
        }
    }
}
