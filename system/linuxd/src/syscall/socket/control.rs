use alloc::{sync::Arc, vec::Vec};

use super::{SocketAddress, encode_address, errno, socket_error};
use crate::{
    execution::task::TaskControlBlock,
    file::OpenFileDescription,
    net::{Socket, SocketDomain, UnixPassedFile, UnixRights},
};

use super::control_list::{CMSG_HEADER, ControlList, ControlSnapshot};
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
const IPPROTO_IP: i32 = 0;
const IP_PKTINFO: i32 = 8;

pub(super) const MSG_CTRUNC: i32 = 0x8;
pub(super) const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

fn align(length: usize) -> Option<usize> {
    length.checked_add(7).map(|value| value & !7)
}

/// 解析 sendmsg control list，并在一次 fd-table snapshot 中捕获 SCM_RIGHTS。
///
/// # Parameters
///
/// - `task`: caller 与 fd-table owner。
/// - `socket`: 目标 socket façade，用于 domain policy。
/// - `snapshot`: ABI 已完整 copyin 的唯一 native control bytes；domain preflight 先于语义解析。
///
/// # Returns
///
/// 无 rights 或一条合并且保持用户顺序的 rights 集合。
///
/// # Errors
///
/// malformed/unsupported cmsg、无效 fd、地址 policy 或 OOM 返回标准 errno。
pub(super) struct SendAncillary {
    pub(super) rights: Option<UnixRights>,
    pub(super) credentials: crate::net::UnixSendCredentials,
    pub(super) packet_info: Option<crate::net::InternetSendPacketInfo>,
}

pub(super) fn parse_send(
    task: &TaskControlBlock,
    socket: &Arc<Socket>,
    snapshot: &ControlSnapshot,
) -> Result<SendAncillary, isize> {
    let mut credentials = super::credentials::sender(task);
    let mut descriptors = Vec::new();
    let mut packet_info: Option<crate::net::InternetSendPacketInfo> = None;
    for message in ControlList::new(snapshot.bytes()) {
        let message = message.map_err(|_| -errno::EINVAL)?;
        match (message.level, message.kind) {
            (SOL_SOCKET, SCM_CREDENTIALS) => {
                credentials.claimed = Some(super::credentials::prepare(task, message.data)?);
            }
            (SOL_SOCKET, SCM_RIGHTS) => {
                if socket.domain() != SocketDomain::Unix {
                    return Err(-errno::EINVAL);
                }
                // Linux scm_fp_copy 按完整 int 个数截断：空/不足 int 的 payload 不产生 rights。
                let count = message.data.len() / core::mem::size_of::<i32>();
                if descriptors.len().saturating_add(count) > crate::net::SCM_MAX_FD {
                    return Err(-errno::EINVAL);
                }
                descriptors.try_reserve(count).map_err(|_| -errno::ENOMEM)?;
                for encoded in message.data.as_chunks::<4>().0 {
                    let fd = i32::from_ne_bytes(*encoded);
                    if fd < 0 {
                        return Err(-errno::EBADF);
                    }
                    descriptors.push(fd as usize);
                }
            }
            (IPPROTO_IP, IP_PKTINFO) if socket.domain() == SocketDomain::Inet => {
                if message.data.len() != 12 {
                    return Err(-errno::EINVAL);
                }
                let index = i32::from_ne_bytes(message.data[..4].try_into().unwrap()) as u32;
                let source =
                    core::net::Ipv4Addr::from(<[u8; 4]>::try_from(&message.data[4..8]).unwrap());
                let info = packet_info.get_or_insert_with(Default::default);
                if index != 0 {
                    info.interface_index = Some(index);
                }
                info.source_address = (!source.is_unspecified()).then_some(source);
            }
            // AF_UNIX 的 scm parser 忽略非 SOL_SOCKET level；SOL_SOCKET 未知 type 为 EINVAL。
            (level, _) if socket.domain() == SocketDomain::Unix && level != SOL_SOCKET => {}
            (SOL_SOCKET, _) => return Err(-errno::EINVAL),
            _ => return Err(-errno::EOPNOTSUPP),
        }
    }
    if descriptors.is_empty() {
        return Ok(SendAncillary {
            rights: None,
            credentials,
            packet_info,
        });
    }
    let files = task
        .fd_capture_many(&descriptors)
        .map_err(crate::syscall::file_descriptor_error)?;
    let mut passed: Vec<Arc<dyn UnixPassedFile>> = Vec::new();
    passed
        .try_reserve_exact(files.len())
        .map_err(|_| -errno::ENOMEM)?;
    passed.extend(
        files
            .into_iter()
            .map(|file| -> Arc<dyn UnixPassedFile> { file }),
    );
    let authority = task.namespace_authority();
    let initial = crate::namespace::initial_user();
    let exempt = authority.capable(&initial, syscall_abi::capability::Capability::SYS_RESOURCE)
        || authority.capable(&initial, syscall_abi::capability::Capability::SYS_ADMIN);
    UnixRights::new(
        passed,
        task.credential_id(true, false),
        (!exempt).then(|| task.unix_file_limit()),
    )
    .map(|rights| SendAncillary {
        rights: Some(rights),
        credentials,
        packet_info,
    })
    .map_err(socket_error)
}

fn rights_capacity(remaining: usize, count: usize) -> usize {
    count.min(remaining.saturating_sub(CMSG_HEADER) / 4)
}

/// 原生 scm_detach_fds：每个编号 copyout 后立即发布，随后失败只截断尚未接收的尾部。
/// header fault 不撤销已安装的 prefix，也不推进 control cursor。
fn write_rights(
    task: &TaskControlBlock,
    pointer: usize,
    remaining: usize,
    rights: crate::net::UnixReceivedRights,
    cloexec: bool,
) -> (usize, bool) {
    let total = rights.len();
    let fit = rights_capacity(remaining, total);
    let mut installed = 0usize;
    for file in rights.into_files().into_iter().take(fit) {
        let file = Arc::downcast::<OpenFileDescription>(file.into_any())
            .expect("AF_UNIX rights contained a non-OFD capability");
        let reservation = match task.fd_prepare_open(cloexec) {
            Ok(reservation) => reservation,
            Err(_) => break,
        };
        let descriptor = reservation.descriptors()[0];
        if task
            .copy_to_user(
                pointer.wrapping_add(CMSG_HEADER + installed * 4),
                &(descriptor as i32).to_ne_bytes(),
            )
            .is_err()
        {
            break;
        }
        reservation.publish(file);
        installed += 1;
    }
    let truncated = installed < total;
    if installed == 0 {
        return (0, truncated);
    }
    let length = CMSG_HEADER + installed * 4;
    // native 顺序为 level → type → len；这些写入失败不会返回 recvmsg EFAULT。
    let header = task
        .copy_to_user(pointer.wrapping_add(8), &SOL_SOCKET.to_ne_bytes())
        .and_then(|()| task.copy_to_user(pointer.wrapping_add(12), &SCM_RIGHTS.to_ne_bytes()))
        .and_then(|()| task.copy_to_user(pointer, &length.to_ne_bytes()));
    let consumed = if header.is_ok() {
        align(length)
            .expect("bounded SCM_RIGHTS length")
            .min(remaining)
    } else {
        0
    };
    (consumed, truncated)
}

/// 原生 put_cmsg 的 cursor 规则：短 buffer 标记 CTRUNC，copy fault 保留 cursor。
/// credentials 与 IP_PKTINFO caller 按 Linux 忽略该 control copy fault。
fn write_control(
    task: &TaskControlBlock,
    pointer: usize,
    remaining: usize,
    level: i32,
    kind: i32,
    data: &[u8],
    output_flags: &mut i32,
) -> usize {
    if pointer == 0 || remaining < CMSG_HEADER {
        *output_flags |= MSG_CTRUNC;
        return 0;
    }
    let full_length = CMSG_HEADER + data.len();
    let length = full_length.min(remaining);
    if length < full_length {
        *output_flags |= MSG_CTRUNC;
    }
    let copied = task
        .copy_to_user(pointer, &length.to_ne_bytes())
        .and_then(|()| task.copy_to_user(pointer.wrapping_add(8), &level.to_ne_bytes()))
        .and_then(|()| task.copy_to_user(pointer.wrapping_add(12), &kind.to_ne_bytes()))
        .and_then(|()| {
            task.copy_to_user(
                pointer.wrapping_add(CMSG_HEADER),
                &data[..length - CMSG_HEADER],
            )
        });
    if copied.is_ok() {
        align(full_length)
            .expect("bounded native cmsg")
            .min(remaining)
    } else {
        0
    }
}

/// recvmsg ancillary ABI 的用户输出目标。
pub(super) struct ReceiveTarget<'a> {
    /// 当前 fd-table 与 user-copy owner。
    pub(super) task: &'a TaskControlBlock,
    /// raw msghdr pointer。
    pub(super) message: usize,
    /// raw name buffer；native move_addr_to_user 在 controls 后重新读取 namelen。
    pub(super) name: usize,
    /// raw control buffer `(pointer, capacity)`。
    pub(super) control: (usize, usize),
}

/// socket backend 已提交、等待编码到 recvmsg 的 ancillary 内容。
pub(super) struct ReceiveContent {
    /// backend source address。
    pub(super) source: Option<SocketAddress>,
    /// actual ingress冻结的IP_PKTINFO；不查询当前caller或已变更接口。
    pub(super) internet_packet_info: Option<crate::net::InternetPacketInfo>,
    /// 当前 socket 是否请求 IP_PKTINFO。
    pub(super) packet_info: bool,
    /// AF_UNIX transport 转交的 descriptor capabilities。
    pub(super) rights: Option<crate::net::UnixReceivedRights>,
    /// transport 保存的实际消息凭据；None 与 peer credentials 无关。
    pub(super) credentials: Option<crate::net::UnixCredentials>,
    /// SO_PASSCRED 请求输出，未保存凭据时编码 overflow IDs。
    pub(super) pass_credentials: bool,
    /// 新发布 descriptor 是否设置 FD_CLOEXEC。
    pub(super) cloexec: bool,
    /// payload 是否超过用户 byte capacity。
    pub(super) truncated: bool,
}

/// 编码 recvmsg name/control/flags，并把收到的 rights 发布进 fd table。
///
/// # Parameters
///
/// - `target`: caller、msghdr 与 name/control user buffers。
/// - `content`: backend source、packet info、rights 与 output flags。
///
/// # Returns
///
/// metadata 与 fd publication 成功。
///
/// # Errors
///
/// name/msghdr copyout fault 返回 EFAULT；FD admission/copyout 失败通过 MSG_CTRUNC 截断。
pub(super) fn write_receive(
    target: ReceiveTarget<'_>,
    content: ReceiveContent,
) -> Result<(), isize> {
    let ReceiveTarget {
        task,
        message,
        name,
        control: (control, control_length),
    } = target;
    let ReceiveContent {
        source,
        internet_packet_info,
        packet_info,
        rights,
        credentials,
        pass_credentials,
        cloexec,
        truncated,
    } = content;
    let mut output_flags = if truncated {
        super::MSG_TRUNC as i32
    } else {
        0
    };
    if cloexec {
        output_flags |= MSG_CMSG_CLOEXEC as i32;
    }
    // 1. transport 已消费 payload；controls 在 name 与 msghdr metadata 之前输出。
    let mut written = 0usize;
    if pass_credentials {
        written += write_control(
            task,
            control,
            control_length,
            SOL_SOCKET,
            SCM_CREDENTIALS,
            &super::credentials::encode(task, credentials.as_ref()),
            &mut output_flags,
        );
    }
    if let Some(rights) = rights {
        if control == 0 {
            drop(rights);
            output_flags |= MSG_CTRUNC;
        } else {
            let (consumed, rights_truncated) = write_rights(
                task,
                control.wrapping_add(written),
                control_length.saturating_sub(written),
                rights,
                cloexec,
            );
            written += consumed;
            if rights_truncated {
                output_flags |= MSG_CTRUNC;
            }
        }
    }
    if packet_info && let Some(info) = internet_packet_info {
        let mut data = [0u8; 12];
        data[..4].copy_from_slice(&(info.interface_index as i32).to_ne_bytes());
        data[4..8].copy_from_slice(&info.specific_address.octets());
        data[8..].copy_from_slice(&info.destination.octets());
        written += write_control(
            task,
            if control == 0 {
                0
            } else {
                control.wrapping_add(written)
            },
            control_length.saturating_sub(written),
            IPPROTO_IP,
            IP_PKTINFO,
            &data,
            &mut output_flags,
        );
    }
    // 2. native recvmsg：name → flags → controllen；fault 保留已安装的全部 FD。
    if name != 0 {
        let (encoded, actual) = encode_address(source);
        let mut capacity = [0u8; 4];
        task.copy_from_user(message.wrapping_add(8), &mut capacity)
            .map_err(|_| -errno::EFAULT)?;
        let capacity = i32::from_ne_bytes(capacity);
        if capacity < 0 {
            return Err(-errno::EINVAL);
        }
        task.copy_to_user(message.wrapping_add(8), &(actual as u32).to_ne_bytes())
            .map_err(|_| -errno::EFAULT)?;
        task.copy_to_user(name, &encoded[..actual.min(capacity as usize)])
            .map_err(|_| -errno::EFAULT)?;
    }
    task.copy_to_user(message.wrapping_add(48), &output_flags.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)?;
    task.copy_to_user(message.wrapping_add(40), &written.to_ne_bytes())
        .map_err(|_| -errno::EFAULT)
}
