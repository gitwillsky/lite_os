use alloc::{sync::Arc, vec::Vec};

use super::{SocketAddress, encode_address, errno, interface_snapshot, socket_error};
use crate::{
    fs::{FileDescriptorError, OpenFileDescription},
    socket::{Socket, SocketDomain, UnixPassedFile, UnixRights},
    task::TaskControlBlock,
};

const CMSG_HEADER: usize = 16;
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const IPPROTO_IP: i32 = 0;
const IP_PKTINFO: i32 = 8;
const IP_PKTINFO_LENGTH: usize = CMSG_HEADER + 12;
const IP_PKTINFO_SPACE: usize = 32;
const INTERFACE_INDEX: i32 = 1;

pub(super) const MSG_CTRUNC: i32 = 0x8;
pub(super) const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

fn align(length: usize) -> Option<usize> {
    length.checked_add(7).map(|value| value & !7)
}

fn file_error(error: FileDescriptorError) -> isize {
    match error {
        FileDescriptorError::NotFound => -errno::EBADF,
        FileDescriptorError::Limit => -errno::EMFILE,
        FileDescriptorError::OutOfMemory => -errno::ENOMEM,
        FileDescriptorError::Busy => -errno::EBUSY,
    }
}

/// @description 解析 sendmsg control list，并在一次 fd-table snapshot 中捕获 SCM_RIGHTS。
/// @param task caller 与 fd-table owner。
/// @param socket 目标 socket façade，用于 domain policy。
/// @param pointer raw msg_control。
/// @param length raw msg_controllen。
/// @return 无 rights 或一条合并且保持用户顺序的 rights 集合。
/// @errors malformed/unsupported cmsg、无效 fd、地址 policy 或 OOM 返回标准 errno。
pub(super) fn parse_send(
    task: &TaskControlBlock,
    socket: &Arc<Socket>,
    pointer: usize,
    length: usize,
) -> Result<Option<UnixRights>, isize> {
    if length == 0 {
        return Ok(None);
    }
    if pointer == 0 {
        return Err(-errno::EFAULT);
    }
    let mut descriptors = Vec::new();
    let mut offset = 0usize;
    while length - offset >= CMSG_HEADER {
        let mut header = [0u8; CMSG_HEADER];
        task.copy_from_user(pointer + offset, &mut header)
            .map_err(|_| -errno::EFAULT)?;
        let cmsg_length = usize::from_ne_bytes(header[..8].try_into().unwrap());
        if cmsg_length < CMSG_HEADER || cmsg_length > length - offset {
            return Err(-errno::EINVAL);
        }
        let level = i32::from_ne_bytes(header[8..12].try_into().unwrap());
        let kind = i32::from_ne_bytes(header[12..16].try_into().unwrap());
        let data_length = cmsg_length - CMSG_HEADER;
        match (level, kind) {
            (SOL_SOCKET, SCM_RIGHTS) => {
                if socket.domain() != SocketDomain::Unix
                    || data_length == 0
                    || !data_length.is_multiple_of(core::mem::size_of::<i32>())
                {
                    return Err(-errno::EINVAL);
                }
                let count = data_length / core::mem::size_of::<i32>();
                if descriptors.len().saturating_add(count) > crate::socket::SCM_MAX_FD {
                    return Err(-errno::EINVAL);
                }
                descriptors.try_reserve(count).map_err(|_| -errno::ENOMEM)?;
                for index in 0..count {
                    let mut encoded = [0u8; 4];
                    task.copy_from_user(pointer + offset + CMSG_HEADER + index * 4, &mut encoded)
                        .map_err(|_| -errno::EFAULT)?;
                    let fd = i32::from_ne_bytes(encoded);
                    if fd < 0 {
                        return Err(-errno::EBADF);
                    }
                    descriptors.push(fd as usize);
                }
            }
            (IPPROTO_IP, IP_PKTINFO) if cmsg_length >= IP_PKTINFO_LENGTH => {
                let mut packet_info = [0u8; 12];
                task.copy_from_user(pointer + offset + CMSG_HEADER, &mut packet_info)
                    .map_err(|_| -errno::EFAULT)?;
                let requested =
                    core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&packet_info[4..8]).unwrap());
                if !requested.is_unspecified()
                    && interface_snapshot()
                        .map_err(socket_error)?
                        .address
                        .is_none_or(|address| address != requested)
                {
                    return Err(-errno::EADDRNOTAVAIL);
                }
            }
            _ => return Err(-errno::EOPNOTSUPP),
        }
        let next = align(cmsg_length).ok_or(-errno::EINVAL)?;
        if next > length - offset {
            if cmsg_length == length - offset {
                offset = length;
                break;
            }
            return Err(-errno::EINVAL);
        }
        offset += next;
    }
    if offset != length {
        return Err(-errno::EINVAL);
    }
    if descriptors.is_empty() {
        return Ok(None);
    }
    let files = task.fd_capture_many(&descriptors).map_err(file_error)?;
    let mut passed: Vec<Arc<dyn UnixPassedFile>> = Vec::new();
    passed
        .try_reserve_exact(files.len())
        .map_err(|_| -errno::ENOMEM)?;
    passed.extend(
        files
            .into_iter()
            .map(|file| -> Arc<dyn UnixPassedFile> { file }),
    );
    UnixRights::new(
        passed,
        task.credential_id(true, false),
        task.file_descriptor_limit(),
    )
    .map(Some)
    .map_err(socket_error)
}

fn rights_capacity(remaining: usize, count: usize) -> usize {
    count.min(remaining.saturating_sub(CMSG_HEADER) / 4)
}

fn write_rights(
    task: &TaskControlBlock,
    pointer: usize,
    remaining: usize,
    rights: UnixRights,
    cloexec: bool,
) -> (usize, bool) {
    let total = rights.len();
    let fit = rights_capacity(remaining, total);
    if fit == 0 {
        drop(rights);
        return (0, true);
    }
    let mut installed = 0;
    for file in rights.into_files().into_iter().take(fit) {
        let file = Arc::downcast::<OpenFileDescription>(file.into_any())
            .expect("AF_UNIX rights contained a non-OFD capability");
        let Ok(Some(descriptor)) = task.fd_reserve_received(file, cloexec) else {
            break;
        };
        if task
            .copy_to_user(
                pointer + CMSG_HEADER + installed * 4,
                &(descriptor as i32).to_ne_bytes(),
            )
            .is_err()
        {
            drop(task.fd_cancel_received(descriptor));
            break;
        }
        task.fd_publish_received(descriptor);
        installed += 1;
    }
    if installed == 0 {
        return (0, true);
    }
    let cmsg_length = CMSG_HEADER + installed * 4;
    let space = align(cmsg_length)
        .expect("bounded SCM_RIGHTS length overflowed")
        .min(remaining);
    let mut header = [0u8; CMSG_HEADER];
    header[..8].copy_from_slice(&cmsg_length.to_ne_bytes());
    header[8..12].copy_from_slice(&SOL_SOCKET.to_ne_bytes());
    header[12..16].copy_from_slice(&SCM_RIGHTS.to_ne_bytes());
    if task.copy_to_user(pointer, &header).is_err() {
        return (0, true);
    }
    (space, installed < total)
}

/// @description recvmsg ancillary ABI 的用户输出目标。
pub(super) struct ReceiveTarget<'a> {
    /// 当前 fd-table 与 user-copy owner。
    pub(super) task: &'a TaskControlBlock,
    /// raw msghdr pointer。
    pub(super) message: usize,
    /// raw name buffer `(pointer, capacity)`。
    pub(super) name: (usize, usize),
    /// raw control buffer `(pointer, capacity)`。
    pub(super) control: (usize, usize),
}

/// @description socket backend 已提交、等待编码到 recvmsg 的 ancillary 内容。
pub(super) struct ReceiveContent {
    /// backend source address。
    pub(super) source: Option<SocketAddress>,
    /// IP_PKTINFO 使用的本地 IPv4 address。
    pub(super) local: Option<core::net::Ipv4Addr>,
    /// 当前 socket 是否请求 IP_PKTINFO。
    pub(super) packet_info: bool,
    /// AF_UNIX transport 转交的 descriptor capabilities。
    pub(super) rights: Option<UnixRights>,
    /// 新发布 descriptor 是否设置 FD_CLOEXEC。
    pub(super) cloexec: bool,
    /// payload 是否超过用户 byte capacity。
    pub(super) truncated: bool,
}

/// @description 编码 recvmsg name/control/flags，并把收到的 rights 发布进 fd table。
/// @param target caller、msghdr 与 name/control user buffers。
/// @param content backend source、packet info、rights 与 output flags。
/// @return metadata 与 fd publication 成功。
/// @errors copyout、fd limit 或 OOM 返回标准 errno。
pub(super) fn write_receive(
    target: ReceiveTarget<'_>,
    content: ReceiveContent,
) -> Result<(), isize> {
    let ReceiveTarget {
        task,
        message,
        name: (name, name_length),
        control: (control, control_length),
    } = target;
    let ReceiveContent {
        source,
        local,
        packet_info,
        rights,
        cloexec,
        truncated,
    } = content;
    let mut output_flags = if truncated {
        super::MSG_TRUNC as i32
    } else {
        0
    };
    if name != 0 {
        let (encoded, actual) = encode_address(source);
        task.copy_to_user(name, &encoded[..actual.min(name_length)])
            .map_err(|_| -errno::EFAULT)?;
        task.copy_to_user(message + 8, &(actual as u32).to_ne_bytes())
            .map_err(|_| -errno::EFAULT)?;
    }
    let mut written = 0usize;
    if let Some(rights) = rights {
        if control == 0 {
            drop(rights);
            output_flags |= MSG_CTRUNC;
        } else {
            let (count, truncated) = write_rights(task, control, control_length, rights, cloexec);
            written += count;
            if truncated {
                output_flags |= MSG_CTRUNC;
            }
        }
    }
    if packet_info && let Some(local) = local {
        if control != 0 && control_length.saturating_sub(written) >= IP_PKTINFO_SPACE {
            let mut packet_info = [0u8; IP_PKTINFO_SPACE];
            packet_info[..8].copy_from_slice(&IP_PKTINFO_LENGTH.to_ne_bytes());
            packet_info[8..12].copy_from_slice(&IPPROTO_IP.to_ne_bytes());
            packet_info[12..16].copy_from_slice(&IP_PKTINFO.to_ne_bytes());
            packet_info[16..20].copy_from_slice(&INTERFACE_INDEX.to_ne_bytes());
            packet_info[20..24].copy_from_slice(&local.octets());
            packet_info[24..28].copy_from_slice(&local.octets());
            task.copy_to_user(control + written, &packet_info)
                .map_err(|_| -errno::EFAULT)?;
            written += IP_PKTINFO_SPACE;
        } else {
            output_flags |= MSG_CTRUNC;
        }
    }
    task.copy_to_user(message + 40, &written.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)?;
    task.copy_to_user(message + 48, &output_flags.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)
}
