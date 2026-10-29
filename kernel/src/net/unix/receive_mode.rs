/// 区分 VFS read 与 native recvmsg backend 调用；recvfrom 也是 Message，但不接收 rights 输出。
#[derive(Clone, Copy)]
pub(in crate::net) enum ReceiveMode {
    Read { nonblocking: bool },
    Message { rights: bool, nonblocking: bool },
}
impl ReceiveMode {
    pub(in crate::net) fn nonblocking(self) -> bool {
        match self {
            Self::Read { nonblocking } | Self::Message { nonblocking, .. } => nonblocking,
        }
    }
    pub(in crate::net) fn is_message(self) -> bool {
        matches!(self, Self::Message { .. })
    }
    pub(in crate::net) fn receives_rights(self) -> bool {
        matches!(self, Self::Message { rights: true, .. })
    }
}
