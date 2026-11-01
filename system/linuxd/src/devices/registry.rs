use alloc::{sync::Arc, vec::Vec};
use spin::Mutex;

/// 一个 seam 的全部已发布 adapter，按注册顺序编号，只追加；index 是 adapter 的稳定 identity。
pub(crate) struct AppendOnlyRegistry<T: ?Sized> {
    devices: Mutex<Vec<Arc<T>>>,
}

impl<T: ?Sized> AppendOnlyRegistry<T> {
    pub(crate) const fn new() -> Self {
        Self {
            devices: Mutex::new(Vec::new()),
        }
    }

    /// 追加一个 adapter，返回其稳定 index。
    ///
    /// # Errors
    ///
    /// 扩容失败时原样返回 adapter。
    pub(crate) fn register(&self, device: Arc<T>) -> Result<usize, Arc<T>> {
        let mut devices = self.devices.lock();
        if devices.try_reserve(1).is_err() {
            return Err(device);
        }
        devices.push(device);
        Ok(devices.len() - 1)
    }

    /// 第 `index` 个 adapter。
    pub(crate) fn get(&self, index: usize) -> Option<Arc<T>> {
        self.devices.lock().get(index).cloned()
    }

    /// 已发布数量。
    pub(crate) fn count(&self) -> usize {
        self.devices.lock().len()
    }
}
