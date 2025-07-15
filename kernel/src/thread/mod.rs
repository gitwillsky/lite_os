use core::sync::atomic::{AtomicUsize, Ordering};
use crate::sync::UPSafeCell;
use lazy_static::lazy_static;

pub mod thread;
pub mod thread_manager;
pub mod sync;
pub mod signal;

pub use thread::{ThreadControlBlock, ThreadId, ThreadStatus, ThreadStack};
pub use thread_manager::{ThreadManager, ThreadStackAllocator, create_thread, exit_thread, join_thread, yield_current_thread};
pub use sync::{Mutex, Condvar, RwLock, Semaphore, SyncObjectManager, SyncObjectId};
pub use signal::{ThreadSignalState, ThreadSignalDelivery, send_signal_to_thread, check_and_handle_thread_signals, inherit_signal_state_for_thread, cleanup_thread_signals};

/// 全局线程ID计数器
static THREAD_ID_COUNTER: AtomicUsize = AtomicUsize::new(1);

/// 分配新的线程ID
pub fn alloc_thread_id() -> ThreadId {
    ThreadId(THREAD_ID_COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// 全局同步对象管理器
lazy_static! {
    static ref GLOBAL_SYNC_MANAGER: UPSafeCell<SyncObjectManager> = UPSafeCell::new(SyncObjectManager::new());
}

/// 初始化多线程系统
pub fn init_threading() {
    info!("Initializing multi-threading support...");
    
    // 初始化全局同步对象管理器
    let _sync_manager = GLOBAL_SYNC_MANAGER.exclusive_access();
    
    info!("Multi-threading support initialized successfully");
}

/// 获取全局同步对象管理器
pub fn get_sync_manager() -> core::cell::RefMut<'static, SyncObjectManager> {
    GLOBAL_SYNC_MANAGER.exclusive_access()
}