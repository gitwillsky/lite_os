use alloc::{sync::Arc, vec::Vec};

use super::{SocketAddress, encode_address, errno, socket_error};
use crate::{
    file::OpenFileDescription,
    net::{Socket, SocketDomain, UnixPassedFile, UnixRights},
    task::TaskControlBlock,
};

use super::control_list::{CMSG_HEADER, ControlList, ControlSnapshot};
const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const SCM_CREDENTIALS: i32 = 2;
const IPPROTO_IP: i32 = 0;
const IP_PKTINFO: i32 = 8;
const IPPROTO_IPV6: i32 = 41;
const IPV6_PKTINFO: i32 = 50;
const IP_RECVERR: i32 = 11;
const IPV6_RECVERR: i32 = 25;
const SOL_PACKET: i32 = 263;
const PACKET_AUXDATA: i32 = 8;

pub(super) const MSG_CTRUNC: i32 = 0x8;
pub(super) const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;

fn align(length: usize) -> Option<usize> {
    length.checked_add(7).map(|value| value & !7)
}

/// 只借已冻结并完整验证 framing 的 cmsg，保留顺序和目的族相关的失败；不分配第二份控制缓冲。
#[derive(Clone)]
pub(super) struct PacketInfos<'a> {
    messages: ControlList<'a>,
    remaining: usize,
    ipv6: bool,
}
impl<'a> PacketInfos<'a> {
    pub(super) fn new(bytes: &'a [u8], remaining: usize, ipv6: bool) -> Self {
        Self {
            messages: ControlList::new(bytes),
            remaining,
            ipv6,
        }
    }
    pub(super) fn empty() -> Self {
        Self::new(&[], 0, false)
    }
}
impl Iterator for PacketInfos<'_> {
    type Item = network_protocol::InternetSendPacketInfo;
    fn next(&mut self) -> Option<Self::Item> {
        use network_protocol::{InetFamily, InternetSendPacketInfo};
        if self.remaining == 0 {
            return None;
        }
        for message in self.messages.by_ref() {
            let message = message.expect("packet info borrows prevalidated frozen cmsg framing");
            let item = match (message.level, message.kind) {
                (IPPROTO_IP, IP_PKTINFO) => {
                    if message.data.len() != 12 {
                        InternetSendPacketInfo::Invalid(InetFamily::Ipv4)
                    } else {
                        let index =
                            u32::from_ne_bytes(message.data[..4].try_into().expect("IPv4 index"));
                        InternetSendPacketInfo::Ipv4 {
                            interface_index: (index != 0).then_some(index),
                            source_address: core::net::Ipv4Addr::from(
                                <[u8; 4]>::try_from(&message.data[4..8]).expect("IPv4 source"),
                            ),
                        }
                    }
                }
                (IPPROTO_IPV6, IPV6_PKTINFO) if self.ipv6 => {
                    if message.data.len() < 20 {
                        InternetSendPacketInfo::Invalid(InetFamily::Ipv6)
                    } else {
                        let index = u32::from_ne_bytes(
                            message.data[16..20].try_into().expect("IPv6 index"),
                        );
                        InternetSendPacketInfo::Ipv6 {
                            interface_index: (index != 0).then_some(index),
                            source_address: core::net::Ipv6Addr::from(
                                <[u8; 16]>::try_from(&message.data[..16]).expect("IPv6 source"),
                            ),
                        }
                    }
                }
                _ => continue,
            };
            self.remaining -= 1;
            return Some(item);
        }
        unreachable!("validated packet-info count must match immutable cmsg snapshot")
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for PacketInfos<'_> {}

pub(super) struct SendAncillary<'a> {
    pub(super) rights: Option<UnixRights>,
    pub(super) credentials: crate::net::UnixSendCredentials,
    pub(super) packet_info: PacketInfos<'a>,
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
pub(super) fn parse_send<'a>(
    task: &TaskControlBlock,
    socket: &Arc<Socket>,
    snapshot: &'a ControlSnapshot,
) -> Result<SendAncillary<'a>, isize> {
    let mut credentials = super::credentials::sender(task);
    let mut descriptors = Vec::new();
    let mut packet_info_count = 0;
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
            (IPPROTO_IP, IP_PKTINFO)
                if matches!(socket.domain(), SocketDomain::Inet | SocketDomain::Inet6) =>
            {
                packet_info_count += 1
            }
            (IPPROTO_IPV6, IPV6_PKTINFO) if socket.domain() == SocketDomain::Inet6 => {
                packet_info_count += 1
            }
            // IPv4 的 ip_cmsg_send 不处理 SOL_IPV6，不能误用其中的地址覆盖本次 IPv4 源。
            (IPPROTO_IPV6, IPV6_PKTINFO) if socket.domain() == SocketDomain::Inet => {}
            // AF_UNIX 的 scm parser 忽略非 SOL_SOCKET level；SOL_SOCKET 未知 type 为 EINVAL。
            (level, _) if socket.domain() == SocketDomain::Unix && level != SOL_SOCKET => {}
            (SOL_SOCKET, _) => return Err(-errno::EINVAL),
            _ => return Err(-errno::EOPNOTSUPP),
        }
    }
    let packet_info = PacketInfos::new(
        snapshot.bytes(),
        packet_info_count,
        socket.domain() == SocketDomain::Inet6,
    );
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
    let exempt = authority.capable(&initial, linux_abi::capability::Capability::SYS_RESOURCE)
        || authority.capable(&initial, linux_abi::capability::Capability::SYS_ADMIN);
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
    /// An error queue record carries sock_extended_err and the reporting address.
    pub(super) internet_error: Option<network_protocol::InternetError>,
    /// actual ingress冻结的IP_PKTINFO；不查询当前caller或已变更接口。
    pub(super) internet_packet_info: Option<crate::net::InternetPacketInfo>,
    /// 实际帧状态及完整长度，仅启用 PACKET_AUXDATA 时提供。
    pub(super) packet_auxiliary: Option<(crate::net::PacketInformation, usize)>,
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
        internet_error,
        internet_packet_info,
        packet_auxiliary,
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
    if let Some(info) = internet_packet_info {
        // 双栈 IPv4 的标准输出先 IPv6 mapped 投影，再 IPv4；短缓冲与 copy fault 沿原 put_cmsg 路径。
        if let Some((index, destination)) = info.ipv6() {
            let mut data = [0; 20];
            data[..16].copy_from_slice(&destination.octets());
            data[16..].copy_from_slice(&index.to_ne_bytes());
            written += write_control(
                task,
                if control == 0 {
                    0
                } else {
                    control.wrapping_add(written)
                },
                control_length.saturating_sub(written),
                IPPROTO_IPV6,
                IPV6_PKTINFO,
                &data,
                &mut output_flags,
            );
        }
        if let Some((index, specific, destination)) = info.ipv4() {
            let mut data = [0; 12];
            data[..4].copy_from_slice(&index.to_ne_bytes());
            data[4..8].copy_from_slice(&specific.octets());
            data[8..].copy_from_slice(&destination.octets());
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
    }
    if let Some((information, full_length)) = packet_auxiliary {
        // Linux tpacket_auxdata：USER | 实际 checksum 状态，未启用 BPF 截短，snaplen 与原长相同。
        // 用户 iovec 的截断不改变 snaplen；VLAN 未剥离，TCI/TPID 和有效位均保持零。
        let mut data = [0u8; 20];
        let status = 1u32
            | match information.checksum {
                crate::net::PacketChecksum::Unverified => 0,
                crate::net::PacketChecksum::Partial => 1 << 3,
                crate::net::PacketChecksum::Verified => 1 << 7,
            };
        data[..4].copy_from_slice(&status.to_ne_bytes());
        data[4..8].copy_from_slice(&(full_length as u32).to_ne_bytes());
        data[8..12].copy_from_slice(&(full_length as u32).to_ne_bytes());
        data[14..16].copy_from_slice(&information.network_offset.to_ne_bytes());
        written += write_control(
            task,
            if control == 0 {
                0
            } else {
                control.wrapping_add(written)
            },
            control_length.saturating_sub(written),
            SOL_PACKET,
            PACKET_AUXDATA,
            &data,
            &mut output_flags,
        );
    }
    if let Some(error) = internet_error {
        use network_protocol::InternetErrorOrigin;
        output_flags |= super::MSG_ERRQUEUE as i32;
        let ipv6 = error.destination.is_ipv6();
        let mut data = [0u8; 44];
        data[..4].copy_from_slice(&((-super::socket_error(error.error)) as u32).to_ne_bytes());
        data[4] = match error.origin {
            InternetErrorOrigin::Local => 1,
            InternetErrorOrigin::Icmpv4 => 2,
            InternetErrorOrigin::Icmpv6 => 3,
        };
        data[5] = error.kind;
        data[6] = error.code;
        data[8..12].copy_from_slice(&error.info.to_ne_bytes());
        if let Some(offender) = error.offender {
            let (address, length) = encode_address(Some(SocketAddress::Inet(offender)));
            data[16..16 + length].copy_from_slice(&address[..length]);
        }
        written += write_control(
            task,
            if control == 0 {
                0
            } else {
                control.wrapping_add(written)
            },
            control_length.saturating_sub(written),
            if ipv6 { IPPROTO_IPV6 } else { IPPROTO_IP },
            if ipv6 { IPV6_RECVERR } else { IP_RECVERR },
            &data[..if ipv6 { 44 } else { 32 }],
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
