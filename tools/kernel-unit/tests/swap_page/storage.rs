use super::*;
use std::sync::{Mutex, atomic::AtomicBool};

#[derive(Debug, Clone, Copy)]
pub(crate) enum SwapError {
    OutOfMemory,
    Io,
}

pub(crate) struct Storage {
    pub(crate) identity: memory::SharedFileId,
    pub(crate) reserved: AtomicUsize,
    pub(crate) live_slots: AtomicUsize,
    pub(crate) active: AtomicBool,
    pub(crate) fail_write: AtomicBool,
    pub(crate) fail_read: AtomicBool,
}

thread_local! { static STORAGE: RefCell<Option<Arc<Storage>>> = const { RefCell::new(None) }; }
static NEXT_STORAGE: AtomicUsize = AtomicUsize::new(1);

pub(crate) fn configure() -> Arc<Storage> {
    let storage = Arc::new(Storage {
        identity: memory::SharedFileId {
            filesystem: NEXT_STORAGE.fetch_add(1, Ordering::Relaxed),
            inode: 1,
        },
        reserved: AtomicUsize::new(0),
        live_slots: AtomicUsize::new(0),
        active: AtomicBool::new(true),
        fail_write: AtomicBool::new(false),
        fail_read: AtomicBool::new(false),
    });
    STORAGE.with(|value| *value.borrow_mut() = Some(storage.clone()));
    storage
}

pub(crate) struct SwapSlot {
    inner: Arc<SlotInner>,
}
impl core::fmt::Debug for SwapSlot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SwapSlot").finish_non_exhaustive()
    }
}

struct SlotInner {
    storage: Arc<Storage>,
    bytes: Mutex<[u8; memory::PAGE_SIZE]>,
}
impl Drop for SlotInner {
    fn drop(&mut self) {
        self.storage.live_slots.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) fn reserve() -> Option<SwapSlot> {
    STORAGE.with(|value| {
        let storage = value.borrow().as_ref().unwrap().clone();
        if !storage.active.load(Ordering::Relaxed) {
            return None;
        }
        storage.reserved.fetch_add(1, Ordering::Relaxed);
        storage.live_slots.fetch_add(1, Ordering::Relaxed);
        Some(SwapSlot {
            inner: Arc::new(SlotInner {
                storage,
                bytes: Mutex::new([0; memory::PAGE_SIZE]),
            }),
        })
    })
}

impl SwapSlot {
    pub(crate) fn identity(&self) -> memory::SharedFileId {
        self.inner.storage.identity
    }
    pub(crate) fn active(&self) -> bool {
        self.inner.storage.active.load(Ordering::Relaxed)
    }
    pub(crate) fn read(&self, output: &mut [u8; memory::PAGE_SIZE]) -> Result<(), SwapError> {
        let hook = READ_HOOK.with(|hook| hook.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
        if self.inner.storage.fail_read.load(Ordering::Relaxed) {
            return Err(SwapError::Io);
        }
        output.copy_from_slice(&*self.inner.bytes.lock().unwrap());
        Ok(())
    }
    pub(crate) fn write(&self, input: &[u8; memory::PAGE_SIZE]) -> Result<(), SwapError> {
        WRITE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().take() {
                hook();
            }
        });
        if self.inner.storage.fail_write.load(Ordering::Relaxed) {
            return Err(SwapError::Io);
        }
        self.inner.bytes.lock().unwrap().copy_from_slice(input);
        Ok(())
    }
}

impl Clone for SwapSlot {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}
