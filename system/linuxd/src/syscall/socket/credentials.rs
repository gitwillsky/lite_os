//! Linux ucred 输入验证；PID 只经实际 namespace association 取得 observer，不选择 live TCB。
use super::errno;
use crate::{
    execution::task::{MessageIdentityError, TaskControlBlock},
    net::UnixCredentials,
};

pub(super) fn prepare(task: &TaskControlBlock, bytes: &[u8]) -> Result<UnixCredentials, isize> {
    if bytes.len() != 12 {
        return Err(-errno::EINVAL);
    }
    let requested_pid = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
    let requested_uid = u32::from_ne_bytes(bytes[4..8].try_into().unwrap());
    let requested_gid = u32::from_ne_bytes(bytes[8..12].try_into().unwrap());
    let namespace = task.pid_namespace();
    let ids = task
        .message_identity()
        .validate_claim(
            requested_uid,
            requested_gid,
            i64::from(requested_pid) == i64::from(task.tgid_identity().number_in(&namespace)),
            &namespace.user_namespace(),
        )
        .map_err(|error| match error {
            MessageIdentityError::Invalid => -errno::EINVAL,
            MessageIdentityError::Permission => -errno::EPERM,
        })?;
    let pid = u32::try_from(requested_pid)
        .ok()
        .and_then(|pid| namespace.lookup_identity(pid))
        .ok_or(-errno::ESRCH)?;
    Ok(UnixCredentials {
        pid,
        uid: ids[0],
        gid: ids[1],
    })
}

/// 每次发送调用冻结 TGID 和 real UID/GID；peer credentials 仍独立使用 effective IDs。
pub(in crate::syscall) fn sender(task: &TaskControlBlock) -> crate::net::UnixSendCredentials {
    let ids = task.message_identity().ids(false);
    crate::net::UnixSendCredentials {
        sender: UnixCredentials {
            pid: task.tgid_identity().clone(),
            uid: ids[0],
            gid: ids[1],
        },
        claimed: None,
    }
}

/// 将 canonical IDs 和历史 PID observer 投影到实际接收者的 namespaces；未保存凭据输出 native overflow IDs。
pub(super) fn encode(task: &TaskControlBlock, credentials: Option<&UnixCredentials>) -> [u8; 12] {
    let view = task.pid_namespace();
    let namespace = task.user_namespace();
    let (pid, uid, gid) = credentials.map_or((0, u32::MAX, u32::MAX), |credentials| {
        (
            credentials.pid.number_in(&view),
            credentials.uid,
            credentials.gid,
        )
    });
    let mut bytes = [0u8; 12];
    bytes[..4].copy_from_slice(&(pid as i32).to_ne_bytes());
    bytes[4..8].copy_from_slice(&namespace.display_id(true, uid).to_ne_bytes());
    bytes[8..].copy_from_slice(&namespace.display_id(false, gid).to_ne_bytes());
    bytes
}
