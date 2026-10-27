//! ring fd 的 `DeviceFile` backend：poll/epoll、mmap 与匿名 inode 标签。

use alloc::sync::Arc;
use syscall_abi::errno;

use super::ring::Ring;
use crate::{
    fs::chardev::{DeviceError, DeviceFile, DeviceWaitSources, MapRequest},
    ipc::Pipe,
    memory::{DeviceMappingSource, DevicePageAccounting, PAGE_SIZE},
};

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;

impl DeviceFile for Ring {
    fn supports_read(&self) -> bool {
        false
    }

    fn supports_write(&self) -> bool {
        false
    }

    /// 最后一个 OFD 引用释放（含进程退出关闭 fd）：取消该 ring 的全部挂起请求。
    fn release(&self) {
        self.close();
    }

    fn anonymous_name(&self) -> &'static [u8] {
        super::ANONYMOUS_NAME
    }

    /// Linux `io_uring_poll`：CQ 有完成时可读，SQ 有空位时可写。
    fn poll(&self, events: i16) -> i16 {
        let mut ready = 0;
        if self.has_completions() {
            ready |= POLLIN;
        }
        if self.sq_has_space() {
            ready |= POLLOUT;
        }
        events & ready
    }

    fn wait_sources(&self, events: i16) -> DeviceWaitSources {
        let wanted = events & (POLLIN | POLLOUT);
        if wanted == 0 {
            return DeviceWaitSources::new();
        }
        DeviceWaitSources::pipe(self.readiness_read().pipe(), wanted)
    }

    fn readiness_generation(&self) -> u64 {
        Ring::readiness_generation(self)
    }

    /// 阻塞前排空已消费的合并 edge 再复查，避免排空与新 CQE 之间丢唤醒。
    fn prepare_wait(&self, events: i16) -> Option<Arc<Pipe>> {
        if DeviceFile::poll(self, events) != 0 {
            return None;
        }
        self.readiness_read().drain_readiness();
        (DeviceFile::poll(self, events) == 0).then(|| self.readiness_read().pipe())
    }

    /// 只接受三个魔数偏移（Linux `io_uring_validate_mmap_request`）；必须 `MAP_SHARED`、可写、
    /// 不可执行，长度不得超过对应区域按页取整后的大小。
    fn mmap(
        &self,
        offset: u64,
        length: usize,
        request: MapRequest,
    ) -> Result<DeviceMappingSource, DeviceError> {
        if !request.shared || request.executable || !request.writable {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        let (backing, identity) = self
            .region(offset)
            .ok_or(DeviceError::Errno(errno::EINVAL))?;
        if length == 0 || length > backing.pages() * PAGE_SIZE {
            return Err(DeviceError::Errno(errno::EINVAL));
        }
        // Linux io_region_mmap 使用 vm_insert_pages，ring 页计入 RSS。
        Ok(DeviceMappingSource::new(
            identity,
            backing,
            DevicePageAccounting::InsertedPages,
        ))
    }
}
