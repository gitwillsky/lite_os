//! Linux BACKING_OPEN/CLOSE：登记事实与请求共用 Connection.queue；reopen/drop 在锁外。
use super::*;
use crate::fs::{BackingOpen, chardev::IoctlCall};

/// 一次成功的 passthrough 打开：登记 identity 与独立下层 OFD 成对拥有。
pub(in crate::fs::fuse) struct OpenedBacking {
    pub(in crate::fs::fuse) registration: Arc<dyn crate::fs::BackingFile>,
    pub(in crate::fs::fuse) opened: Arc<dyn BackingOpen>,
}

impl Connection {
    /// 处理标准 BACKING_OPEN/CLOSE；调用者必须有 initial CAP_SYS_ADMIN，且已协商 PASSTHROUGH。
    /// 参数从 user-copy seam 读取；返回 backing_id 或零，失败返回标准 errno。
    pub(in crate::fs::fuse) fn backing_ioctl(
        &self,
        call: &IoctlCall<'_>,
    ) -> Result<isize, DeviceError> {
        let negotiated = self
            .negotiated()
            .filter(|options| options.has(fuse::PASSTHROUGH));
        if !call.privileged || negotiated.is_none() {
            return Err(DeviceError::Errno(errno::EPERM));
        }
        let negotiated = negotiated.unwrap();
        if call.request == fuse::DEV_IOC_BACKING_OPEN {
            let mut bytes = [0; 16];
            call.user.read(call.argument, &mut bytes).map_err(fault)?;
            let fd = i32::from_ne_bytes(bytes[..4].try_into().unwrap());
            if bytes[4..].iter().any(|byte| *byte != 0) {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            let backing = call.user.backing_file(fd)?;
            if backing.stack_depth() >= negotiated.max_stack_depth {
                return Err(DeviceError::Errno(errno::ELOOP));
            }
            let mut queue = self.queue.lock();
            queue
                .backings
                .try_reserve(1)
                .map_err(|_| DeviceError::Errno(errno::ENOMEM))?;
            let id = (1..=i32::MAX)
                .find(|id| !queue.backings.iter().any(|(old, _)| old == id))
                .ok_or(DeviceError::Errno(errno::ENOSPC))?;
            queue.backings.push((id, backing));
            Ok(id as isize)
        } else {
            let mut bytes = [0; 4];
            call.user.read(call.argument, &mut bytes).map_err(fault)?;
            let id = i32::from_ne_bytes(bytes);
            if id <= 0 {
                return Err(DeviceError::Errno(errno::EINVAL));
            }
            let retired = {
                let mut queue = self.queue.lock();
                let index = queue
                    .backings
                    .iter()
                    .position(|(old, _)| *old == id)
                    .ok_or(DeviceError::Errno(errno::ENOENT))?;
                queue.backings.swap_remove(index)
            };
            drop(retired);
            Ok(0)
        }
    }

    /// 用 OPEN/CREATE 回复的 backing_id 建立本次打开；登记在锁内 clone，reopen 不持连接锁。
    /// 未协商/非正 ID 为 EINVAL，不存在为 ENOENT；权限、只读与后端错误原样传播。
    pub(in crate::fs::fuse) fn open_backing(
        &self,
        id: i32,
        flags: u32,
    ) -> Result<OpenedBacking, FileSystemError> {
        if id <= 0
            || !self
                .negotiated()
                .is_some_and(|options| options.has(fuse::PASSTHROUGH))
        {
            return Err(FileSystemError::InvalidOperation);
        }
        let backing = self
            .queue
            .lock()
            .backings
            .iter()
            .find(|(old, _)| *old == id)
            .map(|(_, backing)| backing.clone())
            .ok_or(FileSystemError::NotFound)?;
        let opened = backing.reopen(flags)?;
        Ok(OpenedBacking {
            registration: backing,
            opened,
        })
    }
}
