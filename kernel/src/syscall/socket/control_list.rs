//! Linux native cmsghdr 的一次 copyin 与只读遍历；解析期间不再次读取 userspace header/data。
use crate::{execution::task::TaskControlBlock, syscall::user_iovec::UserInputStaging};

pub(super) const CMSG_HEADER: usize = 16;
const INLINE_CONTROL: usize = CMSG_HEADER + 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImportError {
    NoBuffer,
    CopyFault,
}
enum Storage {
    Inline {
        bytes: [u8; INLINE_CONTROL],
        length: usize,
    },
    Heap {
        bytes: UserInputStaging<'static>,
        // 字段 drop 顺序保证先释放 allocation，再退实际 socket 费用。
        _charge: crate::net::ControlMemoryCharge,
    },
}
/// 已完整 copyin 的唯一 ancillary bytes；只把 initialized prefix 暴露给 codec。
pub(super) struct ControlSnapshot(Storage);
impl ControlSnapshot {
    /// Linux native sendmsg 的 INT_MAX 上界与 36-byte stack fast path；大区块不预清零。
    /// 任一 fault 不返回半份 snapshot，OOM/超界返回 NoBuffer，不捕获任何 descriptor。
    pub(super) fn import(
        task: &TaskControlBlock,
        socket: &crate::net::Socket,
        pointer: usize,
        length: usize,
    ) -> Result<Self, ImportError> {
        if length > i32::MAX as usize {
            return Err(ImportError::NoBuffer);
        }
        if length <= INLINE_CONTROL {
            let mut bytes = [0; INLINE_CONTROL];
            if length != 0 {
                task.copy_from_user(pointer, &mut bytes[..length])
                    .map_err(|_| ImportError::CopyFault)?;
            }
            Ok(Self(Storage::Inline { bytes, length }))
        } else {
            let charge = socket
                .reserve_control_memory(length)
                .ok_or(ImportError::NoBuffer)?;
            let mut bytes = UserInputStaging::try_new(length).map_err(|_| ImportError::NoBuffer)?;
            task.copy_from_user_uninit(pointer, bytes.prepare(length))
                .map_err(|_| ImportError::CopyFault)?;
            // SAFETY: 实际 copy_from_user_uninit 只有完整初始化全部 destination 后才成功；
            // 任一失败在 publish 前退出，codec 永远看不到未初始化或 fault 后的半份 control。
            unsafe { bytes.publish(length) };
            Ok(Self(Storage::Heap {
                bytes,
                _charge: charge,
            }))
        }
    }
    pub(super) fn bytes(&self) -> &[u8] {
        match &self.0 {
            Storage::Inline { bytes, length } => &bytes[..*length],
            Storage::Heap { bytes, .. } => bytes.initialized(),
        }
    }
}

/// 一份 cmsghdr 的 decoded fields 与借用 payload，不拥有 socket policy 或 descriptor state。
pub(super) struct ControlMessage<'a> {
    pub(super) level: i32,
    pub(super) kind: i32,
    pub(super) data: &'a [u8],
}
/// 对 frozen native control snapshot 按 CMSG_FIRSTHDR/CMSG_NXTHDR 单路遍历。
pub(super) struct ControlList<'a> {
    bytes: &'a [u8],
}
impl<'a> ControlList<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
}
impl<'a> Iterator for ControlList<'a> {
    type Item = Result<ControlMessage<'a>, ()>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.bytes.len() < CMSG_HEADER {
            return None;
        }
        let length = usize::from_ne_bytes(self.bytes[..8].try_into().unwrap());
        if !(CMSG_HEADER..=self.bytes.len()).contains(&length) {
            self.bytes = &[];
            return Some(Err(()));
        }
        let message = ControlMessage {
            level: i32::from_ne_bytes(self.bytes[8..12].try_into().unwrap()),
            kind: i32::from_ne_bytes(self.bytes[12..16].try_into().unwrap()),
            data: &self.bytes[CMSG_HEADER..length],
        };
        // CMSG_NXTHDR 不要求尾部 padding 完整；不足下一 header 的 tail 直接忽略。
        let next = length
            .checked_add(7)
            .map(|next| next & !7)
            .unwrap_or(self.bytes.len());
        self.bytes = &self.bytes[next.min(self.bytes.len())..];
        Some(Ok(message))
    }
}
